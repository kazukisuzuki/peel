#[path = "cli/common.rs"]
mod common;
#[path = "cli/discovery.rs"]
mod discovery;
#[path = "cli/jpeg.rs"]
mod jpeg;
#[path = "cli/png.rs"]
mod png_tests;
#[path = "cli/webp.rs"]
mod webp;

use std::fs;

use common::{assert_second_run_unchanged, assert_success, peel};
use jpeg::{decode_jpeg, jpeg_with_metadata};
use png_tests::{decode_png, make_png};
use webp::{make_webp, riff_chunk};

#[test]
fn optimizes_supported_formats_without_changing_image_data() {
    let directory = tempfile::tempdir().unwrap();
    let jpeg = directory.path().join("image.jpg");
    let png = directory.path().join("image.png");
    let webp = directory.path().join("image.WeBp");

    let jpeg_bytes = jpeg_with_metadata();
    let jpeg_pixels = decode_jpeg(&jpeg_bytes);
    fs::write(&jpeg, &jpeg_bytes).unwrap();

    let (mut png_bytes, png_pixels) = make_png();
    png_bytes.extend_from_slice(b"private-png-suffix");
    fs::write(&png, &png_bytes).unwrap();

    let vp8l = [0x2f, 0x01, 0x40, 0x00, 0x10, 0xaa, 0xbb];
    let webp_input = make_webp(
        &[
            riff_chunk(*b"VP8X", &[0xfd, 1, 2, 3, 1, 0, 0, 1, 0, 0], 0),
            riff_chunk(*b"ICCP", b"icc", 0xa1),
            riff_chunk(*b"ALPH", &[0xd9, 0x11, 0x22, 0x33, 0x44], 0xa2),
            riff_chunk(*b"VP8L", &vp8l, 0xa3),
            riff_chunk(*b"EXIF", b"exif", 0),
            riff_chunk(*b"XMP ", b"xmp", 0xa4),
            riff_chunk(*b"PRIV", b"private", 0xa5),
        ],
        b"private-webp-suffix",
    );
    let webp_expected = make_webp(
        &[
            riff_chunk(*b"VP8X", &[0x10, 0, 0, 0, 1, 0, 0, 1, 0, 0], 0),
            riff_chunk(*b"ALPH", &[0x19, 0x11, 0x22, 0x33, 0x44], 0),
            riff_chunk(*b"VP8L", &vp8l, 0),
        ],
        &[],
    );
    fs::write(&webp, webp_input).unwrap();

    let output = peel().arg(&jpeg).arg(&png).arg(&webp).output().unwrap();
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Done: 3 optimized"));
    let webp_status = stdout.lines().find(|line| line.contains("(WebP,")).unwrap();
    assert!(webp_status.contains("container normalized, metadata removed"));
    assert!(!webp_status.contains("recompressed"));

    let optimized_jpeg = fs::read(&jpeg).unwrap();
    assert!(optimized_jpeg.len() < jpeg_bytes.len());
    assert!(!optimized_jpeg.windows(2).any(|part| part == [0xff, 0xc0]));
    assert!(optimized_jpeg.windows(2).any(|part| part == [0xff, 0xc2]));
    assert_eq!(decode_jpeg(&optimized_jpeg), jpeg_pixels);
    assert!(!optimized_jpeg.ends_with(b"private-jpeg-suffix"));

    let optimized_png = fs::read(&png).unwrap();
    assert_eq!(decode_png(&optimized_png), png_pixels);
    assert!(!optimized_png.ends_with(b"private-png-suffix"));

    assert_eq!(fs::read(&webp).unwrap(), webp_expected);

    let paths = [jpeg.as_path(), png.as_path(), webp.as_path()];
    let second = assert_second_run_unchanged(&paths);
    assert!(String::from_utf8_lossy(&second.stdout).contains("Done: 0 optimized, 3 unchanged"));
}

#[test]
fn quiet_flags_suppress_per_file_output_without_changing_work() {
    for flag in ["-q", "--quiet"] {
        let directory = tempfile::tempdir().unwrap();
        let jpeg = directory.path().join("quiet.jpg");
        let input = jpeg_with_metadata();
        fs::write(&jpeg, &input).unwrap();

        let output = peel().arg(flag).arg(&jpeg).output().unwrap();
        assert_success(&output);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.lines().count(), 1);
        assert!(stdout.starts_with("Done: 1 optimized, 0 unchanged"));
        assert!(stdout.contains("net saved"));
        assert!(!stdout.contains("quiet.jpg"));

        let optimized = fs::read(&jpeg).unwrap();
        assert!(optimized.len() < input.len());

        let second = peel().arg(flag).arg(&jpeg).output().unwrap();
        assert_success(&second);
        let second_stdout = String::from_utf8(second.stdout).unwrap();
        assert!(second_stdout.starts_with("Done: 0 optimized, 1 unchanged"));
    }
}
