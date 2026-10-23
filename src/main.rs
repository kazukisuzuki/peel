use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use anyhow::Result;
use clap::{Arg, ArgAction, Command, value_parser};
use peel::discovery;
use peel::{Optimization, Status, optimize_file};

#[derive(Default)]
struct Summary {
    optimized: usize,
    unchanged: usize,
    failed: usize,
    bytes_saved: u64,
}

fn main() -> ExitCode {
    let patterns = parse_cli();
    let (files, discovery_errors) = discovery::discover(&patterns);
    let mut summary = Summary {
        failed: discovery_errors.len(),
        ..Summary::default()
    };

    for error in discovery_errors {
        eprintln!("[ERROR] {error:#}");
    }

    for (path, result) in optimize_files(files) {
        match result {
            Ok(result) => match result.status {
                Status::Optimized => {
                    summary.optimized += 1;
                    summary.bytes_saved += result.bytes_saved();
                    let original_size = format_size(result.original_size);
                    let final_size = format_size(result.final_size);
                    let changes = result
                        .changes
                        .iter()
                        .map(|change| change.label())
                        .collect::<Vec<_>>()
                        .join(", ");
                    if result.final_size <= result.original_size {
                        println!(
                            "[OK] {}: {} -> {} ({}, {})",
                            path.display(),
                            original_size,
                            final_size,
                            result.format,
                            changes,
                        );
                    } else {
                        let size_larger = format_size(result.final_size - result.original_size);
                        println!(
                            "[OK] {}: {} -> {} ({} larger, {}, {})",
                            path.display(),
                            original_size,
                            final_size,
                            size_larger,
                            result.format,
                            changes,
                        );
                    }
                }
                Status::Unchanged => {
                    summary.unchanged += 1;
                    println!(
                        "[SKIP] {}: no smaller lossless representation",
                        path.display()
                    );
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

fn parse_cli() -> Vec<OsString> {
    let mut matches = Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .bin_name("peel")
        .about("Optimizes JPEG and PNG images losslessly in place")
        .arg(
            Arg::new("patterns")
                .value_name("PATTERN")
                .help("File path or glob pattern (*.jpg, **/*.png, **/*.apng, and similar)")
                .value_parser(value_parser!(OsString))
                .num_args(1..)
                .action(ArgAction::Append)
                .required(true),
        )
        .get_matches();

    matches
        .remove_many("patterns")
        .expect("patterns are required by clap")
        .collect()
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
