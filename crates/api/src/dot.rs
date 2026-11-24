//! `GET /api/streams.dot`: the streams as a Graphviz graph, in the shape of
//! go2rtc's (its `net.html` draws it with vis-network).
//!
//! Per stream: a node per source that has connected (`producer`), linked
//! from its host and to a node per track (`node`), and a node per consumer
//! (`consumer`), linked from the stream's producers and to its host. Every
//! identifier is a number of this answer, every label quoted with its quotes
//! and backslashes escaped; the source URLs are the redacted ones.

use std::fmt::Write;

use oxrtc_core::{SourceState, StreamSnapshot};

/// Longest label or title written, in characters.
const MAX_TEXT: usize = 512;

/// `"text"`, a DOT string: control characters out, quotes as go2rtc's (`'`),
/// backslashes doubled, cut at [`MAX_TEXT`].
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars().filter(|c| !c.is_control() || *c == '\n').take(MAX_TEXT) {
        match c {
            '"' => out.push('\''),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// go2rtc's sizes: `512 B`, `56.94 KB`, `1.20 MB` (powers of 1000).
pub(crate) fn human_bytes(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < 4 {
        value /= 1000.0;
        unit += 1;
    }
    let units = ["KB", "MB", "GB", "TB", "PB"];
    format!("{value:.2} {}", units.get(unit).copied().unwrap_or("PB"))
}

/// The host of a source's URL (`rtsp://***@192.168.1.10:554/x` is
/// `192.168.1.10`), or of a consumer's address; `unknown` without one.
fn host_of(address: &str) -> String {
    let rest = address.split_once("://").map_or(address, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(inner) => inner.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    if host.is_empty() { "unknown".to_owned() } else { host.to_owned() }
}

/// The graph of `streams`, `digraph { ... }`.
pub(crate) fn graph(streams: &[StreamSnapshot]) -> String {
    let mut out = String::from("digraph {\n");
    let mut next = 0u64;
    let mut id = || {
        next += 1;
        next
    };
    for stream in streams {
        let mut producers = Vec::new();
        for source in stream.sources.iter().filter(|s| s.state == SourceState::Running || !s.tracks.is_empty()) {
            let producer = id();
            producers.push(producer);
            let host = quoted(&host_of(&source.url));
            let scheme = source.url.split_once(':').map_or("source", |(scheme, _)| scheme);
            let title = format!("stream={}\nurl={}\nstate={}", stream.name, source.url, source.state.as_str());
            let _ = writeln!(out, "{host} [group=host];");
            let _ = writeln!(out, "{producer} [group=producer, label={}, title={}];", quoted(scheme), quoted(&title));
            let _ = writeln!(out, "{host} -> {producer} [label={}];", quoted(&human_bytes(source.bytes)));
            for (index, track) in source.tracks.iter().enumerate() {
                let node = id();
                let mut title = format!("codec_name={}\nclock_rate={}", track.codec.as_str(), track.clock_rate);
                if track.channels != 0 {
                    let _ = write!(title, "\nchannels={}", track.channels);
                }
                let frames = source.track_frames.get(index).copied().unwrap_or(0);
                let _ = writeln!(out, "{producer} -> {node} [label={}];", quoted(&format!("{frames} frames")));
                let _ = writeln!(
                    out,
                    "{node} [group=node, label={}, title={}];",
                    quoted(track.codec.as_str()),
                    quoted(&title)
                );
            }
        }
        for consumer in &stream.consumers {
            let node = id();
            let host = quoted(&consumer.remote.as_deref().map_or_else(|| "127.0.0.1".to_owned(), host_of));
            let mut title = format!("format_name={}\nqueue={}", consumer.kind, consumer.class.as_str());
            if let Some(agent) = &consumer.user_agent {
                let _ = write!(title, "\nuser_agent={agent}");
            }
            let _ = writeln!(out, "{host} [group=host];");
            let _ =
                writeln!(out, "{node} [group=consumer, label={}, title={}];", quoted(&consumer.kind), quoted(&title));
            let _ = writeln!(out, "{node} -> {host} [label={}];", quoted(&human_bytes(consumer.bytes)));
            for producer in &producers {
                let _ =
                    writeln!(out, "{producer} -> {node} [label={}];", quoted(&format!("{} frames", consumer.frames)));
            }
        }
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_as_go2rtc_writes_them() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(56_936), "56.94 KB");
        assert_eq!(human_bytes(1_200_000), "1.20 MB");
        assert_eq!(human_bytes(u64::MAX), "18446.74 PB");
    }

    #[test]
    fn hosts_of_urls_and_addresses() {
        assert_eq!(host_of("rtsp://***@192.168.1.10:554/live"), "192.168.1.10");
        assert_eq!(host_of("127.0.0.1:63857"), "127.0.0.1");
        assert_eq!(host_of("[::1]:5000"), "::1");
        assert_eq!(host_of("rtsp://[fe80::1]:554/x"), "fe80::1");
        assert_eq!(host_of("stream:cam"), "stream");
        assert_eq!(host_of(""), "unknown");
    }

    #[test]
    fn a_label_cannot_break_out_of_its_string() {
        assert_eq!(quoted("a\"b\\c\nd\re"), "\"a'b\\\\c\\nde\"");
        assert_eq!(quoted(&"x".repeat(2000)).len(), MAX_TEXT + 2);
    }
}
