use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};
use oxipng::internal_tests::{PngData, PngImage};
use oxipng::{BitDepth, ColorType, Options, RGB16, RawImage, StripChunks};
use png::{
    BitDepth as DecodedBitDepth, ColorType as DecodedColorType, Decoder, Limits, Transformations,
};

use super::{Candidate, Change, FormatOutcome};

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_DECOMPRESSED_SIZE: usize = 512 * 1024 * 1024;

pub(super) fn optimize(input: &[u8]) -> Result<FormatOutcome> {
    let audit = inspect(input)?;
    if audit.apng {
        bail!("UnsupportedApngWithStripAll: APNG cannot be optimized without removing animation");
    }

    let options = options();
    let source = decode_canonical(input)
        .context("failed to decode source PNG for canonical verification")?;
    let preliminary = oxipng::optimize_from_memory(input, &options)
        .map_err(|error| anyhow!("PNG normalization failed: {error}"))?;
    let decoded = PngData::from_slice(&preliminary, &options)
        .map_err(|error| anyhow!("normalized PNG failed to decode: {error}"))?;
    if decoded.raw.ihdr.interlaced {
        bail!("oxipng did not produce the requested non-interlaced PNG");
    }
    let baseline = normalize_baseline(decoded.raw.as_ref().clone())?;

    let outputs = encode_candidates(&baseline, &options)?;
    let selected = select_candidate(&source, outputs, &options)?;
    if selected == input {
        return Ok(FormatOutcome::Unchanged);
    }

    let mut changes = vec![Change::Recompressed];
    if audit.metadata_cleanup_required() {
        changes.push(Change::MetadataRemoved);
    }
    changes.push(Change::ContainerNormalized);
    Ok(FormatOutcome::Candidate(Candidate {
        bytes: selected,
        changes,
    }))
}

fn options() -> Options {
    let mut options = Options::from_preset(2);
    options.interlace = Some(false);
    options.optimize_alpha = false;
    options.bit_depth_reduction = true;
    options.color_type_reduction = true;
    options.palette_reduction = true;
    options.grayscale_reduction = true;
    options.scale_16 = false;
    options.idat_recoding = true;
    options.fix_errors = false;
    options.force = true;
    options.strip = StripChunks::All;
    options.max_decompressed_size = Some(MAX_DECOMPRESSED_SIZE);
    options
}

fn normalize_baseline(mut image: PngImage) -> Result<PngImage> {
    compact_indexed_palette(&mut image)?;
    remove_unused_direct_trns(&mut image)?;
    zero_row_padding(&mut image)?;
    Ok(image)
}

fn encode_candidates(source: &PngImage, options: &Options) -> Result<Vec<Vec<u8>>> {
    let mut outputs = Vec::with_capacity(4);
    if let Some(transparency_variant) = exact_alpha_to_trns(source)? {
        push_encoded(&mut outputs, transparency_variant, options)?;
    }
    if let Some(direct_variant) = indexed_to_direct(source)? {
        if let Some(transparency_variant) = exact_alpha_to_trns(&direct_variant)? {
            push_encoded(&mut outputs, transparency_variant, options)?;
        }
        push_encoded(&mut outputs, direct_variant, options)?;
    }
    push_encoded(&mut outputs, source.clone(), options)?;
    Ok(outputs)
}

fn select_candidate(
    source: &DecodedImage,
    outputs: Vec<Vec<u8>>,
    options: &Options,
) -> Result<Vec<u8>> {
    let mut selected: Option<Vec<u8>> = None;
    let mut last_error = None;
    for output in outputs {
        match verify_candidate(source, &output, options) {
            Ok(()) => {
                if selected
                    .as_ref()
                    .is_none_or(|current| output.len() < current.len())
                {
                    selected = Some(output);
                }
            }
            Err(error) => last_error = Some(error),
        }
    }

    if let Some(selected) = selected {
        return Ok(selected);
    }
    if let Some(error) = last_error {
        return Err(error).context("no sanitized PNG candidate passed verification");
    }
    bail!("no sanitized PNG candidate was produced")
}

