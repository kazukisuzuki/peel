use std::borrow::Cow;
use std::fs;
use std::io::Cursor;

use crate::common::{assert_failure, assert_second_run_unchanged, assert_success, peel};

pub(super) fn make_png() -> (Vec<u8>, Vec<u8>) {
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

pub(super) fn decode_png(bytes: &[u8]) -> Vec<u8> {
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

#[derive(Clone, Copy)]
struct PngChunk {
    start: usize,
    kind: [u8; 4],
    data_start: usize,
    data_end: usize,
}

impl PngChunk {
    fn data(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.data_start..self.data_end]
    }
}

fn scan_png_chunks(bytes: &[u8]) -> Vec<PngChunk> {
    let mut chunks = Vec::new();
    let mut position = 8;
    while position < bytes.len() {
        let length = u32::from_be_bytes(bytes[position..position + 4].try_into().unwrap()) as usize;
        let kind = bytes[position + 4..position + 8].try_into().unwrap();
        let data_start = position + 8;
        let data_end = data_start + length;
        chunks.push(PngChunk {
            start: position,
            kind,
            data_start,
            data_end,
        });
        position = data_end + 4;
        if kind == *b"IEND" {
            assert_eq!(position, bytes.len(), "PNG has trailing bytes after IEND");
            break;
        }
    }
    chunks
}

fn png_chunks(bytes: &[u8]) -> Vec<([u8; 4], &[u8])> {
    scan_png_chunks(bytes)
        .into_iter()
        .map(|chunk| (chunk.kind, chunk.data(bytes)))
        .collect()
}

fn find_chunk(bytes: &[u8], wanted: [u8; 4], occurrence: usize) -> (usize, usize, usize) {
    let chunk = scan_png_chunks(bytes)
        .into_iter()
        .filter(|chunk| chunk.kind == wanted)
        .nth(occurrence)
        .expect("requested PNG chunk was not found");
    (chunk.start, chunk.data_start, chunk.data_end)
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
        assert!(png_chunks(&optimized).iter().all(|(kind, _)| matches!(
            kind,
            b"IHDR" | b"PLTE" | b"tRNS" | b"IDAT" | b"IEND" | b"acTL" | b"fcTL" | b"fdAT"
        )));
    }

    let paths = targets
        .iter()
        .map(|(path, _)| path.as_path())
        .collect::<Vec<_>>();
    assert_second_run_unchanged(&paths);
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
