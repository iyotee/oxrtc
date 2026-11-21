use super::*;

#[test]
fn the_crcs_are_flacs() {
    // The catalogue's check values: CRC-8/SMBUS and CRC-16/UMTS.
    assert_eq!(crc8(b"123456789"), 0xf4);
    assert_eq!(crc16(b"123456789"), 0xfee8);
}

#[test]
fn sample_numbers_take_flacs_utf8_coding() {
    let coded = |n| {
        let mut out = Vec::new();
        coded_number(n, &mut out);
        out
    };
    assert_eq!(coded(0), vec![0]);
    assert_eq!(coded(0x7f), vec![0x7f]);
    assert_eq!(coded(0x80), vec![0xc2, 0x80]);
    assert_eq!(coded(0x7ff), vec![0xdf, 0xbf]);
    assert_eq!(coded(0x800), vec![0xe0, 0xa0, 0x80]);
    assert_eq!(coded(0x7fff_ffff), vec![0xfd, 0xbf, 0xbf, 0xbf, 0xbf, 0xbf]);
    assert_eq!(coded((1 << 36) - 1), vec![0xfe, 0xbf, 0xbf, 0xbf, 0xbf, 0xbf, 0xbf]);
    assert_eq!(coded(1 << 36), vec![0], "wraps at 36 bits");
}

#[test]
fn a_frame_is_a_header_verbatim_subframes_and_its_crc() {
    let mut framer = Framer::new(1, 8000).unwrap();
    let samples: Vec<i16> = (0..160).map(|i| (i * 100 - 8000) as i16).collect();
    let (frame, block) = framer.frame(&samples).unwrap();
    assert_eq!(block, 160);
    // Sync, block size from an 8-bit field, 8 kHz, mono, 16 bits, sample 0, 159.
    assert_eq!(&frame[..6], &[0xff, 0xf9, 0x64, 0x08, 0x00, 159]);
    assert_eq!(frame[6], crc8(&frame[..6]));
    assert_eq!(frame[7], 0x02, "verbatim");
    let body = &frame[8..8 + 320];
    assert_eq!(
        &body[..4],
        &(-8000i16).to_be_bytes().iter().chain(&(-7900i16).to_be_bytes()).copied().collect::<Vec<_>>()[..]
    );
    assert_eq!(frame.len(), 8 + 320 + 2);
    let crc = u16::from_be_bytes([frame[328], frame[329]]);
    assert_eq!(crc, crc16(&frame[..328]));
    // The next frame starts at sample 160, coded on two bytes.
    let (next, _) = framer.frame(&samples).unwrap();
    assert_eq!(&next[4..6], &[0xc2, 0xa0]);
}

#[test]
fn stereo_is_two_subframes_and_a_long_block_a_16_bit_field() {
    let mut framer = Framer::new(2, 44_100).unwrap();
    let samples: Vec<i16> = (0..600).map(|i| if i % 2 == 0 { 1 } else { -1 }).collect();
    let (frame, block) = framer.frame(&samples).unwrap();
    assert_eq!(block, 300);
    assert_eq!(&frame[..7], &[0xff, 0xf9, 0x79, 0x18, 0x00, 0x01, 0x2b]);
    let left = &frame[8..9 + 600];
    assert_eq!(left[0], 0x02);
    assert!(left[1..].chunks(2).all(|s| s == [0, 1]));
    let right = &frame[9 + 600..10 + 1200];
    assert_eq!(right[0], 0x02);
    assert!(right[1..].chunks(2).all(|s| s == [0xff, 0xff]));
    assert_eq!(frame.len(), 8 + 2 * 601 + 2);
}

#[test]
fn what_flac_cannot_hold_is_refused_and_changes_nothing() {
    assert!(Framer::new(0, 8000).is_err());
    assert!(Framer::new(9, 8000).is_err());
    assert!(Framer::new(1, 1 << 20).is_err());
    let mut framer = Framer::new(2, 8000).unwrap();
    assert!(framer.frame(&[]).is_err());
    assert!(framer.frame(&[1, 2, 3]).is_err(), "half a stereo frame");
    assert!(framer.frame(&vec![0; 2 * 65_536]).is_err());
    let (frame, _) = framer.frame(&[1, 2]).unwrap();
    assert_eq!(frame[4], 0, "the refused frames took no sample number");
}

#[test]
fn g711_and_l16_decode_as_oxrtc_audio_does() {
    let mut out = Vec::new();
    Pcm::Alaw.decode(&[0xd5, 0x55], 1, &mut out).unwrap();
    assert_eq!(out, vec![g711::decode_alaw(0xd5), g711::decode_alaw(0x55)]);
    out.clear();
    Pcm::Ulaw.decode(&[0xff, 0x7f], 2, &mut out).unwrap();
    assert_eq!(out, vec![g711::decode_ulaw(0xff), g711::decode_ulaw(0x7f)]);
    out.clear();
    Pcm::L16.decode(&[0x12, 0x34, 0xff, 0xfe], 2, &mut out).unwrap();
    assert_eq!(out, vec![0x1234, -2]);
    assert!(Pcm::L16.decode(&[1, 2, 3], 1, &mut out).is_err());
    assert!(Pcm::L16.decode(&[1, 2], 2, &mut out).is_err(), "half a stereo frame");
    assert!(Pcm::Alaw.decode(&[], 1, &mut out).is_err());
}

#[test]
fn dfla_is_one_streaminfo_block() {
    let boxed = dfla(1, 8000).unwrap();
    assert_eq!(boxed.len(), 12 + 4 + 34);
    assert_eq!(&boxed[4..12], b"dfLa\0\0\0\0");
    assert_eq!(&boxed[12..16], &[0x80, 0, 0, 34]);
    assert_eq!(&boxed[16..20], &[0, 16, 0xff, 0xff]);
    // 8000 Hz = 0x01f40 on 20 bits, mono, 16 bits.
    assert_eq!(&boxed[26..30], &[0x01, 0xf4, 0x00, 0xf0]);
    assert!(dfla(0, 8000).is_err());
}