fn push_encoded(outputs: &mut Vec<Vec<u8>>, image: PngImage, options: &Options) -> Result<()> {
    let width = image.ihdr.width;
    let height = image.ihdr.height;
    let bit_depth = image.ihdr.bit_depth;
    let color_type = image.ihdr.color_type;
    let raw = RawImage::new(width, height, color_type, bit_depth, image.data)
        .map_err(|error| anyhow!("failed to construct sanitized PNG: {error}"))?;
    let output = raw
        .create_optimized_png(options)
        .map_err(|error| anyhow!("PNG recompression failed: {error}"))?;
    if !outputs.contains(&output) {
        outputs.push(output);
    }
    Ok(())
}

fn verify_candidate(source: &DecodedImage, candidate: &[u8], options: &Options) -> Result<()> {
    let audit = inspect(candidate)?;
    if audit.apng || audit.ancillary_or_trailing || audit.truecolor_palette {
        bail!("PNG output is not structurally sanitized");
    }

    let decoded = PngData::from_slice(candidate, options)
        .map_err(|error| anyhow!("optimized PNG failed to decode: {error}"))?;
    verify_sanitized_representation(&decoded.raw, audit.saw_trns)?;
    let decoded_pixels = decode_canonical(candidate)
        .context("failed to decode optimized PNG for canonical verification")?;
    compare_canonical_pixels(source, &decoded_pixels)
}

fn compact_indexed_palette(image: &mut PngImage) -> Result<()> {
    let ColorType::Indexed { palette } = image.ihdr.color_type.clone() else {
        return Ok(());
    };
    let view = SampleView::new(image)?;
    let mut used = vec![false; palette.len()];
    for y in 0..view.height {
        for x in 0..view.width {
            let index = usize::from(view.sample(x, y, 0)?);
            let entry = used
                .get_mut(index)
                .ok_or_else(|| anyhow!("PNG palette index {index} is out of range"))?;
            *entry = true;
        }
    }

    let mut colors = HashMap::with_capacity(palette.len());
    let mut remap = vec![0u8; palette.len()];
    let mut compact = Vec::with_capacity(palette.len());
    for (index, color) in palette.iter().copied().enumerate() {
        if !used[index] {
            continue;
        }
        let key = (color.r, color.g, color.b, color.a);
        let new_index = *colors.entry(key).or_insert_with(|| {
            let new_index = compact.len() as u8;
            compact.push(color);
            new_index
        });
        remap[index] = new_index;
    }

    if compact.len() == palette.len()
        && remap
            .iter()
            .enumerate()
            .all(|(index, mapped)| index == usize::from(*mapped))
    {
        return Ok(());
    }

    let mut data = vec![0; image.data.len()];
    for y in 0..view.height {
        for x in 0..view.width {
            let old_index = usize::from(view.sample(x, y, 0)?);
            write_packed_sample(
                &mut data,
                view.row_bytes,
                view.depth,
                x,
                y,
                remap[old_index],
            )?;
        }
    }
    image.ihdr.color_type = ColorType::Indexed { palette: compact };
    image.data = data;
    Ok(())
}

