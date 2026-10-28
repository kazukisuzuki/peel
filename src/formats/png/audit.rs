use anyhow::{Result, anyhow, bail};

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Copy, Eq, PartialEq)]
enum FrameData {
    Missing,
    Idat,
    Fdat,
}

#[derive(Default)]
pub(super) struct PngAudit {
    pub(super) ancillary_or_trailing: bool,
    pub(super) is_animated: bool,
    pub(super) truecolor_palette: bool,
    pub(super) saw_trns: bool,
}

impl PngAudit {
    pub(super) fn metadata_cleanup_required(&self) -> bool {
        self.ancillary_or_trailing || self.truecolor_palette
    }
}

struct Chunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
}

impl Chunk<'_> {
    fn u32_at(&self, offset: usize) -> u32 {
        u32::from_be_bytes(
            self.data[offset..offset + 4]
                .try_into()
                .expect("validated PNG chunk contains the requested u32"),
        )
    }
}

struct ChunkReader<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> ChunkReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            position: SIGNATURE.len(),
        }
    }

    fn next_chunk(&mut self) -> Result<Option<Chunk<'a>>> {
        if self.position >= self.input.len() {
            return Ok(None);
        }

        let header = self
            .input
            .get(self.position..self.position + 8)
            .ok_or_else(|| anyhow!("truncated PNG chunk header"))?;
        let data_length = u32::from_be_bytes(
            header[..4]
                .try_into()
                .expect("PNG chunk length slice has four bytes"),
        ) as usize;
        let kind: [u8; 4] = header[4..8]
            .try_into()
            .expect("PNG chunk type slice has four bytes");
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            bail!("invalid PNG chunk type");
        }
        let data_start = self
            .position
            .checked_add(8)
            .ok_or_else(|| anyhow!("PNG chunk offset overflow"))?;
        let data_end = data_start
            .checked_add(data_length)
            .filter(|end| {
                end.checked_add(4)
                    .is_some_and(|crc_end| crc_end <= self.input.len())
            })
            .ok_or_else(|| anyhow!("truncated PNG chunk"))?;
        let end = data_end + 4;
        let chunk = Chunk {
            kind,
            data: &self.input[data_start..data_end],
        };
        self.position = end;
        Ok(Some(chunk))
    }

    fn has_trailing_data(&self) -> bool {
        self.position != self.input.len()
    }
}

#[derive(Default)]
struct PngState {
    saw_ihdr: bool,
    canvas: Option<(u32, u32)>,
    color_type: Option<u8>,
    saw_idat: bool,
    idat_ended: bool,
}

impl PngState {
    fn inspect_chunk(&mut self, chunk: &Chunk<'_>, audit: &mut PngAudit) -> Result<()> {
        if !self.saw_ihdr && chunk.kind != *b"IHDR" {
            bail!("PNG IHDR must be the first chunk");
        }
        if chunk.kind == *b"IHDR" {
            if self.saw_ihdr || chunk.data.len() != 13 {
                bail!("invalid or duplicate PNG IHDR");
            }
            self.saw_ihdr = true;
            let width = chunk.u32_at(0);
            let height = chunk.u32_at(4);
            if width == 0 || height == 0 {
                bail!("PNG canvas dimensions must be non-zero");
            }
            self.canvas = Some((width, height));
            self.color_type = Some(chunk.data[9]);
        } else if chunk.kind == *b"PLTE" && matches!(self.color_type, Some(2 | 6)) {
            audit.truecolor_palette = true;
        }
        if chunk.kind == *b"tRNS" {
            audit.saw_trns = true;
        }
        if chunk.kind == *b"IDAT" {
            if self.idat_ended {
                bail!("PNG IDAT chunks must be consecutive");
            }
            self.saw_idat = true;
        } else if self.saw_idat {
            self.idat_ended = true;
        }
        Ok(())
    }

    fn canvas(&self) -> (u32, u32) {
        self.canvas
            .expect("IHDR validation establishes PNG canvas dimensions")
    }
}

#[derive(Default)]
struct ApngState {
    num_frames: Option<u32>,
    expected_sequence: u64,
    frame_count: u32,
    current_frame_data: Option<FrameData>,
}

