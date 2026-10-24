use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};
use png::{BitDepth, ColorType, Decoder, Limits, Transformations};

use super::MAX_DECOMPRESSED_SIZE;

#[derive(Clone)]
pub(super) struct RasterImage {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bit_depth: u8,
    pub(super) color_type: RasterColorType,
    pub(super) data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RasterColorType {
    Grayscale { transparent: Option<u16> },
    Rgb { transparent: Option<[u16; 3]> },
    Indexed { palette: Vec<Rgba8> },
    GrayscaleAlpha,
    Rgba,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Rgba8 {
    pub(super) r: u8,
    pub(super) g: u8,
    pub(super) b: u8,
    pub(super) a: u8,
}

pub(super) struct DecodedRaster {
    pub(super) image: RasterImage,
    pub(super) interlaced: bool,
}

impl RasterImage {
    pub(super) fn new(
        width: u32,
        height: u32,
        bit_depth: u8,
        color_type: RasterColorType,
        data: Vec<u8>,
    ) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("PNG raster dimensions must be non-zero");
        }
        validate_format(&color_type, bit_depth)?;

        let width = usize::try_from(width).context("PNG width does not fit in memory")?;
        let height = usize::try_from(height).context("PNG height does not fit in memory")?;
        let row_bytes = row_bytes(width, channels(&color_type), bit_depth)?;
        let expected = row_bytes
            .checked_mul(height)
            .filter(|size| *size <= MAX_DECOMPRESSED_SIZE)
            .ok_or_else(|| anyhow!("PNG raster exceeds the decompressed size limit"))?;
        if data.len() != expected {
            bail!(
                "PNG decoded sample length is {}, expected {expected}",
                data.len()
            );
        }

        match &color_type {
            RasterColorType::Indexed { palette } => {
                let max_entries = 1usize << bit_depth;
                if palette.is_empty() || palette.len() > 256 || palette.len() > max_entries {
                    bail!("PNG palette length is invalid for its bit depth");
                }
                for y in 0..height {
                    for x in 0..width {
                        let index = usize::from(packed_sample(&data, row_bytes, bit_depth, x, y));
                        if index >= palette.len() {
                            bail!("PNG palette index {index} is out of range");
                        }
                    }
                }
            }
            RasterColorType::Grayscale {
                transparent: Some(transparent),
            } if *transparent > sample_max(bit_depth) => {
                bail!("PNG grayscale transparency value exceeds its bit depth");
            }
            RasterColorType::Rgb {
                transparent: Some(transparent),
            } if transparent
                .iter()
                .any(|sample| *sample > sample_max(bit_depth)) =>
            {
                bail!("PNG RGB transparency value exceeds its bit depth");
            }
            _ => {}
        }

        Ok(Self {
            width: u32::try_from(width).expect("width came from u32"),
            height: u32::try_from(height).expect("height came from u32"),
            bit_depth,
            color_type,
            data,
        })
    }
}

pub(super) fn decode_static_raster(input: &[u8], limit: usize) -> Result<DecodedRaster> {
    let mut decoder = Decoder::new_with_limits(Cursor::new(input), Limits { bytes: limit });
    // IDENTITY preserves packed samples, palette indices, and tRNS for Peel's own normalization.
    decoder.set_transformations(Transformations::IDENTITY);
    let mut reader = decoder
        .read_info()
        .map_err(|error| anyhow!("PNG raster decode failed: {error}"))?;

    let (width, height, bit_depth, decoded_color_type, interlaced, palette, trns) = {
        let info = reader.info();
        if info.animation_control.is_some() || info.frame_control.is_some() {
            bail!("static PNG raster decoder received an APNG");
        }
        (
            info.width,
            info.height,
            depth_value(info.bit_depth),
            info.color_type,
            info.interlaced,
            info.palette.as_deref().map(<[u8]>::to_vec),
            info.trns.as_deref().map(<[u8]>::to_vec),
        )
    };

    let expected_row_bytes = row_bytes(
        usize::try_from(width).context("PNG width does not fit in memory")?,
        decoded_channels(decoded_color_type),
        bit_depth,
    )?;
    let expected_size = expected_row_bytes
        .checked_mul(usize::try_from(height).context("PNG height does not fit in memory")?)
        .filter(|size| *size <= limit && *size <= MAX_DECOMPRESSED_SIZE)
        .ok_or_else(|| anyhow!("decoded PNG exceeds the decompressed size limit"))?;
    let output_size = reader
        .output_buffer_size()
        .filter(|size| *size == expected_size)
        .ok_or_else(|| anyhow!("PNG decoder reported an invalid output buffer size"))?;
    let mut data = vec![0; output_size];
    let output = reader
        .next_frame(&mut data)
        .map_err(|error| anyhow!("PNG raster frame decode failed: {error}"))?;
    if output.width != width
        || output.height != height
        || output.color_type != decoded_color_type
        || depth_value(output.bit_depth) != bit_depth
        || output.line_size != expected_row_bytes
        || output.buffer_size() != expected_size
    {
        bail!("PNG decoder output format did not match the source header");
    }
    data.truncate(output.buffer_size());

    let color_type = decode_color_type(
        decoded_color_type,
        bit_depth,
        palette.as_deref(),
        trns.as_deref(),
    )?;
    let image = RasterImage::new(width, height, bit_depth, color_type, data)?;
    Ok(DecodedRaster { image, interlaced })
}