fn indexed_to_direct(image: &PngImage) -> Result<Option<PngImage>> {
    let ColorType::Indexed { palette } = &image.ihdr.color_type else {
        return Ok(None);
    };
    let view = SampleView::new(image)?;
    let grayscale = palette
        .iter()
        .all(|color| color.r == color.g && color.g == color.b);
    let alpha = palette.iter().any(|color| color.a != u8::MAX);
    let channels = if grayscale { 1usize } else { 3 } + usize::from(alpha);
    let capacity = view
        .width
        .checked_mul(view.height)
        .and_then(|pixels| pixels.checked_mul(channels))
        .filter(|size| *size <= MAX_DECOMPRESSED_SIZE);
    let Some(capacity) = capacity else {
        return Ok(None);
    };

    let mut data = Vec::with_capacity(capacity);
    for y in 0..view.height {
        for x in 0..view.width {
            let index = usize::from(view.sample(x, y, 0)?);
            let color = palette
                .get(index)
                .ok_or_else(|| anyhow!("PNG palette index {index} is out of range"))?;
            if grayscale {
                data.push(color.r);
            } else {
                data.extend_from_slice(&[color.r, color.g, color.b]);
            }
            if alpha {
                data.push(color.a);
            }
        }
    }

    let color_type = match (grayscale, alpha) {
        (true, true) => ColorType::GrayscaleAlpha,
        (true, false) => ColorType::Grayscale {
            transparent_shade: None,
        },
        (false, true) => ColorType::RGBA,
        (false, false) => ColorType::RGB {
            transparent_color: None,
        },
    };
    let mut direct = image.clone();
    direct.ihdr.color_type = color_type;
    direct.ihdr.bit_depth = BitDepth::Eight;
    direct.data = data;
    Ok(Some(direct))
}

fn exact_alpha_to_trns(image: &PngImage) -> Result<Option<PngImage>> {
    let color_channels = match image.ihdr.color_type {
        ColorType::GrayscaleAlpha => 1,
        ColorType::RGBA => 3,
        _ => return Ok(None),
    };
    let view = SampleView::new(image)?;
    let alpha_channel = color_channels;
    let max_sample = sample_max(view.depth);
    let color_at = |x, y| -> Result<[u16; 3]> {
        let mut color = [0; 3];
        for (channel, sample) in color.iter_mut().take(color_channels).enumerate() {
            *sample = view.sample(x, y, channel)?;
        }
        Ok(color)
    };
    let mut transparent_color = None;

    for y in 0..view.height {
        for x in 0..view.width {
            let alpha = view.sample(x, y, alpha_channel)?;
            if alpha != 0 && alpha != max_sample {
                return Ok(None);
            }
            if alpha == 0 {
                let color = color_at(x, y)?;
                match &transparent_color {
                    Some(previous) if previous != &color => return Ok(None),
                    Some(_) => {}
                    None => transparent_color = Some(color),
                }
            }
        }
    }

    let Some(transparent_color) = transparent_color else {
        return Ok(None);
    };
    for y in 0..view.height {
        for x in 0..view.width {
            if view.sample(x, y, alpha_channel)? == max_sample
                && color_at(x, y)? == transparent_color
            {
                return Ok(None);
            }
        }
    }

    let bytes_per_sample = usize::from(view.depth / 8);
    let capacity = view
        .width
        .checked_mul(view.height)
        .and_then(|pixels| pixels.checked_mul(color_channels))
        .and_then(|samples| samples.checked_mul(bytes_per_sample))
        .ok_or_else(|| anyhow!("PNG alpha reduction size overflow"))?;
    let mut data = Vec::with_capacity(capacity);
    for y in 0..view.height {
        for x in 0..view.width {
            for channel in 0..color_channels {
                let sample = view.sample(x, y, channel)?;
                if view.depth == 8 {
                    data.push(sample as u8);
                } else {
                    data.extend_from_slice(&sample.to_be_bytes());
                }
            }
        }
    }

    let color_type = if color_channels == 1 {
        ColorType::Grayscale {
            transparent_shade: Some(transparent_color[0]),
        }
    } else {
        ColorType::RGB {
            transparent_color: Some(RGB16::new(
                transparent_color[0],
                transparent_color[1],
                transparent_color[2],
            )),
        }
    };
    let mut reduced = image.clone();
    reduced.ihdr.color_type = color_type;
    reduced.data = data;
    Ok(Some(reduced))
}

