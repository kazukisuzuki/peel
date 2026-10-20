use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use glob::{MatchOptions, glob_with};
use lexical_sort::{PathSort, natural_lexical_cmp};

use crate::formats;

pub fn discover(patterns: &[OsString]) -> (Vec<PathBuf>, Vec<anyhow::Error>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut seen = HashSet::new();

    for pattern in patterns {
        let path = PathBuf::from(pattern);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => match validate_literal(&path, &metadata) {
                Ok(()) => push_unique(path, &mut files, &mut seen),
                Err(error) => errors.push(error),
            },
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound || looks_like_glob(pattern) =>
            {
                expand_pattern(pattern, &mut files, &mut seen, &mut errors);
            }
            Err(error) => errors.push(
                anyhow::Error::new(error)
                    .context(format!("failed to inspect literal path {}", path.display())),
            ),
        }
    }

    files.path_sort(natural_lexical_cmp);
    (files, errors)
}

fn looks_like_glob(value: &OsString) -> bool {
    value
        .to_str()
        .is_some_and(|value| value.contains(['*', '?', '[']))
}

fn validate_literal(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        bail!("symbolic links are not supported: {}", path.display());
    }
    if metadata.is_dir() {
        bail!(
            "{} is a directory; use a glob such as {}/**/*",
            path.display(),
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("not a regular file: {}", path.display());
    }
    if !formats::supports_path(path) {
        return Err(formats::unsupported_extension(path));
    }
    Ok(())
}

fn expand_pattern(
    pattern: &OsString,
    files: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(pattern) = pattern.to_str() else {
        errors.push(anyhow!(
            "non-Unicode glob patterns are not supported unless they name an existing literal file"
        ));
        return;
    };
    let options = MatchOptions {
        case_sensitive: !cfg!(windows),
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let entries = match glob_with(pattern, options) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(anyhow!("invalid glob pattern {pattern:?}: {error}"));
            return;
        }
    };

    let mut supported_matches = 0usize;
    for entry in entries {
        let path = match entry {
            Ok(path) => path,
            Err(error) => {
                errors.push(anyhow!("glob traversal failed for {pattern:?}: {error}"));
                continue;
            }
        };
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                errors.push(
                    anyhow::Error::new(error)
                        .context(format!("failed to inspect glob result {}", path.display())),
                );
                continue;
            }
        };
        if metadata.is_file() && !metadata.file_type().is_symlink() && formats::supports_path(&path)
        {
            supported_matches += 1;
            push_unique(path, files, seen);
        }
    }

    if supported_matches == 0 {
        errors.push(anyhow!(
            "glob pattern matched no supported regular files: {pattern:?}"
        ));
    }
}

fn push_unique(path: PathBuf, files: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
    if seen.insert(identity) {
        files.push(path);
    }
}
