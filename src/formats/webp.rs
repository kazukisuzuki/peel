use anyhow::{Context, Result, anyhow, bail};

use super::{Candidate, Change, FormatOutcome};

const VP8X_ICC: u8 = 0x20;
const VP8X_ALPHA: u8 = 0x10;
const VP8X_EXIF: u8 = 0x08;
const VP8X_XMP: u8 = 0x04;
const VP8X_ANIMATION: u8 = 0x02;

pub(super) fn optimize(input: &[u8]) -> Result<FormatOutcome> {
    let parsed = Webp::parse(input)?;
    let metadata_removed = parsed.removal_required;
    let output = parsed.encode()?;

    if output == input {
        return Ok(FormatOutcome::Unchanged);
    }

    let verified = Webp::parse(&output).context("failed to verify normalized WebP")?;
    if verified.removal_required || verified.encode()? != output {
        bail!("normalized WebP is not structurally sanitized");
    }

    let mut changes = vec![Change::ContainerNormalized];
    if metadata_removed {
        changes.push(Change::MetadataRemoved);
    }
    Ok(FormatOutcome::Candidate(Candidate {
        bytes: output,
        changes,
    }))
}

#[derive(Clone, Copy)]
struct Chunk<'a> {
    kind: [u8; 4],
    payload: &'a [u8],
}

struct Frame<'a> {
    header: &'a [u8],
    chunks: Vec<Chunk<'a>>,
}

