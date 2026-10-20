use anyhow::{Result, anyhow, bail};
use oxipng::internal_tests::PngData;
use oxipng::{Options, StripChunks};

use super::{Candidate, Change, FormatOutcome, choose_candidate};

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_DECOMPRESSED_SIZE: usize = 512 * 1024 * 1024;

pub(super) fn optimize(input: &[u8]) -> Result<FormatOutcome> {
    let audit = inspect(input)?;
    if audit.apng {
        bail!("UnsupportedApngWithStripAll: APNG cannot be optimized without removing animation");
    }

    let cleanup_options = options(false, true);
    let original = PngData::from_slice(input, &cleanup_options)
        .map_err(|error| anyhow!("invalid or unsupported PNG: {error}"))?;
    let cleanup_required = audit.cleanup_required;
    let mut candidates = Vec::with_capacity(2);

    if cleanup_required {
        let bytes = oxipng::optimize_from_memory(input, &cleanup_options)
            .map_err(|error| anyhow!("PNG metadata removal failed: {error}"))?;
        candidates.push(Candidate {
            bytes,
            changes: vec![Change::MetadataRemoved, Change::ContainerNormalized],
        });
    }

    let optimized = oxipng::optimize_from_memory(input, &options(true, false))
        .map_err(|error| anyhow!("PNG recompression failed: {error}"))?;
    let mut changes = vec![Change::Recompressed];
    if cleanup_required {
        changes.push(Change::MetadataRemoved);
    }
    candidates.push(Candidate {
        bytes: optimized,
        changes,
    });

    choose_candidate(input, cleanup_required, candidates, |candidate| {
        let candidate_audit = inspect(candidate)?;
        if candidate_audit.apng || candidate_audit.cleanup_required {
            bail!("PNG output still contains ancillary chunks or trailing bytes");
        }

        let decoded = PngData::from_slice(candidate, &cleanup_options)
            .map_err(|error| anyhow!("optimized PNG failed to decode: {error}"))?;
        let before = &original.raw;
        let after = &decoded.raw;
        if before.ihdr.width != after.ihdr.width
            || before.ihdr.height != after.ihdr.height
            || before.ihdr.color_type != after.ihdr.color_type
            || before.ihdr.bit_depth != after.ihdr.bit_depth
            || before.ihdr.interlaced != after.ihdr.interlaced
            || before.data != after.data
        {
            bail!("PNG sample, palette, alpha, or IHDR representation changed");
        }
        Ok(())
    })
}

fn options(idat_recoding: bool, force: bool) -> Options {
    let mut options = Options::from_preset(2);
    options.interlace = None;
    options.optimize_alpha = false;
    options.bit_depth_reduction = false;
    options.color_type_reduction = false;
    options.palette_reduction = false;
    options.grayscale_reduction = false;
    options.scale_16 = false;
    options.idat_recoding = idat_recoding;
    options.fix_errors = false;
    options.force = force;
    options.strip = StripChunks::All;
    options.max_decompressed_size = Some(MAX_DECOMPRESSED_SIZE);
    options
}

struct PngAudit {
    cleanup_required: bool,
    apng: bool,
}

fn inspect(input: &[u8]) -> Result<PngAudit> {
    if !input.starts_with(SIGNATURE) {
        bail!("invalid PNG signature");
    }

    let mut position = SIGNATURE.len();
    let mut removable_chunks = false;
    let mut apng = false;
    let mut saw_ihdr = false;

    while position < input.len() {
        let header = input
            .get(position..position + 8)
            .ok_or_else(|| anyhow!("truncated PNG chunk header"))?;
        let data_length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = header[4..8].try_into().unwrap();
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            bail!("invalid PNG chunk type");
        }
        let end = position
            .checked_add(12)
            .and_then(|value| value.checked_add(data_length))
            .filter(|end| *end <= input.len())
            .ok_or_else(|| anyhow!("truncated PNG chunk"))?;

        if !saw_ihdr && kind != *b"IHDR" {
            bail!("PNG IHDR must be the first chunk");
        }
        if kind == *b"IHDR" {
            if saw_ihdr || data_length != 13 {
                bail!("invalid or duplicate PNG IHDR");
            }
            saw_ihdr = true;
        }
        if matches!(&kind, b"acTL" | b"fcTL" | b"fdAT") {
            apng = true;
        }

        let ancillary = kind[0] & 0x20 != 0;
        if ancillary && kind != *b"tRNS" {
            removable_chunks = true;
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
            return Ok(PngAudit {
                cleanup_required: removable_chunks || position != input.len(),
                apng,
            });
        }
    }

    bail!("PNG is missing IEND")
}
