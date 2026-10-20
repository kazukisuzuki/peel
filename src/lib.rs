pub mod discovery;
mod formats;
mod mozjpeg;

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

pub use formats::Change;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Optimized,
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Optimization {
    pub original_size: u64,
    pub final_size: u64,
    pub status: Status,
    pub format: &'static str,
    pub changes: Vec<Change>,
}

impl Optimization {
    #[must_use]
    pub fn bytes_saved(&self) -> u64 {
        self.original_size.saturating_sub(self.final_size)
    }
}

/// Applies the format-specific lossless optimizer and metadata cleanup in place.
///
/// The handler selected by the file extension validates the file signature and
/// representation invariants before a same-directory atomic replacement occurs.
pub fn optimize_file(path: &Path) -> Result<Optimization> {
    let original = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let original_size = original.len() as u64;
    let (format, outcome) = formats::optimize(path, &original)?;

    match outcome {
        formats::FormatOutcome::Unchanged => Ok(Optimization {
            original_size,
            final_size: original_size,
            status: Status::Unchanged,
            format,
            changes: Vec::new(),
        }),
        formats::FormatOutcome::Candidate(candidate) => {
            replace_file(path, &candidate.bytes)?;
            Ok(Optimization {
                original_size,
                final_size: candidate.bytes.len() as u64,
                status: Status::Optimized,
                format,
                changes: candidate.changes,
            })
        }
    }
}

fn replace_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let permissions = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?
        .permissions();

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temporary file in {}", parent.display()))?;
    temporary
        .write_all(contents)
        .context("failed to write the optimized image")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to flush the optimized image")?;
    temporary
        .as_file()
        .set_permissions(permissions)
        .context("failed to preserve file permissions")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}