fn decode_color_type(
    color_type: ColorType,
    bit_depth: u8,
    palette: Option<&[u8]>,
    trns: Option<&[u8]>,
) -> Result<RasterColorType> {
    match color_type {
        ColorType::Grayscale => Ok(RasterColorType::Grayscale {
            transparent: trns
                .map(|bytes| decode_grayscale_trns(bytes, bit_depth))
                .transpose()?,
        }),
        ColorType::Rgb => Ok(RasterColorType::Rgb {
            transparent: trns
                .map(|bytes| decode_rgb_trns(bytes, bit_depth))
                .transpose()?,
        }),
        ColorType::Indexed => {
            let palette = palette.ok_or_else(|| anyhow!("indexed PNG has no palette"))?;
            if palette.is_empty() || palette.len() > 256 * 3 || palette.len() % 3 != 0 {
                bail!("indexed PNG palette has an invalid length");
            }
            let entries = palette.len() / 3;
            let trns = trns.unwrap_or_default();
            if trns.len() > entries {
                bail!("indexed PNG tRNS is longer than its palette");
            }
            let palette = palette
                .as_chunks::<3>()
                .0
                .iter()
                .enumerate()
                .map(|(index, rgb)| Rgba8 {
                    r: rgb[0],
                    g: rgb[1],
                    b: rgb[2],
                    a: trns.get(index).copied().unwrap_or(u8::MAX),
                })
                .collect();
            Ok(RasterColorType::Indexed { palette })
        }
        ColorType::GrayscaleAlpha => {
            if trns.is_some() {
                bail!("grayscale-alpha PNG must not contain tRNS");
            }
            Ok(RasterColorType::GrayscaleAlpha)
        }
        ColorType::Rgba => {
            if trns.is_some() {
                bail!("RGBA PNG must not contain tRNS");
            }
            Ok(RasterColorType::Rgba)
        }
    }
}

fn decode_grayscale_trns(bytes: &[u8], bit_depth: u8) -> Result<u16> {
    if bit_depth < 16 {
        let [transparent] = bytes else {
            bail!("decoded grayscale PNG tRNS must contain exactly one sample");
        };
        Ok(u16::from(*transparent))
    } else {
        let bytes: [u8; 2] = bytes
            .try_into()
            .map_err(|_| anyhow!("16-bit grayscale PNG tRNS must contain exactly two bytes"))?;
        Ok(u16::from_be_bytes(bytes))
    }
}

