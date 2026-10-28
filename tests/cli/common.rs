use std::fs;
use std::path::Path;
use std::process::{Command, Output};

pub(super) fn peel() -> Command {
    Command::new(env!("CARGO_BIN_EXE_peel"))
}

pub(super) fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

pub(super) fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

pub(super) fn assert_second_run_unchanged(paths: &[&Path]) -> Output {
    let before_second_run = paths
        .iter()
        .map(|path| {
            (
                fs::read(path).unwrap(),
                fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let output = peel().args(paths).output().unwrap();
    assert_success(&output);
    for (path, (bytes, modified)) in paths.iter().zip(before_second_run) {
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
    }
    output
}
