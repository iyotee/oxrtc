//! What a session's window reads as: go2rtc's master playlist, a media
//! playlist (RFC 8216, with the Low-Latency tags of its second edition:
//! parts, preload hints, blocking reload), and an MPEG-DASH manifest over
//! the same fMP4 segments (ISO/IEC 23009-1, a dynamic MPD with a segment
//! timeline). No I/O: text from the window.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::window::{Segment, Window};

/// What a session's segments are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Fragmented MP4: `init.mp4`, `segment.m4s`, `part.m4s`.
    Fmp4,
    /// MPEG-TS: `segment.ts`, `part.ts`.
    Ts,
}

impl Kind {
    fn extension(self) -> &'static str {
        match self {
            Self::Fmp4 => "m4s",
            Self::Ts => "ts",
        }
    }
}

/// A time as RFC 3339 in UTC, to the millisecond: `2026-11-24T10:20:30.400Z`.
pub(crate) fn rfc3339(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let days = i64::try_from(since.as_secs() / 86_400).unwrap_or(0);
    let secs = since.as_secs() % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        since.subsec_millis()
    )
}

fn seconds(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64())
}

/// The codec strings as an HLS `CODECS` attribute: FLAC as `fLaC`, as
/// go2rtc writes it (what Safari and hls.js look for).
pub(crate) fn codecs_attribute(codecs: &[String]) -> String {
    codecs.iter().map(|c| if c == "flac" { "fLaC" } else { c.as_str() }).collect::<Vec<_>>().join(",")
}

/// go2rtc's master playlist: one variant, the media playlist at
/// `hls/playlist.m3u8?id=` relative to `/api/` (go2rtc's player replaces
/// `hls/` with the server's URL), its `BANDWIDTH` (Safari wants one) and
/// `CODECS`.
pub(crate) fn master(id: &str, codecs: &[String]) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-STREAM-INF:BANDWIDTH=192000,CODECS=\"{}\"\nhls/playlist.m3u8?id={id}\n",
        codecs_attribute(codecs)
    )
}

/// What a media playlist is written from.
pub(crate) struct View<'a> {
    pub id: &'a str,
    pub kind: Kind,
    pub window: &'a Window,
    /// Parts, preload hints and blocking reload (Low-Latency HLS).
    pub low_latency: bool,
}

/// Segments whose parts a Low-Latency playlist lists, besides the open one:
/// the newest complete ones (RFC 8216bis keeps parts within three target
/// durations of the end; these are within two segments).
const PARTS_OF_LAST: usize = 2;

