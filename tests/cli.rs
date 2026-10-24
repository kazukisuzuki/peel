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

fn decode_png_identity(bytes: &[u8]) -> (png::OutputInfo, Vec<u8>) {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().unwrap();
    let mut output = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut output).unwrap();
    output.truncate(info.buffer_size());
    (info, output)
}

fn decode_png_canonical(bytes: &[u8]) -> Vec<[u16; 4]> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().unwrap();
    let mut output = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut output).unwrap();
    output.truncate(info.buffer_size());

    canonical_pixels_from_output(&info, &output)
}

fn canonical_pixels_from_output(info: &png::OutputInfo, output: &[u8]) -> Vec<[u16; 4]> {
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

#[derive(Debug, Eq, PartialEq)]
struct FrameSnapshot {
    control: Option<FrameControlSnapshot>,
    width: u32,
    height: u32,
    pixels: Vec<[u16; 4]>,
}

#[derive(Debug, Eq, PartialEq)]
struct FrameControlSnapshot {
    width: u32,
    height: u32,
    x_offset: u32,
    y_offset: u32,
    delay_num: u16,
    delay_den: u16,
    dispose_op: png::DisposeOp,
    blend_op: png::BlendOp,
}

#[derive(Debug, Eq, PartialEq)]
struct ApngSnapshot {
    width: u32,
    height: u32,
    num_frames: u32,
    num_plays: u32,
    default_image_in_animation: bool,
    frames: Vec<FrameSnapshot>,
}

fn decode_apng_snapshot(bytes: &[u8]) -> ApngSnapshot {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().unwrap();
    let info = reader.info();
    let animation = info
        .animation_control
        .expect("expected APNG animation control");
    let default_image_in_animation = info.frame_control.is_some();
    let image_count = animation.num_frames + u32::from(!default_image_in_animation);
    let width = info.width;
    let height = info.height;
    let mut output = vec![0; reader.output_buffer_size().unwrap()];
    let mut frames = Vec::new();
    for _ in 0..image_count {
        let output_info = reader.next_frame(&mut output).unwrap();
        let control = reader
            .info()
            .frame_control
            .map(|control| FrameControlSnapshot {
                width: control.width,
                height: control.height,
                x_offset: control.x_offset,
                y_offset: control.y_offset,
                delay_num: control.delay_num,
                delay_den: control.delay_den,
                dispose_op: control.dispose_op,
                blend_op: control.blend_op,
            });
        frames.push(FrameSnapshot {
            control,
            width: output_info.width,
            height: output_info.height,
            pixels: canonical_pixels_from_output(&output_info, &output),
        });
    }
    assert!(reader.next_frame(&mut output).is_err());
    ApngSnapshot {
        width,
        height,
        num_frames: animation.num_frames,
        num_plays: animation.num_plays,
        default_image_in_animation,
        frames,
    }
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

fn configure_apng_frame<W: std::io::Write>(
    writer: &mut png::Writer<W>,
    dimensions: (u32, u32),
    position: (u32, u32),
    delay: (u16, u16),
    dispose_op: png::DisposeOp,
    blend_op: png::BlendOp,
) {
    writer.reset_frame_position().unwrap();
    writer
        .set_frame_dimension(dimensions.0, dimensions.1)
        .unwrap();
    writer.set_frame_position(position.0, position.1).unwrap();
    writer.set_frame_delay(delay.0, delay.1).unwrap();
    writer.set_dispose_op(dispose_op).unwrap();
    writer.set_blend_op(blend_op).unwrap();
}

fn make_apng(separate_default_image: bool, with_cleanup: bool) -> Vec<u8> {
    let default_image = [
        1, 2, 3, 0, 10, 20, 30, 255, 40, 50, 60, 128, 4, 5, 6, 0, 70, 80, 90, 255, 100, 110, 120,
        64, 7, 8, 9, 0, 130, 140, 150, 255, 160, 170, 180, 255,
    ];
    let first_subframe = [
        11, 12, 13, 0, 20, 21, 22, 255, 31, 32, 33, 128, 41, 42, 43, 255,
    ];
    let second_subframe = [51, 52, 53, 0, 61, 62, 63, 200];
    let third_subframe = [71, 72, 73, 0, 81, 82, 83, 255];

    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 3, 3);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(3, 7).unwrap();
        encoder.set_sep_def_img(separate_default_image).unwrap();
        encoder.set_frame_delay(1, 10).unwrap();
        encoder.set_dispose_op(png::DisposeOp::None).unwrap();
        encoder.set_blend_op(png::BlendOp::Source).unwrap();
        if with_cleanup {
            encoder
                .add_text_chunk("peel".into(), "remove this metadata".into())
                .unwrap();
        }
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&default_image).unwrap();

        if separate_default_image {
            configure_apng_frame(
                &mut writer,
                (2, 2),
                (1, 0),
                (1, 10),
                png::DisposeOp::None,
                png::BlendOp::Source,
            );
            writer.write_image_data(&first_subframe).unwrap();
        }
        configure_apng_frame(
            &mut writer,
            (1, 2),
            (0, 1),
            (2, 25),
            png::DisposeOp::Background,
            png::BlendOp::Over,
        );
        writer.write_image_data(&second_subframe).unwrap();
        configure_apng_frame(
            &mut writer,
            (2, 1),
            (1, 2),
            (3, 100),
            png::DisposeOp::Previous,
            png::BlendOp::Source,
        );
        writer.write_image_data(&third_subframe).unwrap();
        writer.finish().unwrap();
    }
    if with_cleanup {
        bytes.extend_from_slice(b"private-apng-suffix");
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

fn find_chunk(bytes: &[u8], wanted: [u8; 4], occurrence: usize) -> (usize, usize, usize) {
    let mut position = 8;
    let mut found = 0;
    loop {
        let length = u32::from_be_bytes(bytes[position..position + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = bytes[position + 4..position + 8].try_into().unwrap();
        let data_start = position + 8;
        let data_end = data_start + length;
        if kind == wanted {
            if found == occurrence {
                return (position, data_start, data_end);
            }
            found += 1;
        }
        position = data_end + 4;
        assert_ne!(kind, *b"IEND", "requested PNG chunk was not found");
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xedb8_8320
            };
        }
    }
    !crc
}

fn mutate_chunk(
    bytes: &mut [u8],
    kind: [u8; 4],
    occurrence: usize,
    mutate: impl FnOnce(&mut [u8]),
) {
    let (start, data_start, data_end) = find_chunk(bytes, kind, occurrence);
    mutate(&mut bytes[data_start..data_end]);
    let crc = crc32(&bytes[start + 4..data_end]).to_be_bytes();
    bytes[data_end..data_end + 4].copy_from_slice(&crc);
}

fn assert_sanitized_apng(bytes: &[u8]) {
    let chunks = png_chunks(bytes);
    let mut declared_frames = None;
    let mut frame_controls = 0u32;
    let mut expected_sequence = 0u64;
    let mut saw_fdat = false;
    for (kind, data) in &chunks {
        assert!(
            matches!(
                kind,
                b"IHDR" | b"PLTE" | b"tRNS" | b"IDAT" | b"IEND" | b"acTL" | b"fcTL" | b"fdAT"
            ),
            "unexpected chunk in sanitized APNG: {}",
            String::from_utf8_lossy(kind)
        );
        match kind {
            b"acTL" => {
                assert!(declared_frames.is_none());
                declared_frames = Some(u32::from_be_bytes(data[0..4].try_into().unwrap()));
            }
            b"fcTL" => {
                frame_controls += 1;
                let sequence = u32::from_be_bytes(data[0..4].try_into().unwrap());
                assert_eq!(u64::from(sequence), expected_sequence);
                expected_sequence += 1;
            }
            b"fdAT" => {
                saw_fdat = true;
                let sequence = u32::from_be_bytes(data[0..4].try_into().unwrap());
                assert_eq!(u64::from(sequence), expected_sequence);
                expected_sequence += 1;
            }
            _ => {}
        }
    }
    assert_eq!(declared_frames, Some(frame_controls));
    assert!(saw_fdat);
}

fn riff_chunk(kind: [u8; 4], payload: &[u8], padding: u8) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(8 + payload.len() + payload.len() % 2);
    chunk.extend_from_slice(&kind);
    chunk.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    chunk.extend_from_slice(payload);
    if !payload.len().is_multiple_of(2) {
        chunk.push(padding);
    }
    chunk
}