fn remove_unused_direct_trns(image: &mut PngImage) -> Result<()> {
    let view = SampleView::new(image)?;
    let replacement = match image.ihdr.color_type.clone() {
        ColorType::Grayscale {
            transparent_shade: Some(transparent),
        } => {
            let mut used = false;
            for y in 0..view.height {
                for x in 0..view.width {
                    used |= view.sample(x, y, 0)? == transparent;
                }
            }
            (!used).then_some(ColorType::Grayscale {
                transparent_shade: None,
            })
        }
        ColorType::RGB {
            transparent_color: Some(transparent),
        } => {
            let mut used = false;
            for y in 0..view.height {
                for x in 0..view.width {
                    used |= view.sample(x, y, 0)? == transparent.r
                        && view.sample(x, y, 1)? == transparent.g
                        && view.sample(x, y, 2)? == transparent.b;
                }
            }
            (!used).then_some(ColorType::RGB {
                transparent_color: None,
            })
        }
        _ => None,
    };
    if let Some(color_type) = replacement {
        image.ihdr.color_type = color_type;
    }
    Ok(())
}

#[derive(Default)]
struct PngAudit {
    ancillary_or_trailing: bool,
    apng: bool,
    truecolor_palette: bool,
    saw_trns: bool,
}

impl PngAudit {
    fn metadata_cleanup_required(&self) -> bool {
        self.ancillary_or_trailing || self.truecolor_palette
    }
}

fn inspect(input: &[u8]) -> Result<PngAudit> {
    if !input.starts_with(SIGNATURE) {
        bail!("invalid PNG signature");
    }

    let mut position = SIGNATURE.len();
    let mut audit = PngAudit::default();
    let mut saw_ihdr = false;
    let mut color_type = None;

    while position < input.len() {
        let header = input
            .get(position..position + 8)
            .ok_or_else(|| anyhow!("truncated PNG chunk header"))?;
        let data_length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = header[4..8].try_into().unwrap();
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            bail!("invalid PNG chunk type");
        }
        let data_start = position
            .checked_add(8)
            .ok_or_else(|| anyhow!("PNG chunk offset overflow"))?;
        let data_end = data_start
            .checked_add(data_length)
            .filter(|end| {
                end.checked_add(4)
                    .is_some_and(|crc_end| crc_end <= input.len())
            })
            .ok_or_else(|| anyhow!("truncated PNG chunk"))?;
        let end = data_end + 4;
        let data = &input[data_start..data_end];

        if !saw_ihdr && kind != *b"IHDR" {
            bail!("PNG IHDR must be the first chunk");
        }
        if kind == *b"IHDR" {
            if saw_ihdr || data_length != 13 {
                bail!("invalid or duplicate PNG IHDR");
            }
            saw_ihdr = true;
            color_type = Some(data[9]);
        } else if kind == *b"PLTE" && matches!(color_type, Some(2 | 6)) {
            audit.truecolor_palette = true;
        }
        if kind == *b"tRNS" {
            audit.saw_trns = true;
        }
        if matches!(&kind, b"acTL" | b"fcTL" | b"fdAT") {
            audit.apng = true;
        }

        let ancillary = kind[0] & 0x20 != 0;
        if ancillary && kind != *b"tRNS" {
            audit.ancillary_or_trailing = true;
        } else if !ancillary && !matches!(&kind, b"IHDR" | b"PLTE" | b"IDAT" | b"IEND") {
            bail!(
                "unknown critical PNG chunk: {}",
                String::from_utf8_lossy(&kind)
            );
        }

        position = end;
        if kind == *b"IEND" {
            if data_length != 0 {
                bail!("PNG IEND must be empty");
            }
            audit.ancillary_or_trailing |= position != input.len();
            return Ok(audit);
        }
    }

    bail!("PNG is missing IEND")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CanonicalPixel {
    r: u16,
    g: u16,
    b: u16,
    a: u16,
}

struct DecodedImage {
    width: usize,
    height: usize,
    color_type: DecodedColorType,
    bit_depth: DecodedBitDepth,
    line_size: usize,
    data: Vec<u8>,
}

