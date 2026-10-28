use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::thread;

use anyhow::Result;
use clap::{Arg, ArgAction, Command, value_parser};
use peel::discovery;
use peel::{Optimization, Status, optimize_file};

struct Cli {
    patterns: Vec<OsString>,
    quiet: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Retention {
    All,
    FailuresOnly,
}

#[derive(Debug, Eq, PartialEq)]
enum NetSizeChange {
    Saved(u128),
    Increased(u128),
    Unchanged,
}

#[derive(Default)]
struct Summary {
    optimized: usize,
    unchanged: usize,
    file_failed: usize,
    input_errors: usize,
    bytes_reduced: u128,
    bytes_added: u128,
}

impl Summary {
    fn record_success(&mut self, optimization: &Optimization) {
        match optimization.status {
            Status::Optimized => {
                self.optimized += 1;
                let original_size = u128::from(optimization.original_size);
                let final_size = u128::from(optimization.final_size);
                if final_size <= original_size {
                    self.bytes_reduced += original_size - final_size;
                } else {
                    self.bytes_added += final_size - original_size;
                }
            }
            Status::Unchanged => self.unchanged += 1,
        }
    }

    fn record_file_failure(&mut self) {
        self.file_failed += 1;
    }

    fn record_input_errors(&mut self, count: usize) {
        self.input_errors += count;
    }

    fn merge(&mut self, other: Self) {
        self.optimized += other.optimized;
        self.unchanged += other.unchanged;
        self.file_failed += other.file_failed;
        self.input_errors += other.input_errors;
        self.bytes_reduced += other.bytes_reduced;
        self.bytes_added += other.bytes_added;
    }

    fn has_errors(&self) -> bool {
        self.file_failed != 0 || self.input_errors != 0
    }

