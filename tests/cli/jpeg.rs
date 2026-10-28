use jpeg_decoder::Decoder as JpegDecoder;

pub(super) fn sample_jpeg() -> &'static [u8] {
    include_bytes!("../fixtures/sample.jpg")
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
