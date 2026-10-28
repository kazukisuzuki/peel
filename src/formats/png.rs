use anyhow::{Context, Result, anyhow, bail};
use oxipng::{Options, StripChunks};

use super::{Candidate, Change, FormatOutcome};
use audit::inspect;
use raster::decode_static_raster;
use verify::{decode_canonical, verify_apng_candidate, verify_candidate};

mod audit;
mod normalize;
mod raster;
mod verify;

const MAX_DECOMPRESSED_SIZE: usize = 128 * 1024 * 1024;

pub(super) fn optimize(input: &[u8]) -> Result<FormatOutcome> {
    let audit = inspect(input)?;
    if audit.is_animated {
        return optimize_apng(input, audit.metadata_cleanup_required());
    }

    let options = options();
    let preliminary = oxipng::optimize_from_memory(input, &options)
        .map_err(|error| anyhow!("PNG normalization failed: {error}"))?;
    let decoded = decode_static_raster(&preliminary, MAX_DECOMPRESSED_SIZE)
        .context("normalized PNG failed to decode")?;
    if decoded.interlaced {
        bail!("oxipng did not produce the requested non-interlaced PNG");
    }
    drop(preliminary);
    let baseline = normalize::normalize_baseline(decoded.image)?;
    let source = decode_canonical(input)
        .context("failed to decode source PNG for canonical verification")?;
    let selected = normalize::encode_best_candidate(baseline, &options, |candidate| {
        verify_candidate(&source, candidate)
    })?;
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
