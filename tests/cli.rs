use std::fs;
use std::io::Cursor;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;
use std::process::{Command, Output};

use jpeg_decoder::Decoder as JpegDecoder;

fn sample_jpeg() -> &'static [u8] {
    include_bytes!("fixtures/sample.jpg")
}

fn decode_jpeg(jpeg: &[u8]) -> Vec<u8> {
    let mut decoder = JpegDecoder::new(jpeg);
    decoder.decode().expect("test JPEG should decode")
}

fn jpeg_with_metadata() -> Vec<u8> {
    let marker_payload = b"Exif\0\0peel-test-metadata";
    let mut jpeg = Vec::with_capacity(sample_jpeg().len() + marker_payload.len() + 24);
    jpeg.extend_from_slice(&sample_jpeg()[..2]);
    jpeg.extend_from_slice(&[0xff, 0xe1]);
    jpeg.extend_from_slice(
        &u16::try_from(marker_payload.len() + 2)
            .unwrap()
            .to_be_bytes(),
    );
    jpeg.extend_from_slice(marker_payload);
    jpeg.extend_from_slice(&sample_jpeg()[2..]);
    jpeg.extend_from_slice(b"private-jpeg-suffix");
    jpeg
}

fn make_png() -> (Vec<u8>, Vec<u8>) {
    let pixels = vec![
        255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 255, 255, 255, 255,
    ];
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&pixels).unwrap();
    }
    (bytes, pixels)
}

fn decode_png(bytes: &[u8]) -> Vec<u8> {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info().unwrap();
    let mut output = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut output).unwrap();
    output.truncate(info.buffer_size());
    output
}

fn peel() -> Command {
    Command::new(env!("CARGO_BIN_EXE_peel"))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn cli_contract() {
    let version = peel().arg("--version").output().unwrap();
    assert_success(&version);
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        "peel 0.3.0"
    );

    let help = peel().arg("--help").output().unwrap();
    assert_success(&help);
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("Usage: peel <PATTERN>..."));
    assert!(help.contains("JPEG and PNG"));
    assert!(!help.contains("--recursive"));
    assert_failure(&peel().arg("-r").arg(".").output().unwrap());
}

#[test]
fn optimizes_supported_formats_without_changing_image_data() {
    let directory = tempfile::tempdir().unwrap();
    let jpeg = directory.path().join("image.jpg");
    let png = directory.path().join("image.png");

    let jpeg_bytes = jpeg_with_metadata();
    let jpeg_pixels = decode_jpeg(&jpeg_bytes);
    fs::write(&jpeg, &jpeg_bytes).unwrap();

    let (mut png_bytes, png_pixels) = make_png();
    png_bytes.extend_from_slice(b"private-png-suffix");
    fs::write(&png, &png_bytes).unwrap();

    let output = peel().arg(&jpeg).arg(&png).output().unwrap();
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Done: 2 optimized"));

    let optimized_jpeg = fs::read(&jpeg).unwrap();
    assert!(optimized_jpeg.len() < jpeg_bytes.len());
    assert!(!optimized_jpeg.windows(2).any(|part| part == [0xff, 0xc0]));
    assert!(optimized_jpeg.windows(2).any(|part| part == [0xff, 0xc2]));
    assert_eq!(decode_jpeg(&optimized_jpeg), jpeg_pixels);
    assert!(!optimized_jpeg.ends_with(b"private-jpeg-suffix"));

    let optimized_png = fs::read(&png).unwrap();
    assert_eq!(decode_png(&optimized_png), png_pixels);
    assert!(!optimized_png.ends_with(b"private-png-suffix"));

    let paths = [&jpeg, &png];
    let before_second_run = paths
        .iter()
        .map(|path| {
            (
                fs::read(path).unwrap(),
                fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let second = peel().args(paths).output().unwrap();
    assert_success(&second);
    assert!(String::from_utf8_lossy(&second.stdout).contains("Done: 0 optimized, 2 unchanged"));
    for (path, (bytes, modified)) in paths.iter().zip(before_second_run) {
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
    }
}

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
    assert!(fs::metadata(&child).unwrap().len() < sample_jpeg().len() as u64);
}

#[test]
fn rejects_bad_inputs_without_losing_valid_work() {
    let directory = tempfile::tempdir().unwrap();

    for extension in ["webp", "gif"] {
        let unsupported = directory.path().join(format!("image.{extension}"));
        fs::write(&unsupported, b"not an image").unwrap();
        let output = peel().arg(&unsupported).output().unwrap();
        assert_failure(&output);
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported file extension"));
        assert_eq!(fs::read(&unsupported).unwrap(), b"not an image");
    }

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
    assert!(fs::metadata(&valid).unwrap().len() < sample_jpeg().len() as u64);

    let broken = directory.path().join("broken.png");
    fs::write(&broken, b"not an image").unwrap();
    assert_failure(&peel().arg(&broken).output().unwrap());
    assert_eq!(fs::read(&broken).unwrap(), b"not an image");

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
fn rejects_apng_without_replacing_it() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("animation.png");
    let (mut bytes, _) = make_png();
    let iend = bytes.len() - 12;
    let mut actl = Vec::new();
    actl.extend_from_slice(&8u32.to_be_bytes());
    actl.extend_from_slice(b"acTL");
    actl.extend_from_slice(&1u32.to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes());
    bytes.splice(iend..iend, actl);
    fs::write(&target, &bytes).unwrap();

    let output = peel().arg(&target).output().unwrap();
    assert_failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("UnsupportedApngWithStripAll"));
    assert_eq!(fs::read(target).unwrap(), bytes);
}
