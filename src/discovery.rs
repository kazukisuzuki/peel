use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use glob::{MatchOptions, glob_with};
use lexical_sort::{PathSort, natural_lexical_cmp};

use crate::formats;

#[derive(Default)]
struct DiscoveryState {
    files: Vec<PathBuf>,
    errors: Vec<anyhow::Error>,
    seen: HashSet<PathBuf>,
}

impl DiscoveryState {
    fn add_file(&mut self, path: PathBuf) {
        let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
        if self.seen.insert(identity) {
            self.files.push(path);
        }
    }

    fn record_error(&mut self, error: anyhow::Error) {
        self.errors.push(error);
    }

    fn expand_pattern(&mut self, pattern: &OsString) {
        let Some(pattern) = pattern.to_str() else {
            self.record_error(anyhow!(
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
                self.record_error(anyhow!("invalid glob pattern {pattern:?}: {error}"));
                return;
            }
        };

        let mut supported_matches = 0usize;
        for entry in entries {
            let path = match entry {
                Ok(path) => path,
                Err(error) => {
                    self.record_error(anyhow!("glob traversal failed for {pattern:?}: {error}"));
                    continue;
                }
            };
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.record_error(
                        anyhow::Error::new(error)
                            .context(format!("failed to inspect glob result {}", path.display())),
                    );
                    continue;
                }
            };
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && formats::supports_path(&path)
            {
                supported_matches += 1;
                self.add_file(path);
            }
        }

        if supported_matches == 0 {
            self.record_error(anyhow!(
                "glob pattern matched no supported regular files: {pattern:?}"
            ));
        }
    }

    fn finish(mut self) -> (Vec<PathBuf>, Vec<anyhow::Error>) {
        self.files.path_sort(natural_lexical_cmp);
        (self.files, self.errors)
    }
}

pub fn discover(patterns: &[OsString]) -> (Vec<PathBuf>, Vec<anyhow::Error>) {
    let mut state = DiscoveryState::default();

    for pattern in patterns {
        let path = PathBuf::from(pattern);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => match validate_literal(&path, &metadata) {
                Ok(()) => state.add_file(path),
                Err(error) => state.record_error(error),
            },
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound || looks_like_glob(pattern) =>
            {
                state.expand_pattern(pattern);
            }
            Err(error) => state.record_error(
                anyhow::Error::new(error)
                    .context(format!("failed to inspect literal path {}", path.display())),
            ),
        }
    }

    state.finish()
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