impl ApngState {
    fn inspect_chunk(&mut self, chunk: &Chunk<'_>, png: &PngState) -> Result<()> {
        match &chunk.kind {
            b"acTL" => {
                if self.num_frames.is_some() || chunk.data.len() != 8 || png.saw_idat {
                    bail!("invalid, duplicate, or misplaced APNG acTL chunk");
                }
                let num_frames = chunk.u32_at(0);
                if num_frames == 0 {
                    bail!("APNG must declare at least one animation frame");
                }
                self.num_frames = Some(num_frames);
            }
            b"fcTL" => {
                if self.num_frames.is_none() || chunk.data.len() != 26 {
                    bail!("APNG fcTL requires a preceding acTL chunk and length 26");
                }
                if self.current_frame_data == Some(FrameData::Missing) {
                    bail!("APNG frame has no image data");
                }
                let sequence = chunk.u32_at(0);
                if u64::from(sequence) != self.expected_sequence {
                    bail!("APNG sequence number is not contiguous");
                }
                self.expected_sequence += 1;
                let width = chunk.u32_at(4);
                let height = chunk.u32_at(8);
                let x_offset = chunk.u32_at(12);
                let y_offset = chunk.u32_at(16);
                let (canvas_width, canvas_height) = png.canvas();
                if width == 0
                    || height == 0
                    || x_offset
                        .checked_add(width)
                        .is_none_or(|right| right > canvas_width)
                    || y_offset
                        .checked_add(height)
                        .is_none_or(|bottom| bottom > canvas_height)
                {
                    bail!("APNG frame rectangle is outside the canvas");
                }
                if chunk.data[24] > 2 || chunk.data[25] > 1 {
                    bail!("APNG frame has an invalid dispose or blend operation");
                }
                if !png.saw_idat
                    && (self.frame_count != 0
                        || width != canvas_width
                        || height != canvas_height
                        || x_offset != 0
                        || y_offset != 0)
                {
                    bail!("APNG default-image frame must cover the canvas");
                }
                self.frame_count = self
                    .frame_count
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("APNG frame count overflow"))?;
                self.current_frame_data = Some(FrameData::Missing);
            }
            b"IDAT" => match self.current_frame_data {
                Some(FrameData::Missing | FrameData::Idat) => {
                    self.current_frame_data = Some(FrameData::Idat);
                }
                Some(FrameData::Fdat) => bail!("APNG frame mixes IDAT and fdAT image data"),
                None => {}
            },
            b"fdAT" => {
                if self.num_frames.is_none() || !png.saw_idat || chunk.data.len() < 4 {
                    bail!("invalid or misplaced APNG fdAT chunk");
                }
                match self.current_frame_data {
                    Some(FrameData::Missing | FrameData::Fdat) => {}
                    Some(FrameData::Idat) => bail!("APNG frame mixes IDAT and fdAT image data"),
                    None => bail!("APNG fdAT chunk has no corresponding frame control"),
                }
                let sequence = chunk.u32_at(0);
                if u64::from(sequence) != self.expected_sequence {
                    bail!("APNG sequence number is not contiguous");
                }
                self.expected_sequence += 1;
                self.current_frame_data = Some(FrameData::Fdat);
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(&self) -> Result<bool> {
        if self.current_frame_data == Some(FrameData::Missing) {
            bail!("APNG frame has no image data");
        }
        if let Some(num_frames) = self.num_frames
            && self.frame_count != num_frames
        {
            bail!("APNG acTL frame count does not match fcTL chunks");
        }
        Ok(self.num_frames.is_some())
    }
}

#[derive(Default)]
struct InspectionState {
    png: PngState,
    apng: ApngState,
    audit: PngAudit,
}

impl InspectionState {
    fn inspect_chunk(&mut self, chunk: &Chunk<'_>) -> Result<()> {
        self.png.inspect_chunk(chunk, &mut self.audit)?;
        self.apng.inspect_chunk(chunk, &self.png)?;

        let ancillary = chunk.kind[0] & 0x20 != 0;
        if ancillary && !matches!(&chunk.kind, b"tRNS" | b"acTL" | b"fcTL" | b"fdAT") {
            self.audit.ancillary_or_trailing = true;
        } else if !ancillary && !matches!(&chunk.kind, b"IHDR" | b"PLTE" | b"IDAT" | b"IEND") {
            bail!(
                "unknown critical PNG chunk: {}",
                String::from_utf8_lossy(&chunk.kind)
            );
        }
        Ok(())
    }

    fn finish(mut self, iend: &Chunk<'_>, has_trailing_data: bool) -> Result<PngAudit> {
        if !iend.data.is_empty() {
            bail!("PNG IEND must be empty");
        }
        if !self.png.saw_idat {
            bail!("PNG is missing IDAT");
        }
        self.audit.is_animated = self.apng.finish()?;
        self.audit.ancillary_or_trailing |= has_trailing_data;
        Ok(self.audit)
    }
}

pub(super) fn inspect(input: &[u8]) -> Result<PngAudit> {
    if !input.starts_with(SIGNATURE) {
        bail!("invalid PNG signature");
    }

    let mut chunks = ChunkReader::new(input);
    let mut state = InspectionState::default();

    while let Some(chunk) = chunks.next_chunk()? {
        state.inspect_chunk(&chunk)?;
        let kind = chunk.kind;

        if kind == *b"IEND" {
            return state.finish(&chunk, chunks.has_trailing_data());
        }
    }

    bail!("PNG is missing IEND")
}
