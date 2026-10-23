use std::borrow::Cow;
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
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().unwrap();
    let mut output = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut output).unwrap();
    output.truncate(info.buffer_size());
    output
}

fn decode_png_canonical(bytes: &[u8]) -> Vec<[u16; 4]> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().unwrap();
    let mut output = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut output).unwrap();
    output.truncate(info.buffer_size());

    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => panic!("EXPAND must resolve indexed PNG data"),
    };
    let byte_depth = match info.bit_depth {
        png::BitDepth::Eight => 1,
        png::BitDepth::Sixteen => 2,
        depth => panic!("EXPAND left unsupported bit depth {depth:?}"),
    };
    let sample = |offset: usize| {
        if byte_depth == 1 {
            u16::from(output[offset]) * 257
        } else {
            u16::from_be_bytes([output[offset], output[offset + 1]])
        }
    };

    let mut pixels = Vec::with_capacity(info.width as usize * info.height as usize);
    for y in 0..info.height as usize {
        for x in 0..info.width as usize {
            let offset = y * info.line_size + x * channels * byte_depth;
            let pixel = match info.color_type {
                png::ColorType::Grayscale => {
                    let gray = sample(offset);
                    [gray, gray, gray, u16::MAX]
                }
                png::ColorType::Rgb => [
                    sample(offset),
                    sample(offset + byte_depth),
                    sample(offset + 2 * byte_depth),
                    u16::MAX,
                ],
                png::ColorType::GrayscaleAlpha => {
                    let gray = sample(offset);
                    [gray, gray, gray, sample(offset + byte_depth)]
                }
                png::ColorType::Rgba => [
                    sample(offset),
                    sample(offset + byte_depth),
                    sample(offset + 2 * byte_depth),
                    sample(offset + 3 * byte_depth),
                ],
                png::ColorType::Indexed => unreachable!(),
            };
            pixels.push(pixel);
        }
    }
    pixels
}

fn encode_png(info: png::Info<'static>, pixels: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let encoder = png::Encoder::with_info(&mut bytes, info).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
    }
    bytes
}

fn png_chunks(bytes: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut chunks = Vec::new();
    let mut position = 8;
    while position < bytes.len() {
        let length = u32::from_be_bytes(bytes[position..position + 4].try_into().unwrap()) as usize;
        let kind = bytes[position + 4..position + 8].try_into().unwrap();
        let data_start = position + 8;
        let data_end = data_start + length;
        chunks.push((kind, &bytes[data_start..data_end]));
        position = data_end + 4;
        if kind == *b"IEND" {
            assert_eq!(position, bytes.len(), "PNG has trailing bytes after IEND");
            break;
        }
    }
    chunks
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
        concat!("peel ", env!("CARGO_PKG_VERSION"))
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

#[test]
fn normalizes_interlace_without_changing_transparent_rgb() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("interlaced.png");
    let pixels = vec![
        1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 128, 10, 11, 12, 255, 13, 14, 15, 255, 16, 17, 18, 255,
    ];
    let mut info = png::Info::with_size(3, 2);
    info.color_type = png::ColorType::Rgba;
    info.bit_depth = png::BitDepth::Eight;
    let non_interlaced = encode_png(info, &pixels);
    let mut interlace_options = oxipng::Options::from_preset(2);
    interlace_options.interlace = Some(true);
    interlace_options.optimize_alpha = false;
    interlace_options.force = true;
    let input = oxipng::optimize_from_memory(&non_interlaced, &interlace_options).unwrap();
    assert_eq!(
        png_chunks(&input)
            .into_iter()
            .find(|(kind, _)| kind == b"IHDR")
            .unwrap()
            .1[12],
        1
    );
    fs::write(&target, &input).unwrap();

    let output = peel().arg(&target).output().unwrap();
    assert_success(&output);
    let optimized = fs::read(&target).unwrap();
    let chunks = png_chunks(&optimized);
    let ihdr = chunks.iter().find(|(kind, _)| kind == b"IHDR").unwrap().1;
    assert_eq!(ihdr[12], 0, "output should be non-interlaced");
    assert_eq!(decode_png(&optimized), pixels);
    assert!(
        chunks
            .iter()
            .filter(|(kind, _)| kind[0] & 0x20 != 0)
            .all(|(kind, _)| kind == b"tRNS")
    );

    let first_output = optimized;
    let second = peel().arg(&target).output().unwrap();
    assert_success(&second);
    assert!(String::from_utf8_lossy(&second.stdout).contains("Done: 0 optimized, 1 unchanged"));
    assert_eq!(fs::read(target).unwrap(), first_output);
}

