use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;

use crate::common::{assert_failure, assert_success, peel};
use crate::jpeg::sample_jpeg;
use crate::webp::{make_webp, riff_chunk};

#[test]
fn expands_deduplicates_and_sorts_globs() {
    let directory = tempfile::tempdir().unwrap();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    for name in ["2.jpg", "10.jpg", "photo[1].jpg"] {
        fs::write(directory.path().join(name), sample_jpeg()).unwrap();
    }
    let child = nested.join("child.jpg");
    fs::write(&child, sample_jpeg()).unwrap();

    let shallow = peel()
        .current_dir(directory.path())
        .arg("photo[1].jpg")
        .arg("*.jpg")
        .output()
        .unwrap();
    assert_success(&shallow);
    let stdout = String::from_utf8(shallow.stdout).unwrap();
    assert!(stdout.contains("Done: 3 optimized"));
    assert!(stdout.find("[OK] 2.jpg:").unwrap() < stdout.find("[OK] 10.jpg:").unwrap());
    assert_eq!(fs::read(&child).unwrap(), sample_jpeg());

    let recursive = peel()
        .current_dir(directory.path())
        .arg("**/*.jpg")
        .output()
        .unwrap();
    assert_success(&recursive);
    assert!(String::from_utf8_lossy(&recursive.stdout).contains("Done: 1 optimized"));
}

#[test]
fn rejects_bad_inputs_without_losing_valid_work() {
    let directory = tempfile::tempdir().unwrap();

    let unsupported = directory.path().join("image.gif");
    fs::write(&unsupported, b"not an image").unwrap();
    let output = peel().arg(&unsupported).output().unwrap();
    assert_failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported file extension"));
    assert_eq!(fs::read(&unsupported).unwrap(), b"not an image");

    assert_failure(&peel().arg(directory.path()).output().unwrap());

    let valid = directory.path().join("valid.jpg");
    fs::write(&valid, sample_jpeg()).unwrap();
    let output = peel()
        .current_dir(directory.path())
        .arg("[")
        .arg("missing-*.png")
        .arg("valid.jpg")
        .output()
        .unwrap();
    assert_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid glob pattern"));
    assert!(stderr.contains("matched no supported regular files"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("[OK] valid.jpg:"));

    let broken = directory.path().join("broken.png");
    fs::write(&broken, b"not an image").unwrap();
    assert_failure(&peel().arg(&broken).output().unwrap());
    assert_eq!(fs::read(&broken).unwrap(), b"not an image");

    let broken_webp = directory.path().join("broken.webp");
    let mut webp_bytes = make_webp(&[riff_chunk(*b"VP8L", &[0x2f], 0)], &[]);
    webp_bytes[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&broken_webp, &webp_bytes).unwrap();
    let output = peel().arg(&broken_webp).output().unwrap();
    assert_failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed to optimize WebP"));
    assert_eq!(fs::read(&broken_webp).unwrap(), webp_bytes);

    let target = directory.path().join("target.jpg");
    let link = directory.path().join("link.jpg");
    fs::write(&target, sample_jpeg()).unwrap();
    if let Err(error) = symlink(&target, &link) {
        #[cfg(windows)]
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("failed to create test symbolic link: {error}");
    }
    assert_failure(&peel().arg(&link).output().unwrap());
    assert_eq!(fs::read(&target).unwrap(), sample_jpeg());
}

#[test]
fn quiet_keeps_errors_and_distinguishes_failure_types() {
    let directory = tempfile::tempdir().unwrap();
    let valid = directory.path().join("valid.jpg");
    let broken = directory.path().join("broken.png");
    fs::write(&valid, sample_jpeg()).unwrap();
    fs::write(&broken, b"not an image").unwrap();

    let output = peel()
        .current_dir(directory.path())
        .arg("--quiet")
        .arg("[")
        .arg("valid.jpg")
        .arg("broken.png")
        .output()
        .unwrap();

    assert_failure(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    assert!(stdout.starts_with("Done: 1 optimized, 0 unchanged, 1 file failed, 1 input error,"));
    assert!(!stdout.contains("valid.jpg"));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid glob pattern"));
    assert!(stderr.contains("broken.png"));
    assert!(stderr.contains("failed to optimize PNG"));
    assert_ne!(fs::read(&valid).unwrap(), sample_jpeg());
    assert_eq!(fs::read(&broken).unwrap(), b"not an image");
}