impl View<'_> {
    fn head(&self, out: &mut String, segment: &Segment, previous_init: &mut Option<u64>) {
        if segment.discontinuity {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        if self.kind == Kind::Fmp4 && *previous_init != Some(segment.init) {
            let _ = writeln!(out, "#EXT-X-MAP:URI=\"init.mp4?id={}&i={}\"", self.id, segment.init);
        }
        *previous_init = Some(segment.init);
        let _ = writeln!(out, "#EXT-X-PROGRAM-DATE-TIME:{}", rfc3339(segment.wall));
    }

    fn parts(&self, out: &mut String, segment: &Segment) {
        let ext = self.kind.extension();
        for (index, part) in segment.parts.iter().enumerate() {
            let independent = if part.independent { ",INDEPENDENT=YES" } else { "" };
            let _ = writeln!(
                out,
                "#EXT-X-PART:DURATION={},URI=\"part.{ext}?id={}&n={}&p={index}\"{independent}",
                seconds(part.duration),
                self.id,
                segment.msn
            );
        }
    }

    /// The media playlist.
    pub(crate) fn media(&self) -> String {
        let window = self.window;
        let target = window.longest().as_secs_f64().ceil().max(1.0);
        let version = if self.kind == Kind::Fmp4 { 9 } else { 3 };
        let mut out = format!("#EXTM3U\n#EXT-X-VERSION:{version}\n#EXT-X-TARGETDURATION:{target:.0}\n");
        if self.low_latency {
            // Fixed for the session (`Window::part_target`): a stalled source
            // no longer drags `PART-HOLD-BACK` (and hls.js's latency) along
            // with its longest part (F6P).
            let part = window.part_target();
            let _ = writeln!(
                out,
                "#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK={},HOLD-BACK={}",
                // RFC 8216bis' floor (twice the part target): hls.js holds
                // back what it is told, and three parts were 0.7 s behind the
                // camera where go2rtc's fMP4 is 0.5 s (F6's measurement).
                seconds(part * 2),
                seconds(Duration::from_secs_f64(target * 3.0))
            );
            let _ = writeln!(out, "#EXT-X-PART-INF:PART-TARGET={}", seconds(part));
        }
        let first = window.first().unwrap_or(0);
        let _ = writeln!(out, "#EXT-X-MEDIA-SEQUENCE:{first}");
        if window.discontinuities() > 0 {
            let _ = writeln!(out, "#EXT-X-DISCONTINUITY-SEQUENCE:{}", window.discontinuities());
        }
        // Every segment starts at a keyframe, unless one was closed at its
        // bound with none.
        if window.segments().chain(window.open()).all(|s| s.independent) {
            out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
        }
        let done: Vec<&Segment> = window.segments().collect();
        let with_parts = done.len().saturating_sub(PARTS_OF_LAST);
        let mut init = None;
        let ext = self.kind.extension();
        for (index, segment) in done.iter().enumerate() {
            self.head(&mut out, segment, &mut init);
            if self.low_latency && index >= with_parts {
                self.parts(&mut out, segment);
            }
            let _ =
                writeln!(out, "#EXTINF:{},\nsegment.{ext}?id={}&n={}", seconds(segment.duration), self.id, segment.msn);
        }
        if self.low_latency
            && let Some(open) = window.open()
        {
            self.head(&mut out, open, &mut init);
            self.parts(&mut out, open);
            if let Some((msn, part)) = window.next_part() {
                let _ =
                    writeln!(out, "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part.{ext}?id={}&n={msn}&p={part}\"", self.id);
            }
        }
        out
    }
}

/// An ISO 8601 duration in seconds: `PT1.200S`.
fn iso_duration(d: Duration) -> String {
    format!("PT{}S", seconds(d))
}

/// Where a manifest is read from: `/api/stream.mpd` (its segments under
/// `hls/`) or the session's `/api/hls/manifest.mpd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MpdAt {
    Api,
    Session,
}

/// The MPEG-DASH manifest of an fMP4 session: one period from the
/// session's zero (`availabilityStartTime` its wall clock), an adaptation
/// set per track ([`adaptation_sets`]), its segments on a
/// timeline in milliseconds addressed by their start (`$Time$`), the init
/// segment of the latest track list (the segments of an earlier one are left
/// out).
///
/// By their start, not their number (D47, F6P): FFmpeg 8.0.1's DASH reader
/// reads a live timeline's `$Number$` as an index into the timeline it holds
/// (`dashdec.c`, `refresh_manifest` and `get_current_fragment`), so once
/// the window had slid it asked again for segments it had read - each manifest
/// reload sent it back up to the window's end, "non monotonically increasing
/// dts" by the hundred - where it maps `$Time$` from one reload to the next.
/// No `startNumber`: FFmpeg adds it to its first index.
pub(crate) fn mpd(
    id: &str,
    window: &Window,
    codecs: &[String],
    video: Option<usize>,
    zero: SystemTime,
    now: SystemTime,
    at: MpdAt,
) -> String {
    let (base, location) = match at {
        MpdAt::Api => ("  <BaseURL>hls/</BaseURL>\n", format!("hls/manifest.mpd?id={id}")),
        MpdAt::Session => ("", format!("manifest.mpd?id={id}")),
    };
    let init = window.init();
    let segments: Vec<&Segment> = window.segments().filter(|s| s.init == init).collect();
    let longest = window.longest();
    let depth: Duration = segments.iter().map(|s| s.duration).sum();
    let mut timeline = String::new();
    for s in &segments {
        let _ = writeln!(timeline, "          <S t=\"{}\" d=\"{}\"/>", s.start.as_millis(), s.duration.as_millis());
    }
    let mut sets = String::new();
    for (n, set) in adaptation_sets(codecs, video).iter().enumerate() {
        let _ = write!(
            sets,
            r#"    <AdaptationSet id="{n}" contentType="{content}" mimeType="{mime}" segmentAlignment="true" startWithSAP="1">
      <Representation id="{n}" codecs="{codecs}" bandwidth="192000">
        <SegmentTemplate timescale="1000" initialization="init.mp4?id={id}&amp;i={init}{track}" media="segment.m4s?id={id}&amp;t=$Time${track}">
          <SegmentTimeline>
{timeline}          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
"#,
            content = set.content,
            mime = set.mime,
            codecs = set.codecs,
            track = set.track,
        );
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-live:2011" type="dynamic" availabilityStartTime="{ast}" publishTime="{now}" minimumUpdatePeriod="{update}" minBufferTime="{buffer}" timeShiftBufferDepth="{depth}" suggestedPresentationDelay="{delay}" maxSegmentDuration="{longest}">
{base}  <Location>{location}</Location>
  <Period id="0" start="PT0S">
{sets}  </Period>
</MPD>
"#,
        ast = rfc3339(zero),
        now = rfc3339(now),
        update = iso_duration(window.settings().segment),
        buffer = iso_duration(longest),
        depth = iso_duration(depth.max(longest)),
        delay = iso_duration(dash_delay(longest)),
        longest = iso_duration(longest),
    )
}

