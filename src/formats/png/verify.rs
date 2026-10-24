use std::collections::HashSet;
use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};
use png::{
    BitDepth as DecodedBitDepth, ColorType as DecodedColorType, Decoder, FrameControl, Limits,
    OutputInfo, Transformations,
};

use super::MAX_DECOMPRESSED_SIZE;
use super::audit::inspect;
use super::raster::{
    RasterColorType, RasterImage, SampleView, decode_static_raster, decoded_byte_depth,
    decoded_channels, has_nonzero_row_padding,
};

pub(super) fn verify_candidate(source: &DecodedImage, candidate: &[u8]) -> Result<()> {
    let audit = inspect(candidate)?;
    if audit.animation.is_some() || audit.ancillary_or_trailing || audit.truecolor_palette {
        bail!("PNG output is not structurally sanitized");
    }

    let decoded = decode_static_raster(candidate, MAX_DECOMPRESSED_SIZE)
        .context("optimized PNG failed to decode")?;
    if decoded.interlaced {
        bail!("PNG output is still interlaced");
    }
    verify_sanitized_representation(&decoded.image, audit.saw_trns)?;
    drop(decoded);
    let decoded_pixels = decode_canonical(candidate)
        .context("failed to decode optimized PNG for canonical verification")?;
    compare_canonical_pixels(source, &decoded_pixels)
}

fn canonical_decoder(input: &[u8]) -> Decoder<Cursor<&[u8]>> {
    let mut decoder = Decoder::new_with_limits(
        Cursor::new(input),
        Limits {
            bytes: MAX_DECOMPRESSED_SIZE,
        },
    );
    decoder.set_transformations(Transformations::EXPAND);
    decoder
}

fn checked_output_buffer_size(size: Option<usize>) -> Result<usize> {
    size.filter(|size| *size <= MAX_DECOMPRESSED_SIZE)
        .ok_or_else(|| anyhow!("decoded PNG exceeds the decompressed size limit"))
}

pub(super) fn verify_apng_candidate(source: &[u8], candidate: &[u8]) -> Result<()> {
    let candidate_audit = inspect(candidate)?;
    if candidate_audit.animation.is_none() {
        bail!("optimized output is not an APNG");
    }
    if candidate_audit.metadata_cleanup_required() {
        bail!("APNG output is not structurally sanitized");
    }

    let mut source_reader = canonical_decoder(source)
        .read_info()
        .map_err(|error| anyhow!("source APNG decode failed: {error}"))?;
    let mut candidate_reader = canonical_decoder(candidate)
        .read_info()
        .map_err(|error| anyhow!("optimized APNG decode failed: {error}"))?;

    let source_info = source_reader.info();
    let candidate_info = candidate_reader.info();
    if source_info.width != candidate_info.width || source_info.height != candidate_info.height {
        bail!("APNG canvas dimensions changed during sanitation");
    }
    let source_control = source_info
        .animation_control
        .ok_or_else(|| anyhow!("source APNG decoder found no animation control"))?;
    let candidate_control = candidate_info
        .animation_control
        .ok_or_else(|| anyhow!("optimized APNG decoder found no animation control"))?;
    if source_control.num_frames != candidate_control.num_frames
        || source_control.num_plays != candidate_control.num_plays
    {
        bail!("APNG loop or frame count changed during sanitation");
    }
    let default_image_in_animation = source_info.frame_control.is_some();
    if default_image_in_animation != candidate_info.frame_control.is_some() {
        bail!("APNG default-image relationship changed during sanitation");
    }

    let source_buffer_size = checked_output_buffer_size(source_reader.output_buffer_size())
        .context("source APNG frame")?;
    let candidate_buffer_size = checked_output_buffer_size(candidate_reader.output_buffer_size())
        .context("optimized APNG frame")?;
    let mut source_buffer = vec![0; source_buffer_size];
    let mut candidate_buffer = vec![0; candidate_buffer_size];
    let image_count = source_control
        .num_frames
        .checked_add(u32::from(!default_image_in_animation))
        .ok_or_else(|| anyhow!("APNG image count overflow"))?;

    for frame_index in 0..image_count {
        let source_output = source_reader
            .next_frame(&mut source_buffer)
            .map_err(|error| anyhow!("source APNG frame {frame_index} decode failed: {error}"))?;
        let candidate_output =
            candidate_reader
                .next_frame(&mut candidate_buffer)
                .map_err(|error| {
                    anyhow!("optimized APNG frame {frame_index} decode failed: {error}")
                })?;
        compare_frame_control(
            frame_index,
            source_reader.info().frame_control.as_ref(),
            candidate_reader.info().frame_control.as_ref(),
        )?;
        compare_decoded_frames(
            frame_index,
            source_output,
            &source_buffer,
            candidate_output,
            &candidate_buffer,
        )?;
    }

    if source_reader.next_frame(&mut source_buffer).is_ok()
        || candidate_reader.next_frame(&mut candidate_buffer).is_ok()
    {
        bail!("APNG contains more images than declared");
    }
    Ok(())
}

fn compare_frame_control(
    frame_index: u32,
    before: Option<&FrameControl>,
    after: Option<&FrameControl>,
) -> Result<()> {
    let same = match (before, after) {
        (None, None) => true,
        (Some(before), Some(after)) => {
            before.width == after.width
                && before.height == after.height
                && before.x_offset == after.x_offset
                && before.y_offset == after.y_offset
                && before.delay_num == after.delay_num
                && before.delay_den == after.delay_den
                && before.dispose_op == after.dispose_op
                && before.blend_op == after.blend_op
        }
        _ => false,
    };
    if !same {
        bail!("APNG frame {frame_index} control changed during sanitation");
    }
    Ok(())
}