fn decode_rgb_trns(bytes: &[u8], bit_depth: u8) -> Result<[u16; 3]> {
    if bit_depth < 16 {
        let [r, g, b] = bytes else {
            bail!("decoded RGB PNG tRNS must contain exactly three samples");
        };
        return Ok([u16::from(*r), u16::from(*g), u16::from(*b)]);
    }
    if bytes.len() != 6 {
        bail!("16-bit RGB PNG tRNS must contain exactly six bytes");
    }
    Ok([
        u16::from_be_bytes([bytes[0], bytes[1]]),
        u16::from_be_bytes([bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
    ])
}

fn validate_format(color_type: &RasterColorType, bit_depth: u8) -> Result<()> {
    let valid = match color_type {
        RasterColorType::Grayscale { .. } => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        RasterColorType::Indexed { .. } => matches!(bit_depth, 1 | 2 | 4 | 8),
        RasterColorType::Rgb { .. } | RasterColorType::GrayscaleAlpha | RasterColorType::Rgba => {
            matches!(bit_depth, 8 | 16)
        }
    };
    if !valid {
        bail!("invalid PNG color type and bit depth combination");
    }
    Ok(())
}

fn row_bytes(width: usize, channels: usize, bit_depth: u8) -> Result<usize> {
    width
        .checked_mul(channels)
        .and_then(|samples| samples.checked_mul(usize::from(bit_depth)))
        .and_then(|bits| bits.checked_add(7))
        .map(|bits| bits / 8)
        .ok_or_else(|| anyhow!("PNG row size overflow"))
}

pub(super) fn channels(color_type: &RasterColorType) -> usize {
    match color_type {
        RasterColorType::Grayscale { .. } | RasterColorType::Indexed { .. } => 1,
        RasterColorType::GrayscaleAlpha => 2,
        RasterColorType::Rgb { .. } => 3,
        RasterColorType::Rgba => 4,
    }
}

pub(super) fn decoded_channels(color_type: ColorType) -> usize {
    match color_type {
        ColorType::Grayscale | ColorType::Indexed => 1,
        ColorType::GrayscaleAlpha => 2,
        ColorType::Rgb => 3,
        ColorType::Rgba => 4,
    }
}

fn depth_value(bit_depth: BitDepth) -> u8 {
    bit_depth as u8
}

fn packed_sample(data: &[u8], row_bytes: usize, depth: u8, x: usize, y: usize) -> u8 {
    let bit_offset = x * usize::from(depth);
    let byte = data[y * row_bytes + bit_offset / 8];
    let shift = 8 - usize::from(depth) - bit_offset % 8;
    (byte >> shift) & ((1u8 << depth) - 1)
}

pub(super) fn sample_max(depth: u8) -> u16 {
    if depth == 16 {
        u16::MAX
    } else {
        (1u16 << depth) - 1
    }
}

#[derive(Clone, Copy)]
pub(super) struct SampleView<'a> {
    pub(super) image: &'a RasterImage,
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) channels: usize,
    pub(super) depth: u8,
    pub(super) row_bytes: usize,
}