/// How far behind the live edge a DASH player is told to play
/// (`suggestedPresentationDelay`, which dash.js 5.3.0 takes as its live
/// delay): two of the longest segments and [`DASH_MARGIN`]. A segment is
/// listed once complete, so the newest media a player can have is up to a
/// segment old, and it needs the next one before it has played the one it
/// holds; at one and a half segments (until F6P) dash.js reached the end of
/// its buffer about once a segment, stopping and dropping a picture each time
/// (9 to 24 in 17 s, `f6p-work/abba.py`).
pub(crate) fn dash_delay(longest: Duration) -> Duration {
    longest * 2 + DASH_MARGIN
}

/// What a DASH player holds beyond one segment: a segment's fetch and the
/// camera's own delay (`dash_delay`, and the media a new session's first
/// manifest waits for).
pub(crate) const DASH_MARGIN: Duration = Duration::from_millis(500);

/// One adaptation set of a manifest.
struct AdaptationSet {
    content: &'static str,
    mime: &'static str,
    codecs: String,
    /// What its init segment's and segments' URLs add: `&amp;track=`.
    track: &'static str,
}

fn is_video(codec: &str) -> bool {
    codec.starts_with("avc") || codec.starts_with("hvc") || codec.starts_with("hev")
}

/// A picture and a sound in two adaptation sets, each read alone (its own
/// init segment and segments, `track=video` and `track=audio`): one
/// representation of both tracks is "intentionally not supported" by
/// dash.js 5.3.0, and FFmpeg 8.0.1's DASH reader gives its sound as the
/// picture's stream, on the picture's time base (F6P). Anything else, one set.
fn adaptation_sets(codecs: &[String], video: Option<usize>) -> Vec<AdaptationSet> {
    let sound = |v: usize| codecs.iter().enumerate().find(|(i, _)| *i != v).map(|(_, c)| c);
    let split = video.filter(|_| codecs.len() == 2).and_then(|v| Some((codecs.get(v)?, sound(v)?)));
    if let Some((picture, sound)) = split {
        return vec![
            AdaptationSet { content: "video", mime: "video/mp4", codecs: picture.clone(), track: "&amp;track=video" },
            AdaptationSet { content: "audio", mime: "audio/mp4", codecs: sound.clone(), track: "&amp;track=audio" },
        ];
    }
    let picture = codecs.iter().any(|c| is_video(c));
    let (content, mime) = if picture { ("video", "video/mp4") } else { ("audio", "audio/mp4") };
    vec![AdaptationSet { content, mime, codecs: codecs.join(","), track: "" }]
}

#[cfg(test)]
#[path = "playlist_tests.rs"]
mod tests;