fn compare_decoded_frames(
    frame_index: u32,
    before_info: OutputInfo,
    before_data: &[u8],
    after_info: OutputInfo,
    after_data: &[u8],
) -> Result<()> {
    if before_info.width != after_info.width || before_info.height != after_info.height {
        bail!("APNG frame {frame_index} dimensions changed during sanitation");
    }
    validate_decoded_format(before_info.color_type, before_info.bit_depth)?;
    validate_decoded_format(after_info.color_type, after_info.bit_depth)?;

    let width = before_info.width as usize;
    let height = before_info.height as usize;
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| anyhow!("APNG frame {frame_index} dimensions overflow"))?;
    for pixel_index in 0..pixel_count {
        let x = pixel_index % width;
        let y = pixel_index / width;
        let before = canonical_output_pixel(&before_info, before_data, x, y);
        let after = canonical_output_pixel(&after_info, after_data, x, y);
        if before != after {
            bail!(
                "APNG frame {frame_index} canonical pixel {pixel_index} changed during sanitation"
            );
        }
    }
    Ok(())
}

fn canonical_output_pixel(info: &OutputInfo, data: &[u8], x: usize, y: usize) -> CanonicalPixel {
    canonical_pixel(info.color_type, info.bit_depth, info.line_size, data, x, y)
}

fn validate_decoded_format(color_type: DecodedColorType, bit_depth: DecodedBitDepth) -> Result<()> {
    if matches!(color_type, DecodedColorType::Indexed)
        || !matches!(bit_depth, DecodedBitDepth::Eight | DecodedBitDepth::Sixteen)
    {
        bail!("PNG expansion did not produce 8-bit or 16-bit direct-color samples");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CanonicalPixel {
    r: u16,
    g: u16,
    b: u16,
    a: u16,
}

pub(super) struct DecodedImage {
    width: usize,
    height: usize,
    color_type: DecodedColorType,
    bit_depth: DecodedBitDepth,
    line_size: usize,
    data: Vec<u8>,
}

pub(super) fn decode_canonical(input: &[u8]) -> Result<DecodedImage> {
    let mut reader = canonical_decoder(input)
        .read_info()
        .map_err(|error| anyhow!("PNG decode failed: {error}"))?;
    let buffer_size =
        checked_output_buffer_size(reader.output_buffer_size()).context("expanded PNG image")?;
    let mut data = vec![0; buffer_size];
    let info = reader
        .next_frame(&mut data)
        .map_err(|error| anyhow!("PNG pixel decode failed: {error}"))?;
    data.truncate(info.buffer_size());

    validate_decoded_format(info.color_type, info.bit_depth)?;
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
        canonical_pixel(
            self.color_type,
            self.bit_depth,
            self.line_size,
            &self.data,
            x,
            y,
        )
    }
}

fn canonical_pixel(
    color_type: DecodedColorType,
    bit_depth: DecodedBitDepth,
    line_size: usize,
    data: &[u8],
    x: usize,
    y: usize,
) -> CanonicalPixel {
    let sample = |channel| {
        let channels = decoded_channels(color_type);
        let byte_depth = decoded_byte_depth(bit_depth);
        let offset = y * line_size + (x * channels + channel) * byte_depth;
        if byte_depth == 1 {
            u16::from(data[offset]) * 257
        } else {
            u16::from_be_bytes([data[offset], data[offset + 1]])
        }
    };
    match color_type {
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

fn verify_sanitized_representation(image: &RasterImage, saw_trns: bool) -> Result<()> {
    if has_nonzero_row_padding(image)? {
        bail!("PNG output contains non-zero row padding bits");
    }

    let view = SampleView::new(image)?;
    match &image.color_type {
        RasterColorType::Indexed { palette } => {
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
        RasterColorType::Grayscale { transparent } => {
            if let Some(transparent) = transparent {
                let used = any_pixel(
                    image,
                    |view, x, y| Ok(view.sample(x, y, 0)? == *transparent),
                )?;
                if !used {
                    bail!("PNG output contains an unused grayscale tRNS value");
                }
            }
            if saw_trns != transparent.is_some() {
                bail!("PNG output contains an ineffective or missing grayscale tRNS chunk");
            }
        }
        RasterColorType::Rgb { transparent } => {
            if let Some(transparent) = transparent {
                let used = any_pixel(image, |view, x, y| {
                    Ok(view.sample(x, y, 0)? == transparent[0]
                        && view.sample(x, y, 1)? == transparent[1]
                        && view.sample(x, y, 2)? == transparent[2])
                })?;
                if !used {
                    bail!("PNG output contains an unused RGB tRNS value");
                }
            }
            if saw_trns != transparent.is_some() {
                bail!("PNG output contains an ineffective or missing RGB tRNS chunk");
            }
        }
        RasterColorType::GrayscaleAlpha | RasterColorType::Rgba => {
            if saw_trns {
                bail!("PNG output has tRNS in a color type with an alpha channel");
            }
        }
    }
    Ok(())
}

fn any_pixel(
    image: &RasterImage,
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
