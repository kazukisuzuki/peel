use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;

use assert_cmd::Command;
use jpeg_decoder::Decoder;
use predicates::prelude::*;

fn sample() -> &'static [u8] {
    include_bytes!("fixtures/sample.jpg")
}

fn decode(jpeg: &[u8]) -> Vec<u8> {
    let mut decoder = Decoder::new(jpeg);
    decoder.decode().expect("test JPEG should decode")
}

#[test]
fn produces_an_optimized_progressive_without_changing_pixels() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("copy.jpg");
    fs::write(&target, sample()).unwrap();
    let original_pixels = decode(sample());
    let original_size = sample().len();

    Command::cargo_bin("optijpeg")
        .unwrap()
        .arg(&target)
        .assert()
        .success()
        .stdout(predicate::str::contains(": 165 KiB -> "))
        .stdout(predicate::str::contains("Done: 1 optimized"));

    let optimized = fs::read(&target).unwrap();
    assert!(optimized.len() < original_size);
    assert!(!optimized.windows(2).any(|part| part == [0xff, 0xc0]));
    assert!(optimized.windows(2).any(|part| part == [0xff, 0xc2]));
    assert_eq!(decode(&optimized), original_pixels);
}

#[test]
fn recursively_processes_jpg_and_jpeg_only() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    fs::write(first.path().join("one.JPG"), sample()).unwrap();
    fs::write(second.path().join("two.jpeg"), sample()).unwrap();
    fs::write(second.path().join("ignored.png"), sample()).unwrap();

    Command::cargo_bin("optijpeg")
        .unwrap()
        .arg("-r")
        .arg(first.path())
        .arg(second.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Done: 2 optimized"));

    assert!(fs::metadata(first.path().join("one.JPG")).unwrap().len() < sample().len() as u64);
    assert!(fs::metadata(second.path().join("two.jpeg")).unwrap().len() < sample().len() as u64);
    assert_eq!(
        fs::read(second.path().join("ignored.png")).unwrap(),
        sample()
    );
}

#[test]
fn prints_results_in_natural_path_order() {
    let directory = tempfile::tempdir().unwrap();
    let second = directory.path().join("2.jpg");
    let tenth = directory.path().join("10.jpg");
    fs::write(&second, sample()).unwrap();
    fs::write(&tenth, sample()).unwrap();

    let output = Command::cargo_bin("optijpeg")
        .unwrap()
        .arg(&tenth)
        .arg(&second)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let second_log = format!("[OK] {}:", second.display());
    let tenth_log = format!("[OK] {}:", tenth.display());
    assert!(
        stdout.find(&second_log).unwrap() < stdout.find(&tenth_log).unwrap(),
        "unexpected output order:\n{stdout}"
    );
}

#[test]
fn rejects_direct_files_without_a_jpeg_extension() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("image.png");
    fs::write(&target, sample()).unwrap();

    Command::cargo_bin("optijpeg")
        .unwrap()
        .arg(&target)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "file must have a .jpg or .jpeg extension",
        ));
}

#[test]
fn rejects_direct_symbolic_links() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target.jpg");
    let link = directory.path().join("link.jpg");
    fs::write(&target, sample()).unwrap();
    if let Err(error) = symlink(&target, &link) {
        #[cfg(windows)]
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            return;
        }
        panic!("failed to create test symbolic link: {error}");
    }

    Command::cargo_bin("optijpeg")
        .unwrap()
        .arg(&link)
        .assert()
        .failure()
        .stderr(predicate::str::contains("symbolic links are not supported"));

    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read(target).unwrap(), sample());
}

#[test]
fn invalid_jpeg_is_not_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("broken.jpg");
    let contents = b"not a jpeg";
    fs::write(&target, contents).unwrap();

    Command::cargo_bin("optijpeg")
        .unwrap()
        .arg(&target)
        .assert()
        .failure()
        .stderr(predicate::str::contains("[ERROR]"));

    assert_eq!(fs::read(target).unwrap(), contents);
}

#[test]
fn strips_app_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("with-metadata.jpg");
    let marker_payload = b"Exif\0\0optijpeg-test-metadata";
    let mut jpeg = Vec::with_capacity(sample().len() + marker_payload.len() + 4);
    jpeg.extend_from_slice(&sample()[..2]);
    jpeg.extend_from_slice(&[0xff, 0xe1]);
    jpeg.extend_from_slice(
        &u16::try_from(marker_payload.len() + 2)
            .unwrap()
            .to_be_bytes(),
    );
    jpeg.extend_from_slice(marker_payload);
    jpeg.extend_from_slice(&sample()[2..]);
    fs::write(&target, &jpeg).unwrap();

    Command::cargo_bin("optijpeg")
        .unwrap()
        .arg(&target)
        .assert()
        .success();

    let optimized = fs::read(&target).unwrap();
    assert!(
        !optimized
            .windows(marker_payload.len())
            .any(|part| part == marker_payload)
    );
    assert_eq!(decode(&optimized), decode(sample()));
}
