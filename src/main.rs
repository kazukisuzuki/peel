use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use clap::{Arg, ArgAction, Command, value_parser};
use lexical_sort::{PathSort, natural_lexical_cmp};
use optijpeg::{Optimization, Status, optimize_file};
use walkdir::WalkDir;

#[derive(Debug)]
struct Cli {
    files: Vec<PathBuf>,
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
    let cli = parse_cli();
    let (files, discovery_errors) = discover_files(&cli.files, &cli.recursive);
    let summary = Summary {
        failed: discovery_errors.len(),
        ..Summary::default()
    };

    for error in discovery_errors {
        eprintln!("[ERROR] {error:#}");
    }

    let results = optimize_files(files);

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

fn parse_cli() -> Cli {
    let mut matches = Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .about("Optimize JPEG files losslessly in place")
        .arg(
            Arg::new("files")
                .value_name("JPEG")
                .help("JPEG files to optimize in place")
                .value_parser(value_parser!(PathBuf))
                .num_args(1..)
                .action(ArgAction::Append)
                .required_unless_present("recursive")
                .conflicts_with("recursive"),
        )
        .arg(
            Arg::new("recursive")
                .short('r')
                .long("recursive")
                .value_name("PATH")
                .help("Recursively optimize JPEG files below one or more paths")
                .value_parser(value_parser!(PathBuf))
                .num_args(1..)
                .action(ArgAction::Append),
        )
        .get_matches();

    Cli {
        files: matches
            .remove_many("files")
            .map(Iterator::collect)
            .unwrap_or_default(),
        recursive: matches
            .remove_many("recursive")
            .map(Iterator::collect)
            .unwrap_or_default(),
    }
}

fn optimize_files(files: Vec<PathBuf>) -> Vec<(PathBuf, Result<Optimization>)> {
    let next = AtomicUsize::new(0);
    let results: Vec<OnceLock<Result<Optimization>>> =
        (0..files.len()).map(|_| OnceLock::new()).collect();
    let worker_count = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(files.len());

    thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(index) else {
                        break;
                    };
                    let result = optimize_file(path);
                    results[index]
                        .set(result)
                        .expect("worker produced a duplicate result");
                }
            });
        }
    });

    files
        .into_iter()
        .zip(results)
        .map(|(path, result)| {
            (
                path,
                result
                    .into_inner()
                    .expect("worker did not produce an optimization result"),
            )
        })
        .collect()
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