fn join_chunks(prefix: &[u8], chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = prefix.to_vec();
    for chunk in chunks {
        bytes.extend_from_slice(chunk);
    }
    bytes
}

fn make_webp(chunks: &[Vec<u8>], trailing: &[u8]) -> Vec<u8> {
    let body = join_chunks(b"WEBP", chunks);
    let mut bytes = Vec::with_capacity(body.len() + trailing.len() + 8);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(trailing);
    bytes
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

    let paths = [&jpeg, &png, &webp];
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
    assert!(String::from_utf8_lossy(&second.stdout).contains("Done: 0 optimized, 3 unchanged"));
    for (path, (bytes, modified)) in paths.iter().zip(before_second_run) {
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
    }
}

#[test]
fn optimizes_animated_webp_without_changing_animation_data() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("animated-sample.webp");
    let alpha = [0xd5, 0x51, 0x52, 0x53];
    let frame_1 = [0x10, 0, 0, 0x9d, 0x01, 0x2a, 2, 0, 2, 0, 0xa1];
    let frame_2 = [0x30, 0, 0, 0x9d, 0x01, 0x2a, 2, 0, 2, 0, 0xb2];
    let header_1 = [0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 100, 0, 0, 0xfe];
    let header_2 = [1, 0, 0, 1, 0, 0, 1, 0, 0, 1, 0, 0, 250, 0, 0, 0xfd];
    let input_frame_1 = join_chunks(
        &header_1,
        &[
            riff_chunk(*b"ALPH", &alpha, 0),
            riff_chunk(*b"VP8 ", &frame_1, 0xa1),
            riff_chunk(*b"JUNK", b"x", 0xa2),
        ],
    );
    let input_frame_2 = join_chunks(&header_2, &[riff_chunk(*b"VP8 ", &frame_2, 0xa3)]);
    let input = make_webp(
        &[
            riff_chunk(*b"VP8X", &[0xd3, 1, 2, 3, 3, 0, 0, 3, 0, 0], 0),
            riff_chunk(*b"ANIM", &[1, 2, 3, 4, 7, 0], 0),
            riff_chunk(*b"ANMF", &input_frame_1, 0),
            riff_chunk(*b"ANMF", &input_frame_2, 0),
            riff_chunk(*b"NOTE", b"x", 0xa4),
        ],
        &[],
    );

    let mut clean_header_1 = header_1;
    clean_header_1[15] = 0x02;
    let mut clean_header_2 = header_2;
    clean_header_2[15] = 0x01;
    let expected_frame_1 = join_chunks(
        &clean_header_1,
        &[
            riff_chunk(*b"ALPH", &[0x15, 0x51, 0x52, 0x53], 0),
            riff_chunk(*b"VP8 ", &frame_1, 0),
        ],
    );
    let expected_frame_2 = join_chunks(&clean_header_2, &[riff_chunk(*b"VP8 ", &frame_2, 0)]);
    let expected = make_webp(
        &[
            riff_chunk(*b"VP8X", &[0x12, 0, 0, 0, 3, 0, 0, 3, 0, 0], 0),
            riff_chunk(*b"ANIM", &[1, 2, 3, 4, 7, 0], 0),
            riff_chunk(*b"ANMF", &expected_frame_1, 0),
            riff_chunk(*b"ANMF", &expected_frame_2, 0),
        ],
        &[],
    );
    fs::write(&target, input).unwrap();

    let output = peel()
        .current_dir(directory.path())
        .arg("animated-*.webp")
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(fs::read(target).unwrap(), expected);
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
    assert!(fs::metadata(&valid).unwrap().len() < sample_jpeg().len() as u64);

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
fn optimizes_apng_without_changing_animation_data() {
    let directory = tempfile::tempdir().unwrap();
    let targets = [
        (directory.path().join("included.png"), false),
        (directory.path().join("separate.apng"), true),
    ];
    let expected = targets
        .iter()
        .map(|(_, separate)| decode_apng_snapshot(&make_apng(*separate, false)))
        .collect::<Vec<_>>();
    for (target, separate) in &targets {
        fs::write(target, make_apng(*separate, true)).unwrap();
    }

    let output = peel()
        .args(targets.iter().map(|(path, _)| path))
        .output()
        .unwrap();
    assert_success(&output);
    for ((target, _), expected) in targets.iter().zip(&expected) {
        let optimized = fs::read(target).unwrap();
        assert_eq!(&decode_apng_snapshot(&optimized), expected);
        assert_sanitized_apng(&optimized);
    }

    let before_second_run = targets
        .iter()
        .map(|(path, _)| {
            (
                fs::read(path).unwrap(),
                fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let second = peel()
        .args(targets.iter().map(|(path, _)| path))
        .output()
        .unwrap();
    assert_success(&second);
    for ((target, _), (bytes, modified)) in targets.iter().zip(before_second_run) {
        assert_eq!(fs::read(target).unwrap(), bytes);
        assert_eq!(fs::metadata(target).unwrap().modified().unwrap(), modified);
    }

    let glob_target = directory.path().join("glob-animation.apng");
    fs::write(&glob_target, make_apng(false, true)).unwrap();
    let glob = peel()
        .current_dir(directory.path())
        .arg("glob-*.apng")
        .output()
        .unwrap();
    assert_success(&glob);
    assert_sanitized_apng(&fs::read(glob_target).unwrap());
}

#[test]
fn rejects_malformed_apng_without_replacing_it() {
    let directory = tempfile::tempdir().unwrap();
    let valid = make_apng(false, false);
    let mut cases = Vec::new();

    let mut bad_crc = valid.clone();
    let (_, data_start, _) = find_chunk(&bad_crc, *b"acTL", 0);
    bad_crc[data_start + 7] ^= 1;
    cases.push(("bad-crc", bad_crc));

    let mut duplicate_sequence = valid.clone();
    mutate_chunk(&mut duplicate_sequence, *b"fdAT", 0, |data| {
        data[..4].copy_from_slice(&1u32.to_be_bytes());
    });
    cases.push(("duplicate-sequence", duplicate_sequence));

    let mut frame_count_mismatch = valid.clone();
    mutate_chunk(&mut frame_count_mismatch, *b"acTL", 0, |data| {
        data[..4].copy_from_slice(&4u32.to_be_bytes());
    });
    cases.push(("frame-count-mismatch", frame_count_mismatch));

    let mut outside_canvas = valid.clone();
    mutate_chunk(&mut outside_canvas, *b"fcTL", 1, |data| {
        data[4..8].copy_from_slice(&4u32.to_be_bytes());
    });
    cases.push(("outside-canvas", outside_canvas));

    let mut truncated_fdat = valid;
    let (_, data_start, _) = find_chunk(&truncated_fdat, *b"fdAT", 0);
    truncated_fdat.truncate(data_start + 5);
    cases.push(("truncated-fdat", truncated_fdat));

    for (name, bytes) in cases {
        let target = directory.path().join(format!("{name}.apng"));
        fs::write(&target, &bytes).unwrap();
        let output = peel().arg(&target).output().unwrap();
        assert_failure(&output);
        assert_eq!(
            fs::read(&target).unwrap(),
            bytes,
            "malformed APNG {name} was replaced"
        );
    }
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
    let (_, input_data) = decode_png_identity(&input);
    assert_ne!(input_data[0] & 0x7f, 0);
    let expected = decode_png_canonical(&input);
    fs::write(&target, input).unwrap();

    let output = peel().arg(&target).output().unwrap();
    assert_success(&output);
    let optimized = fs::read(&target).unwrap();
    assert_eq!(decode_png_canonical(&optimized), expected);
    let (info, decoded) = decode_png_identity(&optimized);
    let depth = info.bit_depth as u8;
    if depth < 8 {
        let channels = match info.color_type {
            png::ColorType::Grayscale | png::ColorType::Indexed => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
        };
        let used_bits = info.width as usize * channels * usize::from(depth);
        let padding_bits = (8 - used_bits % 8) % 8;
        if padding_bits != 0 {
            let padding_mask = (1u8 << padding_bits) - 1;
            for row in 0..info.height as usize {
                assert_eq!(decoded[(row + 1) * info.line_size - 1] & padding_mask, 0);
            }
        }
    }
}