fn decode_canonical(input: &[u8]) -> Result<DecodedImage> {
    let mut decoder = Decoder::new_with_limits(
        Cursor::new(input),
        Limits {
            bytes: MAX_DECOMPRESSED_SIZE,
        },
    );
    decoder.set_transformations(Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .map_err(|error| anyhow!("PNG decode failed: {error}"))?;
    let buffer_size = reader
        .output_buffer_size()
        .filter(|size| *size <= MAX_DECOMPRESSED_SIZE)
        .ok_or_else(|| anyhow!("expanded PNG exceeds the decompressed size limit"))?;
    let mut data = vec![0; buffer_size];
    let info = reader
        .next_frame(&mut data)
        .map_err(|error| anyhow!("PNG pixel decode failed: {error}"))?;
    data.truncate(info.buffer_size());

    if matches!(info.color_type, DecodedColorType::Indexed)
        || !matches!(
            info.bit_depth,
            DecodedBitDepth::Eight | DecodedBitDepth::Sixteen
        )
    {
        bail!("PNG expansion did not produce 8-bit or 16-bit direct-color samples");
    }
    Ok(DecodedImage {
        width: info.width as usize,
        height: info.height as usize,
        color_type: info.color_type,
        bit_depth: info.bit_depth,
        line_size: info.line_size,
        data,
    })
}

struct CanonicalPixels<'a> {
    image: &'a DecodedImage,
    next: usize,
}

impl<'a> CanonicalPixels<'a> {
    fn new(image: &'a DecodedImage) -> Self {
        Self { image, next: 0 }
    }
}

impl Iterator for CanonicalPixels<'_> {
    type Item = CanonicalPixel;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.image.width * self.image.height {
            return None;
        }
        let x = self.next % self.image.width;
        let y = self.next / self.image.width;
        self.next += 1;
        Some(self.image.canonical_pixel(x, y))
    }
}

impl DecodedImage {
    fn canonical_pixel(&self, x: usize, y: usize) -> CanonicalPixel {
        let sample = |channel| {
            let channels = decoded_channels(self.color_type);
            let byte_depth = decoded_byte_depth(self.bit_depth);
            let offset = y * self.line_size + (x * channels + channel) * byte_depth;
            if byte_depth == 1 {
                u16::from(self.data[offset]) * 257
            } else {
                u16::from_be_bytes([self.data[offset], self.data[offset + 1]])
            }
        };
        match self.color_type {
            DecodedColorType::Grayscale => {
                let gray = sample(0);
                CanonicalPixel {
                    r: gray,
                    g: gray,
                    b: gray,
                    a: u16::MAX,
                }
            }
            DecodedColorType::Rgb => CanonicalPixel {
                r: sample(0),
                g: sample(1),
                b: sample(2),
                a: u16::MAX,
            },
            DecodedColorType::GrayscaleAlpha => {
                let gray = sample(0);
                CanonicalPixel {
                    r: gray,
                    g: gray,
                    b: gray,
                    a: sample(1),
                }
            }
            DecodedColorType::Rgba => CanonicalPixel {
                r: sample(0),
                g: sample(1),
                b: sample(2),
                a: sample(3),
            },
            DecodedColorType::Indexed => unreachable!("indexed data is expanded by the decoder"),
        }
    }
}

#[derive(Clone, Copy)]
struct SampleView<'a> {
    image: &'a PngImage,
    width: usize,
    height: usize,
    channels: usize,
    depth: u8,
    row_bytes: usize,
}

impl<'a> SampleView<'a> {
    fn new(image: &'a PngImage) -> Result<Self> {
        if image.ihdr.interlaced {
            bail!("canonical PNG iteration requires non-interlaced samples");
        }
        let width = image.ihdr.width as usize;
        let height = image.ihdr.height as usize;
        let channels = channels(&image.ihdr.color_type);
        let depth = image.ihdr.bit_depth as u8;
        let bits_per_row = width
            .checked_mul(channels)
            .and_then(|samples| samples.checked_mul(usize::from(depth)))
            .ok_or_else(|| anyhow!("PNG row size overflow"))?;
        let row_bytes = bits_per_row
            .checked_add(7)
            .ok_or_else(|| anyhow!("PNG row size overflow"))?
            / 8;
        let expected = row_bytes
            .checked_mul(height)
            .ok_or_else(|| anyhow!("PNG image size overflow"))?;
        if expected > MAX_DECOMPRESSED_SIZE || image.data.len() != expected {
            bail!("PNG decoded sample length is invalid or exceeds the safety limit");
        }
        Ok(Self {
            image,
            width,
            height,
            channels,
            depth,
            row_bytes,
        })
    }

