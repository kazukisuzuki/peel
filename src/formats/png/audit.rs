use anyhow::{Result, anyhow, bail};

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
pub(super) struct AnimationAudit {
    pub(super) num_frames: u32,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FrameData {
    Missing,
    Idat,
    Fdat,
}

#[derive(Default)]
pub(super) struct PngAudit {
    pub(super) ancillary_or_trailing: bool,
    pub(super) animation: Option<AnimationAudit>,
    pub(super) truecolor_palette: bool,
    pub(super) saw_trns: bool,
}

impl PngAudit {
    pub(super) fn metadata_cleanup_required(&self) -> bool {
        self.ancillary_or_trailing || self.truecolor_palette
    }
}

pub(super) fn inspect(input: &[u8]) -> Result<PngAudit> {
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
