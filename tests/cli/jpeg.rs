use std::sync::OnceLock;

use jpeg_decoder::Decoder as JpegDecoder;
use jpeg_encoder::{ColorType, Encoder as JpegEncoder};

pub(super) fn sample_jpeg() -> &'static [u8] {
    static JPEG: OnceLock<Vec<u8>> = OnceLock::new();

    JPEG.get_or_init(|| {
        const WIDTH: u16 = 64;
        const HEIGHT: u16 = 64;
        let mut pixels = Vec::with_capacity(usize::from(WIDTH) * usize::from(HEIGHT) * 3);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                pixels.extend_from_slice(&[(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8]);
            }
        }

        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg, 90)
            .encode(&pixels, WIDTH, HEIGHT, ColorType::Rgb)
            .expect("test JPEG should encode");
        jpeg
    })
}

pub(super) fn decode_jpeg(jpeg: &[u8]) -> Vec<u8> {
    let mut decoder = JpegDecoder::new(jpeg);
    decoder.decode().expect("test JPEG should decode")
}

pub(super) fn jpeg_with_metadata() -> Vec<u8> {
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