    fn sample(self, x: usize, y: usize, channel: usize) -> Result<u16> {
        if x >= self.width || y >= self.height || channel >= self.channels {
            bail!("PNG sample coordinate is out of bounds");
        }
        let sample_index = x * self.channels + channel;
        let row_start = y * self.row_bytes;
        match self.depth {
            1 | 2 | 4 => {
                let bit_offset = sample_index * usize::from(self.depth);
                let byte = self.image.data[row_start + bit_offset / 8];
                let shift = 8 - usize::from(self.depth) - bit_offset % 8;
                Ok(u16::from((byte >> shift) & ((1u8 << self.depth) - 1)))
            }
            8 => Ok(u16::from(self.image.data[row_start + sample_index])),
            16 => {
                let offset = row_start + sample_index * 2;
                Ok(u16::from_be_bytes([
                    self.image.data[offset],
                    self.image.data[offset + 1],
                ]))
            }
            _ => bail!("unsupported PNG bit depth: {}", self.depth),
        }
    }
}

fn compare_canonical_pixels(before: &DecodedImage, after: &DecodedImage) -> Result<()> {
    if before.width != after.width || before.height != after.height {
        bail!("PNG dimensions changed during sanitation");
    }

    let before_pixels = CanonicalPixels::new(before);
    let after_pixels = CanonicalPixels::new(after);
    for (index, (before, after)) in before_pixels.zip(after_pixels).enumerate() {
        if before != after {
            bail!("PNG canonical pixel {index} changed during sanitation");
        }
    }
    Ok(())
}

fn verify_sanitized_representation(image: &PngImage, saw_trns: bool) -> Result<()> {
    if image.ihdr.interlaced {
        bail!("PNG output is still interlaced");
    }
    if has_nonzero_row_padding(image)? {
        bail!("PNG output contains non-zero row padding bits");
    }

    let view = SampleView::new(image)?;
    match &image.ihdr.color_type {
        ColorType::Indexed { palette } => {
            let mut used = vec![false; palette.len()];
            for y in 0..view.height {
                for x in 0..view.width {
                    let index = usize::from(view.sample(x, y, 0)?);
                    let entry = used
                        .get_mut(index)
                        .ok_or_else(|| anyhow!("PNG palette index {index} is out of range"))?;
                    *entry = true;
                }
            }
            if used.iter().any(|used| !used) {
                bail!("PNG output contains unused palette entries");
            }
            let mut colors = HashSet::with_capacity(palette.len());
            if palette
                .iter()
                .any(|color| !colors.insert((color.r, color.g, color.b, color.a)))
            {
                bail!("PNG output contains duplicate RGBA palette entries");
            }
            let has_transparency = palette.iter().any(|color| color.a != u8::MAX);
            if saw_trns != has_transparency {
                bail!("PNG output contains an ineffective or missing indexed tRNS chunk");
            }
        }
        ColorType::Grayscale { transparent_shade } => {
            if let Some(transparent) = transparent_shade {
                let used = any_pixel(
                    image,
                    |view, x, y| Ok(view.sample(x, y, 0)? == *transparent),
                )?;
                if !used {
                    bail!("PNG output contains an unused grayscale tRNS value");
                }
            }
            if saw_trns != transparent_shade.is_some() {
                bail!("PNG output contains an ineffective or missing grayscale tRNS chunk");
            }
        }
        ColorType::RGB { transparent_color } => {
            if let Some(transparent) = transparent_color {
                let used = any_pixel(image, |view, x, y| {
                    Ok(view.sample(x, y, 0)? == transparent.r
                        && view.sample(x, y, 1)? == transparent.g
                        && view.sample(x, y, 2)? == transparent.b)
                })?;
                if !used {
                    bail!("PNG output contains an unused RGB tRNS value");
                }
            }
            if saw_trns != transparent_color.is_some() {
                bail!("PNG output contains an ineffective or missing RGB tRNS chunk");
            }
        }
        ColorType::GrayscaleAlpha | ColorType::RGBA => {
            if saw_trns {
                bail!("PNG output has tRNS in a color type with an alpha channel");
            }
        }
    }
    Ok(())
}

