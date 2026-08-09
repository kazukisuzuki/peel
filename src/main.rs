use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;
use lexical_sort::{PathSort, natural_lexical_cmp};
use optijpeg::{Status, optimize_file};
use rayon::prelude::*;
use walkdir::WalkDir;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Optimize JPEG files losslessly in place",
    after_help = "Only files ending in .jpg or .jpeg (case-insensitive) are processed. Directories are accepted only as recursive search roots; symbolic links are not followed."
)]
struct Cli {
    /// JPEG files to optimize in place
    #[arg(
        value_name = "JPEG",
        required_unless_present = "recursive",
        conflicts_with = "recursive"
    )]
    files: Vec<PathBuf>,

    /// Recursively optimize JPEG files below one or more paths
    #[arg(
        short = 'r',
        long = "recursive",
        value_name = "PATH",
        num_args = 1..
    )]
    recursive: Vec<PathBuf>,
}

#[derive(Default)]
struct Summary {
    optimized: usize,
    unchanged: usize,
    failed: usize,
    bytes_saved: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (files, discovery_errors) = discover_files(&cli.files, &cli.recursive);
    let summary = Summary {
        failed: discovery_errors.len(),
        ..Summary::default()
    };

    for error in discovery_errors {
        eprintln!("[ERROR] {error:#}");
    }

    let results: Vec<_> = files
        .into_par_iter()
        .map(|path| {
            let result = optimize_file(&path);
            (path, result)
        })
        .collect();

    let mut summary = summary;
    for (path, result) in results {
        match result {
            Ok(result) => match result.status {
                Status::Optimized => {
                    summary.optimized += 1;
                    summary.bytes_saved += result.bytes_saved();
                    let original_size = format_size(result.original_size);
                    let final_size = format_size(result.final_size);
                    if result.final_size <= result.original_size {
                        let percent =
                            result.bytes_saved() as f64 * 100.0 / result.original_size as f64;
                        println!(
                            "[OK] {}: {} -> {} ({percent:.2}% saved, progressive)",
                            path.display(),
                            original_size,
                            final_size,
                        );
                    } else {
                        let size_larger = format_size(result.final_size - result.original_size);
                        println!(
                            "[OK] {}: {} -> {} ({} larger, metadata removed, progressive)",
                            path.display(),
                            original_size,
                            final_size,
                            size_larger,
                        );
                    }
                }
                Status::Unchanged => {
                    summary.unchanged += 1;
                    println!("[SKIP] {}: already optimal", path.display());
                }
            },
            Err(error) => {
                summary.failed += 1;
                eprintln!("[ERROR] {}: {error:#}", path.display());
            }
        }
    }

    println!(
        "Done: {} optimized, {} unchanged, {} failed, {} saved",
        format_number(summary.optimized),
        format_number(summary.unchanged),
        format_number(summary.failed),
        format_size(summary.bytes_saved),
    );

    if summary.failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn format_number(value: impl ToString) -> String {
    let digits = value.to_string();
    let separator_count = digits.len().saturating_sub(1) / 3;
    let mut formatted = String::with_capacity(digits.len() + separator_count);

    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }

    formatted
}

fn format_size(bytes: u64) -> String {
    const KIBIBYTE: u64 = 1_024;
    const MEBIBYTE: u64 = KIBIBYTE * 1_024;

    if bytes < MEBIBYTE {
        let kibibytes = bytes / KIBIBYTE + u64::from(bytes % KIBIBYTE >= KIBIBYTE / 2);
        return format!("{} KiB", format_number(kibibytes));
    }

    let mut mebibytes = bytes / MEBIBYTE;
    let remainder = bytes % MEBIBYTE;
    let mut hundredths = (remainder * 100 + MEBIBYTE / 2) / MEBIBYTE;
    if hundredths == 100 {
        mebibytes += 1;
        hundredths = 0;
    }

    let mebibytes = format_number(mebibytes);
    match hundredths {
        0 => format!("{mebibytes} MiB"),
        value if value.is_multiple_of(10) => format!("{mebibytes}.{} MiB", value / 10),
        value => format!("{mebibytes}.{value:02} MiB"),
    }
}

fn discover_files(direct: &[PathBuf], recursive: &[PathBuf]) -> (Vec<PathBuf>, Vec<anyhow::Error>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut seen = HashSet::new();

    for path in direct {
        match validate_direct_file(path) {
            Ok(path) => push_unique(path, &mut files, &mut seen),
            Err(error) => errors.push(error),
        }
    }

    for root in recursive {
        if !root.is_dir() {
            errors.push(anyhow::anyhow!(
                "recursive path is not a directory: {}",
                root.display()
            ));
            continue;
        }

        for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
            match entry {
                Ok(entry) if entry.file_type().is_file() && is_jpeg_path(entry.path()) => {
                    push_unique(entry.into_path(), &mut files, &mut seen);
                }
                Ok(_) => {}
                Err(error) => errors.push(error.into()),
            }
        }
    }

    files.path_sort(natural_lexical_cmp);
    (files, errors)
}

fn validate_direct_file(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;

    if metadata.file_type().is_symlink() {
        bail!("symbolic links are not supported: {}", path.display());
    }
    if metadata.is_dir() {
        bail!(
            "{} is a directory; use -r {} for recursive processing",
            path.display(),
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("not a regular file: {}", path.display());
    }
    if !is_jpeg_path(path) {
        bail!(
            "file must have a .jpg or .jpeg extension: {}",
            path.display()
        );
    }
    Ok(path.to_path_buf())
}

fn push_unique(path: PathBuf, files: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
    if seen.insert(identity) {
        files.push(path);
    }
}

fn is_jpeg_path(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg")
    })
}
