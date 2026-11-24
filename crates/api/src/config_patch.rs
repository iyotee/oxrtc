//! A stream set or removed in the configuration file's text, every other line
//! kept as it is (comments included), as go2rtc's `PUT` and `DELETE
//! /api/streams` write its file. Only a block-style `streams:` map is edited
//! in place; anything else (`streams: {...}`, a file that is not a map) is
//! left to the caller, which writes the parsed tree instead.

use serde_norway::{Mapping, Value};

/// The line ending the file uses.
fn ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Spaces before a line's first other character.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let rest = line.trim_start();
    rest.is_empty() || rest.starts_with('#')
}

/// The key of a `key: ...` line (quotes taken off), when it is one.
fn key_of(line: &str) -> Option<&str> {
    let rest = line.trim();
    let end = rest
        .char_indices()
        .find(|(i, c)| {
            *c == ':' && rest.get(i + 1..).is_some_and(|after| after.is_empty() || after.starts_with([' ', '\t']))
        })?
        .0;
    let key = rest.get(..end)?.trim_end();
    let unquoted = key
        .strip_prefix('"')
        .and_then(|k| k.strip_suffix('"'))
        .or_else(|| key.strip_prefix('\'').and_then(|k| k.strip_suffix('\'')))
        .unwrap_or(key);
    Some(unquoted)
}

/// A YAML double-quoted scalar of `text`.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The lines of one entry: `name:` and its sources as a list, go2rtc's form.
fn entry(name: &str, sources: &[String], at: usize, eol: &str) -> String {
    let pad = " ".repeat(at);
    let mut out = format!("{pad}{name}:{eol}");
    for source in sources {
        out.push_str(&format!("{pad}  - {}{eol}", quoted(source)));
    }
    out
}

/// The parsed file split into what is not `streams:` and the streams map.
fn split(value: Value) -> Option<(Mapping, Mapping)> {
    let mut root = match value {
        Value::Mapping(root) => root,
        Value::Null => Mapping::new(),
        _ => return None,
    };
    let streams = match root.remove("streams") {
        None | Some(Value::Null) => Mapping::new(),
        Some(Value::Mapping(streams)) => streams,
        Some(_) => return None,
    };
    Some((root, streams))
}

/// Whether `out` is `text` with that one stream set or removed and nothing
/// else changed, as both parse: a line that only looks like a key (inside a
/// quoted scalar that runs over several lines) is never edited.
fn verified(text: &str, out: &str, name: &str, sources: Option<&[String]>) -> bool {
    let (Ok(before), Ok(after)) = (serde_norway::from_str::<Value>(text), serde_norway::from_str::<Value>(out)) else {
        return false;
    };
    let (Some((rest, mut expected)), Some((rest_after, streams_after))) = (split(before), split(after)) else {
        return false;
    };
    match sources {
        Some(sources) => {
            expected
                .insert(Value::from(name), Value::Sequence(sources.iter().map(|s| Value::from(s.as_str())).collect()));
        }
        None => {
            expected.remove(name);
        }
    }
    rest == rest_after && expected == streams_after
}

/// `text` with stream `name` set to `sources` (`Some`) or removed (`None`):
/// `None` back when the file's `streams:` is not a block map this can edit,
/// or when the edit would change anything else (checked on the parsed text).
pub(crate) fn patch_stream(text: &str, name: &str, sources: Option<&[String]>) -> Option<String> {
    let out = edit(text, name, sources)?;
    verified(text, &out, name, sources).then_some(out)
}

