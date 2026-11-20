use super::*;

fn alaw_8k() -> Input {
    Input { codec: InputCodec::Alaw, rate: 8000, channels: 1 }
}

/// Feeds `frames` frames of `size` samples dated from `start`, and returns
/// every packet.
fn feed(t: &mut Transcoder, start: i64, frames: usize, size: usize, code: u8) -> Vec<Packet> {
    let mut out = Vec::new();
    for i in 0..frames {
        t.push(&vec![code; size], start + (i * size) as i64, &mut out).unwrap();
    }
    out
}

#[test]
fn g711_at_its_own_rate_passes_unchanged_in_20_ms_packets() {
    let mut t = Transcoder::new(alaw_8k(), Output { codec: OutputCodec::Alaw, rate: 8000 }).unwrap();
    // The owner's cameras send 64 ms frames: 512 samples.
    let packets = feed(&mut t, 1000, 5, 512, 0x9A);
    assert_eq!(packets.len(), 5 * 512 / 160);
    assert!(packets.iter().all(|p| p.data.len() == 160 && p.data.iter().all(|&b| b == 0x9A)));
    let dates: Vec<i64> = packets.iter().map(|p| p.pts).collect();
    assert_eq!(dates.first(), Some(&1000));
    assert!(dates.windows(2).all(|w| w[1] - w[0] == 160), "{dates:?}");
    assert_eq!(t.held().0, 5 * 512 % 160);
}

#[test]
fn the_dates_keep_the_inputs_clock_in_the_outputs() {
    #[cfg(feature = "opus")]
    {
        let mut t = Transcoder::new(alaw_8k(), Output { codec: OutputCodec::Opus, rate: 48_000 }).unwrap();
        assert_eq!(t.encode_rate(), 8000, "libopus takes 8 kHz itself: no resampling");
        let packets = feed(&mut t, 8000, 10, 512, 0xD5);
        assert_eq!(packets.len(), 10 * 512 / 160);
        assert_eq!(packets[0].pts, 48_000, "one second of 8 kHz is one second of 48 kHz");
        assert_eq!(packets[1].pts - packets[0].pts, 960);
        assert_eq!(packets[3].source_pts, 8000 + 3 * 160);
    }
    let mut t = Transcoder::new(
        Input { codec: InputCodec::L16, rate: 16_000, channels: 2 },
        Output { codec: OutputCodec::Ulaw, rate: 8000 },
    )
    .unwrap();
    let mut out = Vec::new();
    // 40 ms of stereo L16 at 16 kHz: 640 samples a channel, 2560 bytes.
    for i in 0..10 {
        t.push(&[0x10; 2560], 32_000 + i * 640, &mut out).unwrap();
    }
    assert!(out.len() >= 18 && out.len() <= 20, "{}", out.len());
    assert_eq!(out[0].pts, 16_000);
    assert_eq!(out[0].data.len(), 160);
    assert_eq!(out[2].source_pts, 32_000 + 2 * 320);
}

#[test]
fn a_gap_starts_again_from_the_new_frames_date() {
    let mut t = Transcoder::new(alaw_8k(), Output { codec: OutputCodec::Alaw, rate: 8000 }).unwrap();
    let mut out = Vec::new();
    t.push(&[1; 320], 0, &mut out).unwrap();
    // 40 ms late: within the jitter, taken as continuous.
    t.push(&[2; 320], 360, &mut out).unwrap();
    assert_eq!(out.iter().map(|p| p.pts).collect::<Vec<_>>(), [0, 160, 320, 480]);
    // Two seconds later: a new run from that date, the half packet dropped.
    t.push(&[3; 170], 16_000, &mut out).unwrap();
    assert_eq!(out.last().map(|p| (p.pts, p.data[0])), Some((16_000, 3)));
    assert_eq!(t.held().0, 10);
}

#[test]
fn g722_is_dated_at_its_8000_clock_from_16_khz() {
    let mut t = Transcoder::new(alaw_8k(), Output { codec: OutputCodec::G722, rate: 16_000 }).unwrap();
    assert_eq!(t.output().clock_rate(), 8000);
    let packets = feed(&mut t, 800, 4, 400, 0xD5);
    assert!(packets.len() >= 9, "{}", packets.len());
    assert!(packets.iter().all(|p| p.data.len() == 160));
    assert_eq!(packets[0].pts, 800);
    assert_eq!(packets[1].pts - packets[0].pts, 160);
}

#[test]
fn what_cannot_be_made_is_refused() {
    let out = Output { codec: OutputCodec::Alaw, rate: 8000 };
    assert!(matches!(Transcoder::new(Input { channels: 0, ..alaw_8k() }, out), Err(TranscodeError::Channels(0))));
    assert!(matches!(Transcoder::new(Input { rate: 8001, ..alaw_8k() }, out), Err(TranscodeError::Rate(_))));
    #[cfg(not(feature = "opus"))]
    assert!(matches!(
        Transcoder::new(alaw_8k(), Output { codec: OutputCodec::Opus, rate: 48_000 }),
        Err(TranscodeError::Opus(_))
    ));
}
