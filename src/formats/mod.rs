mod jpeg;
mod png;

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Change {
    Recompressed,
    Progressive,
    MetadataRemoved,
    ContainerNormalized,
}

impl Change {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Recompressed => "recompressed",
            Self::Progressive => "progressive",
            Self::MetadataRemoved => "metadata removed",
            Self::ContainerNormalized => "container normalized",
        }
    }
}

pub(crate) enum FormatOutcome {
    Unchanged,
    Candidate(Candidate),
}

pub(crate) struct Candidate {
    pub bytes: Vec<u8>,
    pub changes: Vec<Change>,
}

struct Handler {
    name: &'static str,
    extensions: &'static [&'static str],
    optimize: fn(&[u8]) -> Result<FormatOutcome>,
}

static HANDLERS: &[Handler] = &[
    Handler {
        name: "JPEG",
        extensions: &["jpg", "jpeg"],
        optimize: jpeg::optimize,
    },
    Handler {
        name: "PNG",
        extensions: &["png", "apng"],
        optimize: png::optimize,
    },
];

pub(crate) fn supports_path(path: &Path) -> bool {
    handler_for_path(path).is_some()
}

pub(crate) fn optimize(path: &Path, input: &[u8]) -> Result<(&'static str, FormatOutcome)> {
    let handler = handler_for_path(path).ok_or_else(|| unsupported_extension(path))?;

    let outcome = (handler.optimize)(input)
        .with_context(|| format!("failed to optimize {}", handler.name))?;
    Ok((handler.name, outcome))
}

pub(crate) fn unsupported_extension(path: &Path) -> anyhow::Error {
    anyhow!(
        "unsupported file extension: {} (expected .jpg, .jpeg, .png, or .apng)",
        path.display()
    )
}

fn handler_for_path(path: &Path) -> Option<&'static Handler> {
    let extension = path.extension()?.to_str()?;
    HANDLERS.iter().find(|handler| {
        handler
            .extensions
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    })
}

pub(super) fn choose_candidate(
    original: &[u8],
    cleanup_required: bool,
    candidates: Vec<Candidate>,
    mut verify: impl FnMut(&[u8]) -> Result<()>,
) -> Result<FormatOutcome> {
    let mut valid = Vec::new();
    let mut last_error = None;

    for candidate in candidates {
        if candidate.bytes == original {
            continue;
        }
        match verify(&candidate.bytes) {
            Ok(()) => valid.push(candidate),
            Err(error) => last_error = Some(error),
        }
    }

    let selected = valid
        .into_iter()
        .filter(|candidate| cleanup_required || candidate.bytes.len() < original.len())
        .min_by_key(|candidate| candidate.bytes.len());

    if let Some(candidate) = selected {
        return Ok(FormatOutcome::Candidate(candidate));
    }

    if cleanup_required {
        if let Some(error) = last_error {
            return Err(error).context("no cleanup candidate passed verification");
        }
        bail!("no cleanup candidate was produced");
    }

    Ok(FormatOutcome::Unchanged)
}
