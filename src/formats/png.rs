use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};
use oxipng::{
    BitDepth as EncodedBitDepth, ColorType as EncodedColorType, Options, RGB16, RGBA8, RawImage,
    StripChunks,
};
use png::{
    BitDepth as DecodedBitDepth, ColorType as DecodedColorType, Decoder, FrameControl, Limits,
    OutputInfo, Transformations,
};

use super::{Candidate, Change, FormatOutcome};
use raster::{RasterColorType, RasterImage, decode_static_raster};

mod raster;

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_DECOMPRESSED_SIZE: usize = 512 * 1024 * 1024;

pub(super) fn optimize(input: &[u8]) -> Result<FormatOutcome> {
    let audit = inspect(input)?;
    if audit.animation.is_some() {
        return optimize_apng(input, audit.metadata_cleanup_required());
    }

    let options = options();
    let source = decode_canonical(input)
        .context("failed to decode source PNG for canonical verification")?;
    let preliminary = oxipng::optimize_from_memory(input, &options)
        .map_err(|error| anyhow!("PNG normalization failed: {error}"))?;
    let decoded = decode_static_raster(&preliminary, MAX_DECOMPRESSED_SIZE)
        .context("normalized PNG failed to decode")?;
    if decoded.interlaced {
        bail!("oxipng did not produce the requested non-interlaced PNG");
    }
    let baseline = normalize_baseline(decoded.image)?;

    let outputs = encode_candidates(&baseline, &options)?;
    let selected = select_candidate(&source, outputs)?;
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

fn optimize_apng(input: &[u8], cleanup_required: bool) -> Result<FormatOutcome> {
    let output = oxipng::optimize_from_memory(input, &apng_options())
        .map_err(|error| anyhow!("APNG normalization failed: {error}"))?;
    verify_apng_candidate(input, &output)?;

    if output == input || (!cleanup_required && output.len() >= input.len()) {
        return Ok(FormatOutcome::Unchanged);
    }

    let mut changes = vec![Change::Recompressed, Change::ContainerNormalized];
    if cleanup_required {
        changes.push(Change::MetadataRemoved);
    }
    Ok(FormatOutcome::Candidate(Candidate {
        bytes: output,
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

fn apng_options() -> Options {
    let mut options = Options::from_preset(2);
    options.strip = StripChunks::Keep(oxipng::indexset![*b"acTL", *b"fcTL", *b"fdAT"]);
    options.interlace = None;
    options.bit_depth_reduction = false;
    options.color_type_reduction = false;
    options.palette_reduction = false;
    options.grayscale_reduction = false;
    options.scale_16 = false;
    options.optimize_alpha = false;
    options.idat_recoding = true;
    options.fix_errors = false;
    options.force = true;
    options.max_decompressed_size = Some(MAX_DECOMPRESSED_SIZE);
    options
}

fn normalize_baseline(mut image: RasterImage) -> Result<RasterImage> {
    compact_indexed_palette(&mut image)?;
    remove_unused_direct_trns(&mut image)?;
    zero_row_padding(&mut image)?;
    Ok(image)
}

fn encode_candidates(source: &RasterImage, options: &Options) -> Result<Vec<Vec<u8>>> {
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

fn select_candidate(source: &DecodedImage, outputs: Vec<Vec<u8>>) -> Result<Vec<u8>> {
    let mut selected: Option<Vec<u8>> = None;
    let mut last_error = None;
    for output in outputs {
        match verify_candidate(source, &output) {
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

fn push_encoded(outputs: &mut Vec<Vec<u8>>, image: RasterImage, options: &Options) -> Result<()> {
    let color_type = match image.color_type {
        RasterColorType::Grayscale { transparent } => EncodedColorType::Grayscale {
            transparent_shade: transparent,
        },
        RasterColorType::Rgb { transparent } => EncodedColorType::RGB {
            transparent_color: transparent.map(|[r, g, b]| RGB16::new(r, g, b)),
        },
        RasterColorType::Indexed { palette } => EncodedColorType::Indexed {
            palette: palette
                .into_iter()
                .map(|color| RGBA8::new(color.r, color.g, color.b, color.a))
                .collect(),
        },
        RasterColorType::GrayscaleAlpha => EncodedColorType::GrayscaleAlpha,
        RasterColorType::Rgba => EncodedColorType::RGBA,
    };
    let bit_depth = EncodedBitDepth::try_from(image.bit_depth)
        .map_err(|error| anyhow!("failed to convert PNG bit depth: {error}"))?;
    // RawImage is the public oxipng boundary for compressing Peel-owned raster data.
    let raw = RawImage::new(image.width, image.height, color_type, bit_depth, image.data)
        .map_err(|error| anyhow!("failed to construct sanitized PNG: {error}"))?;
    let output = raw
        .create_optimized_png(options)
        .map_err(|error| anyhow!("PNG recompression failed: {error}"))?;
    if !outputs.contains(&output) {
        outputs.push(output);
    }
    Ok(())
}

fn verify_candidate(source: &DecodedImage, candidate: &[u8]) -> Result<()> {
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

fn verify_apng_candidate(source: &[u8], candidate: &[u8]) -> Result<()> {
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

fn compact_indexed_palette(image: &mut RasterImage) -> Result<()> {
    let RasterColorType::Indexed { palette } = image.color_type.clone() else {
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
    image.color_type = RasterColorType::Indexed { palette: compact };
    image.data = data;
    Ok(())
}

fn indexed_to_direct(image: &RasterImage) -> Result<Option<RasterImage>> {
    let RasterColorType::Indexed { palette } = &image.color_type else {
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
        (true, true) => RasterColorType::GrayscaleAlpha,
        (true, false) => RasterColorType::Grayscale { transparent: None },
        (false, true) => RasterColorType::Rgba,
        (false, false) => RasterColorType::Rgb { transparent: None },
    };
    let mut direct = image.clone();
    direct.color_type = color_type;
    direct.bit_depth = 8;
    direct.data = data;
    Ok(Some(direct))
}

fn exact_alpha_to_trns(image: &RasterImage) -> Result<Option<RasterImage>> {
    let color_channels = match image.color_type {
        RasterColorType::GrayscaleAlpha => 1,
        RasterColorType::Rgba => 3,
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
        RasterColorType::Grayscale {
            transparent: Some(transparent_color[0]),
        }
    } else {
        RasterColorType::Rgb {
            transparent: Some([
                transparent_color[0],
                transparent_color[1],
                transparent_color[2],
            ]),
        }
    };
    let mut reduced = image.clone();
    reduced.color_type = color_type;
    reduced.data = data;
    Ok(Some(reduced))
}

fn remove_unused_direct_trns(image: &mut RasterImage) -> Result<()> {
    let view = SampleView::new(image)?;
    let replacement = match image.color_type.clone() {
        RasterColorType::Grayscale {
            transparent: Some(transparent),
        } => {
            let mut used = false;
            for y in 0..view.height {
                for x in 0..view.width {
                    used |= view.sample(x, y, 0)? == transparent;
                }
            }
            (!used).then_some(RasterColorType::Grayscale { transparent: None })
        }
        RasterColorType::Rgb {
            transparent: Some(transparent),
        } => {
            let mut used = false;
            for y in 0..view.height {
                for x in 0..view.width {
                    used |= view.sample(x, y, 0)? == transparent[0]
                        && view.sample(x, y, 1)? == transparent[1]
                        && view.sample(x, y, 2)? == transparent[2];
                }
            }
            (!used).then_some(RasterColorType::Rgb { transparent: None })
        }
        _ => None,
    };
    if let Some(color_type) = replacement {
        image.color_type = color_type;
    }
    Ok(())
}

struct AnimationAudit {
    num_frames: u32,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FrameData {
    Missing,
    Idat,
    Fdat,
}

#[derive(Default)]
struct PngAudit {
    ancillary_or_trailing: bool,
    animation: Option<AnimationAudit>,
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
    let mut canvas = None;
    let mut color_type = None;
    let mut saw_idat = false;
    let mut idat_ended = false;
    let mut expected_sequence = 0u64;
    let mut frame_count = 0u32;
    let mut current_frame_data = None;

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
            let width = u32::from_be_bytes(data[0..4].try_into().unwrap());
            let height = u32::from_be_bytes(data[4..8].try_into().unwrap());
            if width == 0 || height == 0 {
                bail!("PNG canvas dimensions must be non-zero");
            }
            canvas = Some((width, height));
            color_type = Some(data[9]);
        } else if kind == *b"PLTE" && matches!(color_type, Some(2 | 6)) {
            audit.truecolor_palette = true;
        }
        if kind == *b"tRNS" {
            audit.saw_trns = true;
        }
        if saw_idat && kind != *b"IDAT" {
            idat_ended = true;
        }
        match &kind {
            b"acTL" => {
                if audit.animation.is_some() || data_length != 8 || saw_idat {
                    bail!("invalid, duplicate, or misplaced APNG acTL chunk");
                }
                let num_frames = u32::from_be_bytes(data[0..4].try_into().unwrap());
                if num_frames == 0 {
                    bail!("APNG must declare at least one animation frame");
                }
                audit.animation = Some(AnimationAudit { num_frames });
            }
            b"fcTL" => {
                if audit.animation.is_none() || data_length != 26 {
                    bail!("APNG fcTL requires a preceding acTL chunk and length 26");
                }
                if current_frame_data == Some(FrameData::Missing) {
                    bail!("APNG frame has no image data");
                }
                let sequence = u32::from_be_bytes(data[0..4].try_into().unwrap());
                if u64::from(sequence) != expected_sequence {
                    bail!("APNG sequence number is not contiguous");
                }
                expected_sequence += 1;
                let width = u32::from_be_bytes(data[4..8].try_into().unwrap());
                let height = u32::from_be_bytes(data[8..12].try_into().unwrap());
                let x_offset = u32::from_be_bytes(data[12..16].try_into().unwrap());
                let y_offset = u32::from_be_bytes(data[16..20].try_into().unwrap());
                let (canvas_width, canvas_height) = canvas.unwrap();
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
                if data[24] > 2 || data[25] > 1 {
                    bail!("APNG frame has an invalid dispose or blend operation");
                }
                if !saw_idat
                    && (frame_count != 0
                        || width != canvas_width
                        || height != canvas_height
                        || x_offset != 0
                        || y_offset != 0)
                {
                    bail!("APNG default-image frame must cover the canvas");
                }
                frame_count = frame_count
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("APNG frame count overflow"))?;
                current_frame_data = Some(FrameData::Missing);
            }
            b"IDAT" => {
                if idat_ended {
                    bail!("PNG IDAT chunks must be consecutive");
                }
                saw_idat = true;
                match current_frame_data {
                    Some(FrameData::Missing | FrameData::Idat) => {
                        current_frame_data = Some(FrameData::Idat);
                    }
                    Some(FrameData::Fdat) => bail!("APNG frame mixes IDAT and fdAT image data"),
                    None => {}
                }
            }
            b"fdAT" => {
                if audit.animation.is_none() || !saw_idat || data_length < 4 {
                    bail!("invalid or misplaced APNG fdAT chunk");
                }
                match current_frame_data {
                    Some(FrameData::Missing | FrameData::Fdat) => {}
                    Some(FrameData::Idat) => bail!("APNG frame mixes IDAT and fdAT image data"),
                    None => bail!("APNG fdAT chunk has no corresponding frame control"),
                }
                let sequence = u32::from_be_bytes(data[0..4].try_into().unwrap());
                if u64::from(sequence) != expected_sequence {
                    bail!("APNG sequence number is not contiguous");
                }
                expected_sequence += 1;
                current_frame_data = Some(FrameData::Fdat);
            }
            _ => {}
        }
        let ancillary = kind[0] & 0x20 != 0;
        if ancillary && !matches!(&kind, b"tRNS" | b"acTL" | b"fcTL" | b"fdAT") {
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
            if !saw_idat {
                bail!("PNG is missing IDAT");
            }
            if current_frame_data == Some(FrameData::Missing) {
                bail!("APNG frame has no image data");
            }
            if let Some(animation) = &audit.animation
                && frame_count != animation.num_frames
            {
                bail!("APNG acTL frame count does not match fcTL chunks");
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

#[derive(Clone, Copy)]
struct SampleView<'a> {
    image: &'a RasterImage,
    width: usize,
    height: usize,
    channels: usize,
    depth: u8,
    row_bytes: usize,
}

impl<'a> SampleView<'a> {
    fn new(image: &'a RasterImage) -> Result<Self> {
        let width = image.width as usize;
        let height = image.height as usize;
        let channels = channels(&image.color_type);
        let depth = image.bit_depth;
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

fn has_nonzero_row_padding(image: &RasterImage) -> Result<bool> {
    let view = SampleView::new(image)?;
    let used_bits = view.width * view.channels * usize::from(view.depth);
    let padding_bits = (8 - used_bits % 8) % 8;
    if padding_bits == 0 || view.row_bytes == 0 {
        return Ok(false);
    }
    let mask = (1u8 << padding_bits) - 1;
    Ok((0..view.height).any(|row| image.data[(row + 1) * view.row_bytes - 1] & mask != 0))
}

fn zero_row_padding(image: &mut RasterImage) -> Result<()> {
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

fn channels(color_type: &RasterColorType) -> usize {
    match color_type {
        RasterColorType::Grayscale { .. } | RasterColorType::Indexed { .. } => 1,
        RasterColorType::GrayscaleAlpha => 2,
        RasterColorType::Rgb { .. } => 3,
        RasterColorType::Rgba => 4,
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