#[test]
fn removes_unused_and_duplicate_palette_entries_without_merging_transparent_rgb() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("palette.png");
    let mut info = png::Info::with_size(4, 1);
    info.color_type = png::ColorType::Indexed;
    info.bit_depth = png::BitDepth::Four;
    info.palette = Some(Cow::Owned(vec![
        200, 10, 20, // used opaque red
        200, 10, 20, // used exact duplicate
        1, 2, 3, // used transparent color
        4, 5, 6, // used different transparent color
        90, 100, 110, // unused
    ]));
    info.trns = Some(Cow::Owned(vec![255, 255, 0, 0, 255]));
    let input = encode_png(info, &[0x01, 0x23]);
    let expected = decode_png_canonical(&input);
    fs::write(&target, &input).unwrap();

    let output = peel().arg(&target).output().unwrap();
    assert_success(&output);
    let optimized = fs::read(&target).unwrap();
    assert_eq!(decode_png_canonical(&optimized), expected);
    assert_eq!(
        &expected[2..],
        &[[257, 514, 771, 0], [1028, 1285, 1542, 0],],
        "distinct RGB values under alpha zero must survive"
    );

    let chunks = png_chunks(&optimized);
    if let Some((_, palette)) = chunks.iter().find(|(kind, _)| kind == b"PLTE") {
        assert_eq!(palette.len() % 3, 0);
        assert!(
            palette.len() / 3 <= 3,
            "unused and exactly duplicate colors should not remain"
        );
    }
}

#[test]
fn removes_unused_truecolor_palette_and_trns() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("unused-truecolor-data.png");
    let mut info = png::Info::with_size(2, 1);
    info.color_type = png::ColorType::Rgb;
    info.bit_depth = png::BitDepth::Eight;
    info.palette = Some(Cow::Owned(vec![255, 0, 0, 0, 255, 0]));
    info.trns = Some(Cow::Owned(vec![0, 9, 0, 9, 0, 9]));
    let input = encode_png(info, &[1, 1, 1, 2, 2, 2]);
    let expected = decode_png_canonical(&input);
    fs::write(&target, &input).unwrap();

    let output = peel().arg(&target).output().unwrap();
    assert_success(&output);
    let optimized = fs::read(&target).unwrap();
    assert_eq!(decode_png_canonical(&optimized), expected);
    let chunks = png_chunks(&optimized);
    assert!(!chunks.iter().any(|(kind, _)| kind == b"PLTE"));
    assert!(!chunks.iter().any(|(kind, _)| kind == b"tRNS"));
}

#[test]
fn reduces_only_exactly_representable_16_bit_samples() {
    let directory = tempfile::tempdir().unwrap();
    let exact = directory.path().join("exact-16.png");
    let precise = directory.path().join("precise-16.png");

    let mut info = png::Info::with_size(2, 1);
    info.color_type = png::ColorType::Grayscale;
    info.bit_depth = png::BitDepth::Sixteen;
    let exact_input = encode_png(info.clone(), &[0x12, 0x12, 0x34, 0x34]);
    let precise_input = encode_png(info, &[0x12, 0x12, 0x12, 0x34]);
    let exact_pixels = decode_png_canonical(&exact_input);
    let precise_pixels = decode_png_canonical(&precise_input);
    fs::write(&exact, exact_input).unwrap();
    fs::write(&precise, precise_input).unwrap();

    let output = peel().arg(&exact).arg(&precise).output().unwrap();
    assert_success(&output);
    let exact_output = fs::read(&exact).unwrap();
    let precise_output = fs::read(&precise).unwrap();
    assert_eq!(decode_png_canonical(&exact_output), exact_pixels);
    assert_eq!(decode_png_canonical(&precise_output), precise_pixels);

    let exact_ihdr = png_chunks(&exact_output)
        .into_iter()
        .find(|(kind, _)| kind == b"IHDR")
        .unwrap()
        .1;
    let precise_ihdr = png_chunks(&precise_output)
        .into_iter()
        .find(|(kind, _)| kind == b"IHDR")
        .unwrap()
        .1;
    assert_ne!(exact_ihdr[8], 16);
    assert_eq!(precise_ihdr[8], 16, "scale_16 must remain disabled");
}

#[test]
fn clears_non_pixel_bits_at_the_end_of_sub_byte_rows() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("padding.png");
    let mut info = png::Info::with_size(1, 1);
    info.color_type = png::ColorType::Grayscale;
    info.bit_depth = png::BitDepth::One;
    let input = encode_png(info, &[0xff]);
    let input_data =
        oxipng::internal_tests::PngData::from_slice(&input, &oxipng::Options::default()).unwrap();
    assert_ne!(input_data.raw.data[0] & 0x7f, 0);
    let expected = decode_png_canonical(&input);
    fs::write(&target, input).unwrap();

    let output = peel().arg(&target).output().unwrap();
    assert_success(&output);
    let optimized = fs::read(&target).unwrap();
    assert_eq!(decode_png_canonical(&optimized), expected);
    let decoded =
        oxipng::internal_tests::PngData::from_slice(&optimized, &oxipng::Options::default())
            .unwrap();
    let depth = decoded.raw.ihdr.bit_depth as u8;
    if depth < 8 {
        let padding_mask = (1u8 << (8 - depth)) - 1;
        assert_eq!(decoded.raw.data[0] & padding_mask, 0);
    }
}
