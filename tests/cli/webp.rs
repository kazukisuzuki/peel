use std::fs;

use crate::common::{assert_success, peel};

pub(super) fn riff_chunk(kind: [u8; 4], payload: &[u8], padding: u8) -> Vec<u8> {
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

pub(super) fn make_webp(chunks: &[Vec<u8>], trailing: &[u8]) -> Vec<u8> {
    let body = join_chunks(b"WEBP", chunks);
    let mut bytes = Vec::with_capacity(body.len() + trailing.len() + 8);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(trailing);
    bytes
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