    fn net_size_change(&self) -> NetSizeChange {
        match self.bytes_reduced.cmp(&self.bytes_added) {
            std::cmp::Ordering::Greater => {
                NetSizeChange::Saved(self.bytes_reduced - self.bytes_added)
            }
            std::cmp::Ordering::Less => {
                NetSizeChange::Increased(self.bytes_added - self.bytes_reduced)
            }
            std::cmp::Ordering::Equal => NetSizeChange::Unchanged,
        }
    }
}

impl Summary {
    fn format(&self) -> String {
        let file_failed = if self.file_failed == 1 {
            "file failed"
        } else {
            "files failed"
        };
        let input_errors = if self.input_errors == 1 {
            "input error"
        } else {
            "input errors"
        };
        let size_change = match self.net_size_change() {
            NetSizeChange::Saved(bytes) => format!("{} net saved", format_size(bytes)),
            NetSizeChange::Increased(bytes) => format!("{} net increase", format_size(bytes)),
            NetSizeChange::Unchanged => "no net size change".to_owned(),
        };

        format!(
            "Done: {} optimized, {} unchanged, {} {file_failed}, {} {input_errors}, {size_change}",
            format_number(self.optimized),
            format_number(self.unchanged),
            format_number(self.file_failed),
            format_number(self.input_errors),
        )
    }
}

struct ProcessedFile {
    index: usize,
    path: PathBuf,
    result: Result<Optimization>,
}

#[derive(Default)]
struct ProcessingOutput {
    summary: Summary,
    files: Vec<ProcessedFile>,
}

fn main() -> ExitCode {
    let cli = parse_cli();
    let (files, discovery_errors) = discovery::discover(&cli.patterns);
    let input_errors = discovery_errors.len();

    for error in discovery_errors {
        eprintln!("[ERROR] {error:#}");
    }

    let retention = if cli.quiet {
        Retention::FailuresOnly
    } else {
        Retention::All
    };
    let mut output = optimize_files(files, retention);
    output.summary.record_input_errors(input_errors);
    render_files(output.files);
    println!("{}", output.summary.format());

    if output.summary.has_errors() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn parse_cli() -> Cli {
    let mut matches = Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .bin_name("peel")
        .about("Optimizes JPEG, PNG, and WebP images losslessly in place")
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .help("Suppress per-file output; show only the final summary and errors")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("patterns")
                .value_name("PATTERN")
                .help("File path or glob pattern (*.jpg, **/*.png, **/*.webp, and similar)")
                .value_parser(value_parser!(OsString))
                .num_args(1..)
                .action(ArgAction::Append)
                .required(true),
        )
        .get_matches();

    Cli {
        quiet: matches.get_flag("quiet"),
        patterns: matches
            .remove_many("patterns")
            .expect("patterns are required by clap")
            .collect(),
    }
}

fn optimize_files(files: Vec<PathBuf>, retention: Retention) -> ProcessingOutput {
    let worker_count = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(files.len());
    let queue = Mutex::new(files.into_iter().enumerate());

    let worker_outputs = thread::scope(|scope| {
        let workers = (0..worker_count)
            .map(|_| {
                scope.spawn(|| {
                    let mut output = ProcessingOutput::default();
                    loop {
                        let next = queue.lock().expect("work queue lock was poisoned").next();
                        let Some((index, path)) = next else {
                            break;
                        };
                        let result = optimize_file(&path);
                        match &result {
                            Ok(optimization) => output.summary.record_success(optimization),
                            Err(_) => output.summary.record_file_failure(),
                        }
                        if retention == Retention::All || result.is_err() {
                            output.files.push(ProcessedFile {
                                index,
                                path,
                                result,
                            });
                        }
                    }
                    output
                })
            })
            .collect::<Vec<_>>();

        workers
            .into_iter()
            .map(|worker| worker.join().expect("optimization worker panicked"))
            .collect::<Vec<_>>()
    });

    let mut output = ProcessingOutput::default();
    for worker_output in worker_outputs {
        output.summary.merge(worker_output.summary);
        output.files.extend(worker_output.files);
    }
    output.files.sort_unstable_by_key(|file| file.index);
    output
}

fn render_files(files: Vec<ProcessedFile>) {
    for file in files {
        match file.result {
            Ok(result) => render_success(&file.path, &result),
            Err(error) => eprintln!("[ERROR] {}: {error:#}", file.path.display()),
        }
    }
}

fn render_success(path: &Path, result: &Optimization) {
    match result.status {
        Status::Optimized => {
            let original_size = format_size(result.original_size.into());
            let final_size = format_size(result.final_size.into());
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
                let size_larger = format_size((result.final_size - result.original_size).into());
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
        Status::Unchanged => println!(
            "[SKIP] {}: no smaller lossless representation",
            path.display()
        ),
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

fn format_size(bytes: u128) -> String {
    const KIBIBYTE: u128 = 1_024;
    const UNITS: [(&str, u128); 4] = [
        ("KiB", KIBIBYTE),
        ("MiB", KIBIBYTE.pow(2)),
        ("GiB", KIBIBYTE.pow(3)),
        ("TiB", KIBIBYTE.pow(4)),
    ];

    if bytes < KIBIBYTE {
        return format!("{} B", format_number(bytes));
    }

    let mut unit_index = UNITS
        .iter()
        .rposition(|(_, size)| bytes >= *size)
        .expect("bytes are at least one kibibyte");

    loop {
        let (unit_name, unit_size) = UNITS[unit_index];
        let mut whole = bytes / unit_size;
        let remainder = bytes % unit_size;
        let mut hundredths = (remainder * 100 + unit_size / 2) / unit_size;
        if hundredths == 100 {
            whole += 1;
            hundredths = 0;
        }
        if whole == KIBIBYTE && unit_index + 1 < UNITS.len() {
            unit_index += 1;
            continue;
        }

        let whole = format_number(whole);
        return match hundredths {
            0 => format!("{whole} {unit_name}"),
            value if value.is_multiple_of(10) => {
                format!("{whole}.{} {unit_name}", value / 10)
            }
            value => format!("{whole}.{value:02} {unit_name}"),
        };
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn optimization(original_size: u64, final_size: u64, status: Status) -> Optimization {
        Optimization {
            original_size,
            final_size,
            status,
            format: "test",
            changes: Vec::new(),
        }
    }

    #[test]
    fn summary_records_reductions_increases_and_unchanged_files() {
        let mut summary = Summary::default();
        summary.record_success(&optimization(1_000, 700, Status::Optimized));
        summary.record_success(&optimization(400, 550, Status::Optimized));
        summary.record_success(&optimization(200, 200, Status::Unchanged));

        assert_eq!(summary.optimized, 2);
        assert_eq!(summary.unchanged, 1);
        assert_eq!(summary.bytes_reduced, 300);
        assert_eq!(summary.bytes_added, 150);
        assert_eq!(summary.net_size_change(), NetSizeChange::Saved(150));
    }

    #[test]
    fn summary_reports_net_increase_and_no_change() {
        let mut summary = Summary {
            bytes_reduced: 100,
            bytes_added: 250,
            ..Summary::default()
        };
        assert_eq!(summary.net_size_change(), NetSizeChange::Increased(150));

        summary.bytes_reduced = 250;
        assert_eq!(summary.net_size_change(), NetSizeChange::Unchanged);
    }

    #[test]
    fn summary_merges_workers_and_distinguishes_error_types() {
        let mut summary = Summary {
            optimized: 1,
            bytes_reduced: 500,
            ..Summary::default()
        };
        summary.merge(Summary {
            unchanged: 2,
            file_failed: 1,
            input_errors: 3,
            bytes_added: 100,
            ..Summary::default()
        });

        assert_eq!(summary.optimized, 1);
        assert_eq!(summary.unchanged, 2);
        assert_eq!(summary.file_failed, 1);
        assert_eq!(summary.input_errors, 3);
        assert_eq!(summary.bytes_reduced, 500);
        assert_eq!(summary.bytes_added, 100);
    }

    #[test]
    fn either_error_type_causes_failure() {
        assert!(!Summary::default().has_errors());
        assert!(
            Summary {
                file_failed: 1,
                ..Summary::default()
            }
            .has_errors()
        );
        assert!(
            Summary {
                input_errors: 1,
                ..Summary::default()
            }
            .has_errors()
        );
    }

    #[test]
    fn format_size_uses_bytes_below_one_kibibyte() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1), "1 B");
        assert_eq!(format_size(1_023), "1,023 B");
    }

    #[test]
    fn format_size_uses_iec_units_and_trims_fractional_zeroes() {
        assert_eq!(format_size(1_024), "1 KiB");
        assert_eq!(format_size(1_152), "1.13 KiB");
        assert_eq!(format_size(1_536), "1.5 KiB");
        assert_eq!(format_size(1_024_u128.pow(2)), "1 MiB");
        assert_eq!(format_size(1_024_u128.pow(3)), "1 GiB");
        assert_eq!(format_size(1_024_u128.pow(4)), "1 TiB");
    }

    #[test]
    fn format_size_promotes_values_that_round_to_the_next_unit() {
        assert_eq!(format_size(1_024_u128.pow(2) - 1), "1 MiB");
    }

    #[test]
    fn format_size_accepts_totals_larger_than_u64() {
        assert_eq!(format_size(u128::from(u64::MAX) + 1), "16,777,216 TiB");
    }

    #[test]
    fn failures_only_retains_only_failed_files() {
        let directory = tempfile::tempdir().unwrap();
        let valid = directory.path().join("valid.jpg");
        let broken = directory.path().join("broken.png");
        fs::write(&valid, include_bytes!("../tests/fixtures/sample.jpg")).unwrap();
        fs::write(&broken, b"not a PNG").unwrap();

        let output = optimize_files(vec![valid, broken.clone()], Retention::FailuresOnly);

        assert_eq!(output.summary.optimized + output.summary.unchanged, 1);
        assert_eq!(output.summary.file_failed, 1);
        assert_eq!(output.files.len(), 1);
        assert_eq!(output.files[0].path, broken);
        assert!(output.files[0].result.is_err());
    }
}