fn edit(text: &str, name: &str, sources: Option<&[String]>) -> Option<String> {
    let eol = ending(text);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let header = lines.iter().position(|line| !line.starts_with([' ', '\t', '#']) && key_of(line) == Some("streams"));
    let Some(header) = header else {
        // No section: added at the end, as go2rtc adds it.
        let Some(sources) = sources else { return Some(text.to_owned()) };
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(&format!("streams:{eol}"));
        out.push_str(&entry(name, sources, 2, eol));
        return Some(out);
    };
    // `streams:` with nothing after it but a comment: a block map follows.
    let after = lines.get(header)?.trim_end().split_once(':').map_or("", |(_, rest)| rest).trim();
    if !(after.is_empty() || after.starts_with('#')) {
        return None;
    }
    let end = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find(|(_, line)| !is_blank_or_comment(line) && indent(line) == 0)
        .map_or(lines.len(), |(i, _)| i);
    let children = lines.get(header + 1..end)?;
    let at = children.iter().find(|line| !is_blank_or_comment(line)).map_or(2, |line| indent(line));
    if at == 0 || children.iter().any(|line| line.starts_with('\t')) {
        return None;
    }
    let found =
        children.iter().position(|line| !is_blank_or_comment(line) && indent(line) == at && key_of(line) == Some(name));
    let mut out: String = lines.get(..header + 1)?.concat();
    if !out.ends_with('\n') {
        out.push_str(eol);
    }
    match found {
        Some(start) => {
            // The entry: its line and what is indented deeper under it.
            let mut last = start;
            for (i, line) in children.iter().enumerate().skip(start + 1) {
                if is_blank_or_comment(line) && line.trim().is_empty() {
                    continue;
                }
                if indent(line) <= at {
                    break;
                }
                last = i;
            }
            out.push_str(&children.get(..start)?.concat());
            if let Some(sources) = sources {
                out.push_str(&entry(name, sources, at, eol));
            }
            out.push_str(&children.get(last + 1..)?.concat());
        }
        None => {
            let Some(sources) = sources else { return Some(text.to_owned()) };
            // After the last entry's lines, before the blank lines and
            // comments that lead into the next section.
            let tail = children.iter().rposition(|line| !is_blank_or_comment(line)).map_or(0, |i| i + 1);
            out.push_str(&children.get(..tail)?.concat());
            if !out.ends_with('\n') {
                out.push_str(eol);
            }
            out.push_str(&entry(name, sources, at, eol));
            out.push_str(&children.get(tail..)?.concat());
        }
    }
    out.push_str(&lines.get(end..)?.concat());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streams(text: &str) -> Vec<(String, Vec<String>)> {
        crate::config::parse(text).map(|(c, _)| c.streams).unwrap_or_default()
    }

    const FILE: &str = "# top\napi:\n  token: t\nstreams:\n  # the camera\n  cam: rtsp://a/x # inline\n  two:\n    - rtsp://b/y\n    # a note\n    - rtsp://c/z\n\n# about logs\nlog:\n  level: info\n";

    #[test]
    fn an_entry_is_replaced_in_place_and_every_comment_kept() {
        let out = patch_stream(FILE, "cam", Some(&["rtsp://d/w#timeout=5".to_owned()])).unwrap_or_default();
        assert!(
            out.contains("# top\n")
                && out.contains("  # the camera\n")
                && out.contains("    # a note\n")
                && out.contains("# about logs\n"),
            "{out}"
        );
        assert!(out.contains("  cam:\n    - \"rtsp://d/w#timeout=5\"\n  two:\n"), "{out}");
        let names: Vec<_> = streams(&out).into_iter().map(|(n, s)| (n, s.len())).collect();
        assert_eq!(names, [("cam".to_owned(), 1), ("two".to_owned(), 2)]);
        assert_eq!(streams(&out)[0].1, ["rtsp://d/w#timeout=5"]);
    }

    #[test]
    fn an_entry_is_added_after_the_last_and_removed_with_its_lines() {
        let added = patch_stream(FILE, "new", Some(&["rtsp://e/v".to_owned(), "ffmpeg:new#audio=opus".to_owned()]))
            .unwrap_or_default();
        assert!(
            added.contains(
                "    - rtsp://c/z\n  new:\n    - \"rtsp://e/v\"\n    - \"ffmpeg:new#audio=opus\"\n\n# about logs\n"
            ),
            "{added}"
        );
        assert_eq!(streams(&added).len(), 3);
        let removed = patch_stream(&added, "two", None).unwrap_or_default();
        assert!(
            !removed.contains("rtsp://b/y") && !removed.contains("# a note") && removed.contains("  # the camera\n"),
            "{removed}"
        );
        let names: Vec<_> = streams(&removed).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["cam", "new"]);
        assert_eq!(patch_stream(&removed, "absent", None).as_deref(), Some(removed.as_str()));
    }

    #[test]
    fn a_file_without_streams_gets_the_section_and_crlf_is_kept() {
        let out = patch_stream("api:\r\n  token: t", "cam", Some(&["rtsp://a/x".to_owned()])).unwrap_or_default();
        assert_eq!(out, "api:\r\n  token: t\r\nstreams:\r\n  cam:\r\n    - \"rtsp://a/x\"\r\n");
        assert_eq!(streams(&out)[0].1, ["rtsp://a/x"]);
    }

    #[test]
    fn a_crlf_file_is_edited_in_place() {
        let text = FILE.replace('\n', "\r\n");
        let out = patch_stream(&text, "cam", None).unwrap_or_default();
        assert_eq!(out.matches("streams:").count(), 1, "{out}");
        assert!(!out.contains("rtsp://a/x") && out.contains("# a note\r\n"), "{out}");
        assert_eq!(streams(&out).len(), 1);
    }

    /// The fuzz target's crash of 2026-11-24 (CI's run 45380915394): a quoted
    /// value running over several lines holds `streams:` at the start of a
    /// line, which the line editor took for the section, changing `api.listen`.
    #[test]
    fn a_line_inside_a_quoted_value_is_never_taken_for_the_section() {
        let text = "api:\n  listen: \"127.0.0.1:1api:\r\n  to84# top\napi:\n  token: t\nstreams:\n  # the camera\n  cam: rtsp://a/x # inline\n  two:\n    - rtsr://b/y\n    # a \"\n  username: \"u\"\n  password: \"p\"\n  local_auth: true\nstreams:\nen: tnote\n  \r  ";
        let set = ["rtsp://u:p@a/x#timeout=5".to_owned()];
        if let Some(out) = patch_stream(text, "cam", Some(&set)) {
            let listen = |t: &str| crate::config::parse(t).map(|(c, _)| c.api.listen).ok();
            assert_eq!(listen(&out), listen(text), "{out:?}");
        }
    }

    #[test]
    fn what_is_not_a_block_map_is_left_to_the_caller() {
        assert_eq!(patch_stream("streams: {cam: rtsp://a/x}\n", "cam", None), None);
    }

    #[test]
    fn a_source_with_quotes_and_backslashes_is_written_as_it_reads() {
        let source = "rtsp://u:p\"a\\b@c/x".to_owned();
        let out = patch_stream(FILE, "cam", Some(std::slice::from_ref(&source))).unwrap_or_default();
        assert_eq!(streams(&out)[0].1, [source]);
    }
}