fn any_pixel(
    image: &PngImage,
    mut predicate: impl FnMut(SampleView<'_>, usize, usize) -> Result<bool>,
) -> Result<bool> {
    let view = SampleView::new(image)?;
    for y in 0..view.height {
        for x in 0..view.width {
            if predicate(view, x, y)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn has_nonzero_row_padding(image: &PngImage) -> Result<bool> {
    let view = SampleView::new(image)?;
    let used_bits = view.width * view.channels * usize::from(view.depth);
    let padding_bits = (8 - used_bits % 8) % 8;
    if padding_bits == 0 || view.row_bytes == 0 {
        return Ok(false);
    }
    let mask = (1u8 << padding_bits) - 1;
    Ok((0..view.height).any(|row| image.data[(row + 1) * view.row_bytes - 1] & mask != 0))
}

fn zero_row_padding(image: &mut PngImage) -> Result<()> {
    let view = SampleView::new(image)?;
    let used_bits = view.width * view.channels * usize::from(view.depth);
    let padding_bits = (8 - used_bits % 8) % 8;
    let row_bytes = view.row_bytes;
    let height = view.height;
    if padding_bits == 0 || row_bytes == 0 {
        return Ok(());
    }
    let keep_mask = !((1u8 << padding_bits) - 1);
    for row in 0..height {
        image.data[(row + 1) * row_bytes - 1] &= keep_mask;
    }
    Ok(())
}

fn write_packed_sample(
    data: &mut [u8],
    row_bytes: usize,
    depth: u8,
    x: usize,
    y: usize,
    value: u8,
) -> Result<()> {
    if u16::from(value) > sample_max(depth) {
        bail!("remapped PNG palette index does not fit the source bit depth");
    }
    let row_start = y
        .checked_mul(row_bytes)
        .ok_or_else(|| anyhow!("PNG row offset overflow"))?;
    match depth {
        1 | 2 | 4 => {
            let bit_offset = x * usize::from(depth);
            let offset = row_start + bit_offset / 8;
            let shift = 8 - usize::from(depth) - bit_offset % 8;
            let mask = ((1u8 << depth) - 1) << shift;
            data[offset] = (data[offset] & !mask) | ((value << shift) & mask);
        }
        8 => data[row_start + x] = value,
        _ => bail!("unsupported indexed PNG bit depth: {depth}"),
    }
    Ok(())
}

fn channels(color_type: &ColorType) -> usize {
    match color_type {
        ColorType::Grayscale { .. } | ColorType::Indexed { .. } => 1,
        ColorType::GrayscaleAlpha => 2,
        ColorType::RGB { .. } => 3,
        ColorType::RGBA => 4,
    }
}

fn decoded_channels(color_type: DecodedColorType) -> usize {
    match color_type {
        DecodedColorType::Grayscale | DecodedColorType::Indexed => 1,
        DecodedColorType::GrayscaleAlpha => 2,
        DecodedColorType::Rgb => 3,
        DecodedColorType::Rgba => 4,
    }
}

fn decoded_byte_depth(bit_depth: DecodedBitDepth) -> usize {
    match bit_depth {
        DecodedBitDepth::Eight => 1,
        DecodedBitDepth::Sixteen => 2,
        _ => unreachable!("canonical decoding rejects sub-byte output"),
    }
}

fn sample_max(depth: u8) -> u16 {
    if depth == 16 {
        u16::MAX
    } else {
        (1u16 << depth) - 1
    }
}
