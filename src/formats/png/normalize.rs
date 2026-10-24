use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use oxipng::{
    BitDepth as EncodedBitDepth, ColorType as EncodedColorType, Options, RGB16, RGBA8, RawImage,
};

use super::MAX_DECOMPRESSED_SIZE;
use super::raster::{
    RasterColorType, RasterImage, SampleView, sample_max, write_packed_sample, zero_row_padding,
};

pub(super) fn normalize_baseline(mut image: RasterImage) -> Result<RasterImage> {
    compact_indexed_palette(&mut image)?;
    remove_unused_direct_trns(&mut image)?;
    zero_row_padding(&mut image)?;
    Ok(image)
}

pub(super) fn encode_best_candidate(
    source: RasterImage,
    options: &Options,
    verify: impl FnMut(&[u8]) -> Result<()>,
) -> Result<Vec<u8>> {
    let mut selector = CandidateSelector::new(options, verify);

    if let Some(transparency_variant) = exact_alpha_to_trns(&source)? {
        selector.consider(transparency_variant)?;
    }
    if let Some(direct_variant) = indexed_to_direct(&source)? {
        if let Some(transparency_variant) = exact_alpha_to_trns(&direct_variant)? {
            selector.consider(transparency_variant)?;
        }
        selector.consider(direct_variant)?;
    }
    selector.consider(source)?;
    selector.finish()
}

struct CandidateSelector<'a, F> {
    options: &'a Options,
    verify: F,
    selected: Option<Vec<u8>>,
    last_error: Option<anyhow::Error>,
}

impl<'a, F> CandidateSelector<'a, F>
where
    F: FnMut(&[u8]) -> Result<()>,
{
    fn new(options: &'a Options, verify: F) -> Self {
        Self {
            options,
            verify,
            selected: None,
            last_error: None,
        }
    }

    fn consider(&mut self, image: RasterImage) -> Result<()> {
        let output = encode(image, self.options)?;
        if self.selected.as_deref() == Some(output.as_slice()) {
            return Ok(());
        }

        match (self.verify)(&output) {
            Ok(()) => {
                if self
                    .selected
                    .as_ref()
                    .is_none_or(|current| output.len() < current.len())
                {
                    self.selected = Some(output);
                }
            }
            Err(error) => self.last_error = Some(error),
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>> {
        if let Some(selected) = self.selected {
            return Ok(selected);
        }
        if let Some(error) = self.last_error {
            return Err(error).context("no sanitized PNG candidate passed verification");
        }
        bail!("no sanitized PNG candidate was produced")
    }
}

fn encode(image: RasterImage, options: &Options) -> Result<Vec<u8>> {
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
    let raw = RawImage::new(image.width, image.height, color_type, bit_depth, image.data)
        .map_err(|error| anyhow!("failed to construct sanitized PNG: {error}"))?;
    raw.create_optimized_png(options)
        .map_err(|error| anyhow!("PNG recompression failed: {error}"))
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
    Ok(Some(RasterImage {
        width: image.width,
        height: image.height,
        bit_depth: 8,
        color_type,
        data,
    }))
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
    Ok(Some(RasterImage {
        width: image.width,
        height: image.height,
        bit_depth: image.bit_depth,
        color_type,
        data,
    }))
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