impl<'a> SampleView<'a> {
    pub(super) fn new(image: &'a RasterImage) -> Result<Self> {
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

    pub(super) fn sample(self, x: usize, y: usize, channel: usize) -> Result<u16> {
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

pub(super) fn has_nonzero_row_padding(image: &RasterImage) -> Result<bool> {
    let view = SampleView::new(image)?;
    let used_bits = view.width * view.channels * usize::from(view.depth);
    let padding_bits = (8 - used_bits % 8) % 8;
    if padding_bits == 0 || view.row_bytes == 0 {
        return Ok(false);
    }
    let mask = (1u8 << padding_bits) - 1;
    Ok((0..view.height).any(|row| image.data[(row + 1) * view.row_bytes - 1] & mask != 0))
}

pub(super) fn zero_row_padding(image: &mut RasterImage) -> Result<()> {
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

pub(super) fn write_packed_sample(
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

pub(super) fn decoded_byte_depth(bit_depth: BitDepth) -> usize {
    match bit_depth {
        BitDepth::Eight => 1,
        BitDepth::Sixteen => 2,
        _ => unreachable!("canonical decoding rejects sub-byte output"),
    }
}
#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    #[test]
    fn decodes_identity_color_types_and_transparency() {
        let mut grayscale = png::Info::with_size(1, 1);
        grayscale.color_type = ColorType::Grayscale;
        grayscale.bit_depth = BitDepth::Eight;
        grayscale.trns = Some(Cow::Owned(vec![0, 7]));

        let mut rgb = png::Info::with_size(1, 1);
        rgb.color_type = ColorType::Rgb;
        rgb.bit_depth = BitDepth::Eight;
        rgb.trns = Some(Cow::Owned(vec![0, 1, 0, 2, 0, 3]));

        let mut grayscale_16 = png::Info::with_size(1, 1);
        grayscale_16.color_type = ColorType::Grayscale;
        grayscale_16.bit_depth = BitDepth::Sixteen;
        grayscale_16.trns = Some(Cow::Owned(vec![0x12, 0x34]));

        let mut rgb_16 = png::Info::with_size(1, 1);
        rgb_16.color_type = ColorType::Rgb;
        rgb_16.bit_depth = BitDepth::Sixteen;
        rgb_16.trns = Some(Cow::Owned(vec![0, 1, 0, 2, 0, 3]));

        let mut indexed = png::Info::with_size(2, 1);
        indexed.color_type = ColorType::Indexed;
        indexed.bit_depth = BitDepth::One;
        indexed.palette = Some(Cow::Owned(vec![10, 20, 30, 40, 50, 60]));
        indexed.trns = Some(Cow::Owned(vec![4]));

        let mut grayscale_alpha = png::Info::with_size(1, 1);
        grayscale_alpha.color_type = ColorType::GrayscaleAlpha;
        grayscale_alpha.bit_depth = BitDepth::Eight;

        let mut rgba = png::Info::with_size(1, 1);
        rgba.color_type = ColorType::Rgba;
        rgba.bit_depth = BitDepth::Sixteen;

        let cases = [
            (
                "grayscale",
                grayscale,
                vec![7],
                RasterColorType::Grayscale {
                    transparent: Some(7),
                },
            ),
            (
                "rgb",
                rgb,
                vec![1, 2, 3],
                RasterColorType::Rgb {
                    transparent: Some([1, 2, 3]),
                },
            ),
            (
                "16-bit grayscale",
                grayscale_16,
                vec![0x12, 0x34],
                RasterColorType::Grayscale {
                    transparent: Some(0x1234),
                },
            ),
            (
                "16-bit rgb",
                rgb_16,
                vec![0, 1, 0, 2, 0, 3],
                RasterColorType::Rgb {
                    transparent: Some([1, 2, 3]),
                },
            ),
            (
                "indexed",
                indexed,
                vec![0x40],
                RasterColorType::Indexed {
                    palette: vec![
                        Rgba8 {
                            r: 10,
                            g: 20,
                            b: 30,
                            a: 4,
                        },
                        Rgba8 {
                            r: 40,
                            g: 50,
                            b: 60,
                            a: u8::MAX,
                        },
                    ],
                },
            ),
            (
                "grayscale alpha",
                grayscale_alpha,
                vec![3, 4],
                RasterColorType::GrayscaleAlpha,
            ),
            (
                "rgba",
                rgba,
                vec![0, 1, 0, 2, 0, 3, 0, 4],
                RasterColorType::Rgba,
            ),
        ];

        for (name, info, data, expected_color_type) in cases {
            let encoded = encode(info, &data);
            let decoded = decode_static_raster(&encoded, MAX_DECOMPRESSED_SIZE)
                .unwrap_or_else(|error| panic!("{name}: {error:#}"));
            assert!(!decoded.interlaced, "{name}");
            assert_eq!(decoded.image.color_type, expected_color_type, "{name}");
            assert_eq!(decoded.image.data, data, "{name}");
        }
    }

    #[test]
    fn rejects_owned_raster_invariant_violations() {
        assert!(
            RasterImage::new(
                0,
                1,
                8,
                RasterColorType::Grayscale { transparent: None },
                vec![]
            )
            .is_err()
        );
        assert!(
            RasterImage::new(1, 1, 4, RasterColorType::Rgb { transparent: None }, vec![0]).is_err()
        );
        assert!(
            RasterImage::new(
                1,
                1,
                1,
                RasterColorType::Indexed {
                    palette: vec![Rgba8 {
                        r: 0,
                        g: 0,
                        b: 0,
                        a: u8::MAX,
                    }],
                },
                vec![0x80]
            )
            .is_err()
        );
        assert!(
            RasterImage::new(
                1,
                1,
                4,
                RasterColorType::Grayscale {
                    transparent: Some(16),
                },
                vec![0]
            )
            .is_err()
        );
    }

    fn encode(info: png::Info<'static>, data: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        {
            let encoder = png::Encoder::with_info(&mut encoded, info).unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        encoded
    }
}
