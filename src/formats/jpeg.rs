use anyhow::{Context, Result, bail};

use super::{Candidate, Change, FormatOutcome, choose_candidate};
use crate::mozjpeg;

pub(super) fn optimize(input: &[u8]) -> Result<FormatOutcome> {
    let cleanup_required = inspect(input)?;
    let bytes = mozjpeg::transcode(input).context("failed to create progressive JPEG")?;
    let mut changes = vec![Change::Progressive, Change::Recompressed];
    if cleanup_required {
        changes.push(Change::MetadataRemoved);
    }

    choose_candidate(
        input,
        cleanup_required,
        vec![Candidate { bytes, changes }],
        |candidate| {
            if inspect(candidate)? {
                bail!("JPEG output still contains metadata or trailing bytes");
            }
            Ok(())
        },
    )
}

fn inspect(input: &[u8]) -> Result<bool> {
    if !input.starts_with(&[0xff, 0xd8]) {
        bail!("invalid JPEG signature");
    }

    let mut position = 2;
    let mut metadata_found = false;

    loop {
        if input.get(position) != Some(&0xff) {
            bail!("invalid JPEG marker at byte {position}");
        }
        while input.get(position) == Some(&0xff) {
            position += 1;
        }
        let marker = *input
            .get(position)
            .ok_or_else(|| anyhow::anyhow!("truncated JPEG marker"))?;
        position += 1;

        if marker == 0xd9 {
            return Ok(metadata_found || position != input.len());
        }
        if marker == 0x00 || marker == 0xd8 {
            bail!("invalid JPEG marker 0x{marker:02x}");
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }

        let length_bytes = input
            .get(position..position + 2)
            .ok_or_else(|| anyhow::anyhow!("truncated JPEG segment length"))?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            bail!("invalid JPEG segment length");
        }
        let segment_end = position
            .checked_add(length)
            .filter(|end| *end <= input.len())
            .ok_or_else(|| anyhow::anyhow!("truncated JPEG segment"))?;

        if (0xe0..=0xef).contains(&marker) || marker == 0xfe {
            metadata_found = true;
        }

        position = segment_end;
        if marker != 0xda {
            continue;
        }

        loop {
            let relative = input[position..]
                .iter()
                .position(|byte| *byte == 0xff)
                .ok_or_else(|| anyhow::anyhow!("JPEG scan has no EOI marker"))?;
            let scan_marker_start = position + relative;
            position = scan_marker_start + 1;
            while input.get(position) == Some(&0xff) {
                position += 1;
            }
            let scan_marker = *input
                .get(position)
                .ok_or_else(|| anyhow::anyhow!("truncated JPEG scan marker"))?;
            if scan_marker == 0x00 || (0xd0..=0xd7).contains(&scan_marker) {
                position += 1;
                continue;
            }
            position = scan_marker_start;
            break;
        }
    }
}
