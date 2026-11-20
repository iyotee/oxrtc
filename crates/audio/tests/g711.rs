//! G.711 against FFmpeg's `pcm_alaw` and `pcm_mulaw`: every code decoded,
//! every 16-bit sample encoded.

mod common;

use oxrtc_audio::g711;

#[test]
fn every_code_decodes_as_ffmpeg_decodes_it() {
    let Some(bin) = common::ffmpeg() else { return };
    let codes: Vec<u8> = (0..=255u8).collect();
    for (format, decode) in [("alaw", g711::decode_alaw as fn(u8) -> i16), ("mulaw", g711::decode_ulaw)] {
        let reference =
            common::s16le(&common::run(&bin, &["-f", format, "-ar", "8000", "-ac", "1"], &["-f", "s16le"], &codes));
        let ours: Vec<i16> = codes.iter().map(|&c| decode(c)).collect();
        assert_eq!(ours, reference, "{format}");
    }
}

#[test]
fn every_sample_encodes_as_ffmpeg_encodes_it() {
    let Some(bin) = common::ffmpeg() else { return };
    let samples: Vec<i16> = (i16::MIN..=i16::MAX).collect();
    let input = common::to_s16le(&samples);
    for (format, codec, encode, decode) in [
        ("alaw", "pcm_alaw", g711::encode_alaw as fn(i16) -> u8, g711::decode_alaw as fn(u8) -> i16),
        ("mulaw", "pcm_mulaw", g711::encode_ulaw, g711::decode_ulaw),
    ] {
        let reference =
            common::run(&bin, &["-f", "s16le", "-ar", "8000", "-ac", "1"], &["-c:a", codec, "-f", format], &input);
        let ours: Vec<u8> = samples.iter().map(|&s| encode(s)).collect();
        assert_eq!(ours.len(), reference.len(), "{format}");
        // G.711 puts a sample in the interval of its decision levels (Tables
        // 1 and 2), and FFmpeg's table picks the nearest reconstruction value
        // instead: the same, except across a segment's edge, where the step
        // doubles and the midpoint of two reconstruction values is not the
        // decision level, and on the levels themselves (G.191's reference puts
        // a negative sample on one in the interval below). There FFmpeg's code
        // is the neighbour of ours, and nearer the sample; anywhere else the
        // codes are equal.
        let mut differ = 0;
        for ((&sample, &mine), &theirs) in samples.iter().zip(&ours).zip(&reference) {
            if mine == theirs {
                continue;
            }
            differ += 1;
            let error = |code: u8| (i32::from(decode(code)) - i32::from(sample)).abs();
            let neighbours = (i32::from(decode(mine)) - i32::from(decode(theirs))).abs() <= 1024;
            assert!(
                neighbours && error(theirs) <= error(mine) + 8,
                "{format}: {sample} coded {mine:#04x} ({}) here, {theirs:#04x} ({}) by FFmpeg",
                decode(mine),
                decode(theirs)
            );
        }
        eprintln!("{format}: {differ} of 65536 samples coded to the nearer value by FFmpeg");
        assert!(differ < 4000, "{format}: {differ}");
    }
}