enum KeptChunk<'a> {
    Vp8x(&'a [u8]),
    Anim(&'a [u8]),
    Alph(&'a [u8]),
    Bitstream(Chunk<'a>),
    Frame(Frame<'a>),
}

impl KeptChunk<'_> {
    fn is_bitstream(&self) -> bool {
        matches!(self, Self::Bitstream(_))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Layout {
    Simple,
    Still,
    Animation,
}

#[derive(Default)]
struct Metadata {
    icc: bool,
    exif: bool,
    xmp: bool,
}

struct Webp<'a> {
    chunks: Vec<KeptChunk<'a>>,
    removal_required: bool,
}

impl<'a> Webp<'a> {
    fn parse(input: &'a [u8]) -> Result<Self> {
        if input.get(..4) != Some(b"RIFF") {
            bail!("invalid WebP RIFF signature");
        }
        let size_bytes = input
            .get(4..8)
            .ok_or_else(|| anyhow!("truncated WebP RIFF header"))?;
        let declared_size = u32::from_le_bytes(size_bytes.try_into().unwrap()) as usize;
        let declared_end = declared_size
            .checked_add(8)
            .ok_or_else(|| anyhow!("WebP RIFF size overflow"))?;
        if declared_end > input.len() {
            bail!("WebP RIFF size exceeds the input length");
        }
        if declared_end < 12 || input.get(8..12) != Some(b"WEBP") {
            bail!("invalid WebP form type");
        }

        let raw_chunks = parse_chunks(input, 12, declared_end)
            .context("invalid top-level WebP chunk structure")?;
        let mut chunks = Vec::new();
        let mut metadata = Metadata::default();
        let mut removal_required = declared_end != input.len();

        for chunk in raw_chunks {
            match &chunk.kind {
                b"VP8X" => {
                    if chunk.payload.len() != 10 {
                        bail!("VP8X chunk must contain 10 bytes");
                    }
                    chunks.push(KeptChunk::Vp8x(chunk.payload));
                }
                b"ANIM" => {
                    if chunk.payload.len() != 6 {
                        bail!("ANIM chunk must contain 6 bytes");
                    }
                    chunks.push(KeptChunk::Anim(chunk.payload));
                }
                b"ANMF" => {
                    let (frame, removed) = Frame::parse(chunk.payload)?;
                    chunks.push(KeptChunk::Frame(frame));
                    removal_required |= removed;
                }
                b"ALPH" => {
                    validate_alpha(chunk.payload)?;
                    chunks.push(KeptChunk::Alph(chunk.payload));
                }
                b"VP8 " | b"VP8L" => chunks.push(KeptChunk::Bitstream(chunk)),
                b"ICCP" => {
                    metadata.icc = true;
                    removal_required = true;
                }
                b"EXIF" => {
                    metadata.exif = true;
                    removal_required = true;
                }
                b"XMP " => {
                    metadata.xmp = true;
                    removal_required = true;
                }
                _ => removal_required = true,
            }
        }

        let layout = validate_layout(&chunks)?;
        if layout != Layout::Simple {
            validate_extended(&chunks, layout, &metadata)?;
        }

        Ok(Self {
            chunks,
            removal_required,
        })
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        body.extend_from_slice(b"WEBP");

        for chunk in &self.chunks {
            match chunk {
                KeptChunk::Vp8x(payload) => {
                    let mut normalized = payload.to_vec();
                    normalized[0] &= VP8X_ALPHA | VP8X_ANIMATION;
                    normalized[1..4].fill(0);
                    write_chunk(&mut body, *b"VP8X", &normalized)?;
                }
                KeptChunk::Anim(payload) => write_chunk(&mut body, *b"ANIM", payload)?,
                KeptChunk::Alph(payload) => {
                    write_chunk(&mut body, *b"ALPH", &normalize_alpha(payload))?;
                }
                KeptChunk::Bitstream(chunk) => {
                    write_chunk(&mut body, chunk.kind, chunk.payload)?;
                }
                KeptChunk::Frame(frame) => {
                    write_chunk(&mut body, *b"ANMF", &frame.encode()?)?;
                }
            }
        }

        let riff_size = u32::try_from(body.len()).context("normalized WebP is too large")?;
        let mut output = Vec::with_capacity(body.len() + 8);
        output.extend_from_slice(b"RIFF");
        output.extend_from_slice(&riff_size.to_le_bytes());
        output.extend_from_slice(&body);
        Ok(output)
    }
}

impl<'a> Frame<'a> {
    fn parse(payload: &'a [u8]) -> Result<(Self, bool)> {
        let header = payload
            .get(..16)
            .ok_or_else(|| anyhow!("ANMF chunk is shorter than its 16-byte header"))?;
        let raw_chunks = parse_chunks(payload, 16, payload.len())
            .context("invalid ANMF frame-data structure")?;
        let mut chunks = Vec::new();
        let mut removed = false;

        for chunk in raw_chunks {
            match &chunk.kind {
                b"ALPH" => {
                    validate_alpha(chunk.payload)?;
                    chunks.push(chunk);
                }
                b"VP8 " | b"VP8L" => chunks.push(chunk),
                _ => removed = true,
            }
        }

        let valid = matches!(chunks.as_slice(), [bitstream] if is_bitstream(bitstream.kind))
            || matches!(chunks.as_slice(), [alpha, bitstream]
                if alpha.kind == *b"ALPH" && is_bitstream(bitstream.kind));
        if !valid {
            bail!("ANMF must contain one bitstream, optionally preceded by ALPH");
        }

        Ok((Self { header, chunks }, removed))
    }

    fn validate_bounds(&self, canvas_width: u32, canvas_height: u32) -> Result<()> {
        let x = u64::from(read_u24(&self.header[0..3])) * 2;
        let y = u64::from(read_u24(&self.header[3..6])) * 2;
        let width = u64::from(read_u24(&self.header[6..9])) + 1;
        let height = u64::from(read_u24(&self.header[9..12])) + 1;
        if x + width > u64::from(canvas_width) || y + height > u64::from(canvas_height) {
            bail!("ANMF frame rectangle exceeds the WebP canvas");
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut payload = self.header.to_vec();
        payload[15] &= 0x03;
        for chunk in &self.chunks {
            if chunk.kind == *b"ALPH" {
                write_chunk(&mut payload, chunk.kind, &normalize_alpha(chunk.payload))?;
            } else {
                write_chunk(&mut payload, chunk.kind, chunk.payload)?;
            }
        }
        Ok(payload)
    }

    fn has_alpha(&self) -> bool {
        self.chunks.iter().any(|chunk| chunk.kind == *b"ALPH")
    }

    fn has_vp8l(&self) -> bool {
        self.chunks.iter().any(|chunk| chunk.kind == *b"VP8L")
    }
}

fn validate_layout(chunks: &[KeptChunk<'_>]) -> Result<Layout> {
    if matches!(chunks, [chunk] if chunk.is_bitstream()) {
        return Ok(Layout::Simple);
    }

    if !matches!(chunks.first(), Some(KeptChunk::Vp8x(_))) {
        bail!("invalid WebP chunk layout");
    }
    let image_data = &chunks[1..];
    if matches!(image_data, [chunk] if chunk.is_bitstream())
        || matches!(image_data, [KeptChunk::Alph(_), chunk] if chunk.is_bitstream())
    {
        return Ok(Layout::Still);
    }
    if matches!(image_data.first(), Some(KeptChunk::Anim(_)))
        && image_data.len() >= 2
        && image_data[1..]
            .iter()
            .all(|chunk| matches!(chunk, KeptChunk::Frame(_)))
    {
        return Ok(Layout::Animation);
    }

    bail!("invalid extended WebP chunk layout")
}

fn validate_extended(chunks: &[KeptChunk<'_>], layout: Layout, metadata: &Metadata) -> Result<()> {
    let KeptChunk::Vp8x(payload) = &chunks[0] else {
        unreachable!();
    };
    let flags = payload[0];
    require_flag(flags, VP8X_ICC, metadata.icc, "ICC")?;
    require_flag(flags, VP8X_EXIF, metadata.exif, "EXIF")?;
    require_flag(flags, VP8X_XMP, metadata.xmp, "XMP")?;
    require_flag(
        flags,
        VP8X_ANIMATION,
        layout == Layout::Animation,
        "animation",
    )?;

    let canvas_width = read_u24(&payload[4..7]) + 1;
    let canvas_height = read_u24(&payload[7..10]) + 1;
    if u64::from(canvas_width) * u64::from(canvas_height) > u64::from(u32::MAX) {
        bail!("WebP canvas dimensions exceed the format limit");
    }

    let has_alpha = chunks.iter().any(|chunk| match chunk {
        KeptChunk::Alph(_) => true,
        KeptChunk::Frame(frame) => frame.has_alpha(),
        _ => false,
    });
    let has_vp8l = chunks.iter().any(|chunk| match chunk {
        KeptChunk::Bitstream(chunk) => chunk.kind == *b"VP8L",
        KeptChunk::Frame(frame) => frame.has_vp8l(),
        _ => false,
    });
    let alpha_flag = flags & VP8X_ALPHA != 0;
    if has_alpha && !alpha_flag {
        bail!("VP8X alpha flag is missing for an ALPH chunk");
    }
    if alpha_flag && !has_alpha && !has_vp8l {
        bail!("VP8X alpha flag has no corresponding alpha-capable image data");
    }

    for chunk in chunks {
        if let KeptChunk::Frame(frame) = chunk {
            frame.validate_bounds(canvas_width, canvas_height)?;
        }
    }
    Ok(())
}

fn require_flag(flags: u8, mask: u8, present: bool, name: &str) -> Result<()> {
    if (flags & mask != 0) != present {
        bail!("VP8X {name} flag does not match the chunk structure");
    }
    Ok(())
}

fn validate_alpha(payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
        bail!("ALPH chunk has no control byte");
    }
    Ok(())
}

fn normalize_alpha(payload: &[u8]) -> Vec<u8> {
    let mut normalized = payload.to_vec();
    normalized[0] &= 0x3f;
    normalized
}

fn parse_chunks(input: &[u8], start: usize, end: usize) -> Result<Vec<Chunk<'_>>> {
    let mut chunks = Vec::new();
    let mut position = start;
    while position < end {
        let header_end = position
            .checked_add(8)
            .filter(|end_of_header| *end_of_header <= end)
            .ok_or_else(|| anyhow!("truncated chunk header at byte {position}"))?;
        let kind = input[position..position + 4].try_into().unwrap();
        let length =
            u32::from_le_bytes(input[position + 4..header_end].try_into().unwrap()) as usize;
        let payload_end = header_end
            .checked_add(length)
            .ok_or_else(|| anyhow!("chunk payload size overflow at byte {position}"))?;
        let padded_end = payload_end
            .checked_add(length & 1)
            .filter(|end_of_chunk| *end_of_chunk <= end)
            .ok_or_else(|| anyhow!("truncated chunk payload at byte {position}"))?;
        chunks.push(Chunk {
            kind,
            payload: &input[header_end..payload_end],
        });
        position = padded_end;
    }
    Ok(chunks)
}

fn write_chunk(output: &mut Vec<u8>, kind: [u8; 4], payload: &[u8]) -> Result<()> {
    let length = u32::try_from(payload.len()).context("WebP chunk is too large")?;
    output.extend_from_slice(&kind);
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(payload);
    if !payload.len().is_multiple_of(2) {
        output.push(0);
    }
    Ok(())
}

fn read_u24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

fn is_bitstream(kind: [u8; 4]) -> bool {
    matches!(&kind, b"VP8 " | b"VP8L")
}
