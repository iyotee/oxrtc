use super::*;

/// The header openIVR's sound de-duplicator writes (`audio_dedup.py`
/// `wav_header`): RIFF and data sizes at their maximum, a 16-byte `fmt `.
fn dedup_header(tag: u16, rate: u32, channels: u16) -> Vec<u8> {
    let mut out = b"RIFF\xff\xff\xff\xffWAVEfmt \x10\x00\x00\x00".to_vec();
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(channels)).to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(b"data\xff\xff\xff\xff");
    out
}

#[test]
fn the_de_duplicators_stream_is_read_as_it_comes() {
    let mut stream = dedup_header(6, 8000, 1);
    stream.extend_from_slice(&[0x55, 0xD5, 0x2A]);
    let mut reader = Reader::new();
    let mut samples = Vec::new();
    for byte in &stream {
        reader.push(std::slice::from_ref(byte), &mut samples).unwrap();
    }
    assert_eq!(reader.format(), Some(Format { encoding: Encoding::Alaw, rate: 8000, channels: 1 }));
    assert_eq!(samples, [0x55, 0xD5, 0x2A]);
    assert!(!reader.finished());
    let mut more = Vec::new();
    reader.push(&[1, 2, 3], &mut more).unwrap();
    assert_eq!(more, [1, 2, 3]);

    let mut reader = Reader::new();
    let mut samples = Vec::new();
    reader.push(&dedup_header(7, 16_000, 1), &mut samples).unwrap();
    assert_eq!(reader.format().map(|f| (f.encoding, f.rate)), Some((Encoding::Ulaw, 16_000)));
}

#[test]
fn pcm_is_cut_on_whole_frames_and_a_finite_data_chunk_ends() {
    let mut stream = b"RIFF\x00\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x02\x00\x80\xbb\x00\x00\x00\xee\x02\x00\x04\x00\x10\x00LIST\x03\x00\x00\x00abc\x00data\x0a\x00\x00\x00".to_vec();
    stream.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    let mut reader = Reader::new();
    let mut samples = Vec::new();
    reader.push(stream.get(..stream.len() - 7).unwrap(), &mut samples).unwrap();
    assert_eq!(reader.format(), Some(Format { encoding: Encoding::Pcm16, rate: 48_000, channels: 2 }));
    assert_eq!(samples, [1, 2, 3, 4], "a frame is 4 bytes; the 5th waits");
    assert_eq!(reader.held(), 1);
    reader.push(stream.get(stream.len() - 7..).unwrap(), &mut samples).unwrap();
    // 10 bytes of data: two whole frames, the last 2 bytes are not a frame.
    assert_eq!(samples, [1, 2, 3, 4, 5, 6, 7, 8]);
    assert!(reader.finished());
    reader.push(&[9; 10], &mut samples).unwrap();
    assert_eq!(samples.len(), 8, "nothing after the data chunk");
}

#[test]
fn the_extensible_format_names_its_coding_in_its_sub_format() {
    let mut stream = b"RIFF\xff\xff\xff\xffWAVEfmt \x28\x00\x00\x00\xfe\xff\x01\x00\x40\x1f\x00\x00\x80\x3e\x00\x00\x02\x00\x10\x00\x16\x00\x10\x00\x04\x00\x00\x00\x01\x00".to_vec();
    stream.extend_from_slice(&[0; 14]);
    stream.extend_from_slice(b"data\x00\x00\x00\x00\x01\x00");
    let mut reader = Reader::new();
    let mut samples = Vec::new();
    reader.push(&stream, &mut samples).unwrap();
    assert_eq!(reader.format().map(|f| f.encoding), Some(Encoding::Pcm16));
    assert_eq!(samples, [1, 0], "a data size of 0 is a live stream's: no end");
}

#[test]
fn what_is_not_taken_is_refused_once() {
    let refused = |stream: &[u8]| {
        let mut reader = Reader::new();
        let error = reader.push(stream, &mut Vec::new()).unwrap_err();
        assert!(reader.finished());
        assert_eq!(reader.held(), 0);
        error
    };
    assert_eq!(refused(b"RIFX"), WavError::NotWav);
    assert_eq!(refused(b"RIFF\0\0\0\0AVI "), WavError::NotWav);
    assert!(matches!(refused(&dedup_header(85, 8000, 1)), WavError::Unsupported(_)), "MP3");
    assert!(matches!(refused(&dedup_header(6, 8000, 9)), WavError::Unsupported(_)));
    assert!(matches!(refused(&dedup_header(6, 500, 1)), WavError::Unsupported(_)));
    assert_eq!(refused(b"RIFF\0\0\0\0WAVEdata\0\0\0\0"), WavError::Malformed("a data chunk before the fmt chunk"));
    assert_eq!(refused(b"RIFF\0\0\0\0WAVELIST\0\x10\0\0"), WavError::HeaderTooLong);
    let mut fmt_short = b"RIFF\0\0\0\0WAVEfmt \x04\0\0\0\x06\0\x01\0".to_vec();
    fmt_short.extend_from_slice(b"data\0\0\0\0");
    assert!(matches!(refused(&fmt_short), WavError::Malformed(_)));
}

#[test]
fn a_header_that_never_ends_is_refused_at_its_bound() {
    let mut reader = Reader::new();
    let mut stream = b"RIFF\0\0\0\0WAVE".to_vec();
    // Small chunks, one after the other, never a data chunk.
    while stream.len() < MAX_HEADER_BYTES + 100 {
        stream.extend_from_slice(b"junk\x02\0\0\0ab");
    }
    let mut result = Ok(());
    for piece in stream.chunks(100) {
        result = reader.push(piece, &mut Vec::new());
        assert!(reader.held() <= MAX_HEADER_BYTES);
        if result.is_err() {
            break;
        }
    }
    assert_eq!(result, Err(WavError::HeaderTooLong));
}
