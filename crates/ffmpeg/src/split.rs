//! Splitting an argument template into an argument vector, without a shell.
//!
//! The rules are go2rtc's `shell.QuoteSplit`, so that a template written for
//! go2rtc splits the same way here: arguments are separated by spaces, tabs,
//! carriage returns and newlines; an argument that *starts* with `"` or `'`
//! runs to the next identical quote, which is dropped, and may hold spaces
//! (`-vf "scale=1280:720"`, `-i "{input}"`). A quote inside an argument is an
//! ordinary character. Nothing else is special: no `$`, no backslash, no glob.

use thiserror::Error;

/// Most arguments one template may produce.
pub const MAX_ARGS: usize = 512;

/// A template that cannot be split.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SplitError {
    /// A quote that is never closed, or a pair of quotes with nothing inside
    /// (go2rtc refuses both).
    #[error("unterminated or empty quoted argument")]
    Quote,
    /// More than [`MAX_ARGS`] arguments.
    #[error("more than {MAX_ARGS} arguments")]
    TooMany,
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// Splits `text` into arguments.
pub fn quote_split(text: &str) -> Result<Vec<String>, SplitError> {
    let mut args = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches(is_space);
        let Some(first) = rest.chars().next() else { break };
        if args.len() >= MAX_ARGS {
            return Err(SplitError::TooMany);
        }
        if first == '"' || first == '\'' {
            let inner = rest.get(1..).unwrap_or_default();
            let close = inner.find(first).filter(|&at| at > 0).ok_or(SplitError::Quote)?;
            args.push(inner.get(..close).unwrap_or_default().to_owned());
            rest = inner.get(close + 1..).unwrap_or_default();
        } else {
            let end = rest.find(is_space).unwrap_or(rest.len());
            args.push(rest.get(..end).unwrap_or_default().to_owned());
            rest = rest.get(end..).unwrap_or_default();
        }
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(quote_split(" -c:v  libx264\t-g 50\r\n").unwrap(), ["-c:v", "libx264", "-g", "50"]);
        assert!(quote_split("").unwrap().is_empty());
        assert!(quote_split("  \n ").unwrap().is_empty());
    }

    #[test]
    fn quotes_group_and_are_dropped() {
        assert_eq!(
            quote_split(r#"-vf "scale=1280:720,transpose=1" -f rtsp"#).unwrap(),
            ["-vf", "scale=1280:720,transpose=1", "-f", "rtsp"]
        );
        assert_eq!(quote_split("-i 'rtsp://cam/a b c' x").unwrap(), ["-i", "rtsp://cam/a b c", "x"]);
        // A quote closes the argument; the next one starts right after it.
        assert_eq!(quote_split(r#""a b"c"#).unwrap(), ["a b", "c"]);
        // The other quote character is ordinary inside quotes.
        assert_eq!(quote_split(r#""it's""#).unwrap(), ["it's"]);
        // A quote inside an argument is ordinary.
        assert_eq!(quote_split(r#"text='%{localtime}'"#).unwrap(), ["text='%{localtime}'"]);
    }

    #[test]
    fn a_url_with_spaces_is_one_argument_when_quoted() {
        let args = quote_split(r#"-format_code Hp29 -f decklink -i "DeckLink SDI (2)""#).unwrap();
        assert_eq!(args, ["-format_code", "Hp29", "-f", "decklink", "-i", "DeckLink SDI (2)"]);
    }

    #[test]
    fn refuses_unterminated_and_empty_quotes() {
        assert_eq!(quote_split(r#"-i "rtsp://cam"#), Err(SplitError::Quote));
        assert_eq!(quote_split(r#"-i """#), Err(SplitError::Quote));
        assert_eq!(quote_split("'"), Err(SplitError::Quote));
    }

    #[test]
    fn is_bounded() {
        assert_eq!(quote_split(&"a ".repeat(MAX_ARGS)).unwrap().len(), MAX_ARGS);
        assert_eq!(quote_split(&"a ".repeat(MAX_ARGS + 1)), Err(SplitError::TooMany));
    }

    proptest::proptest! {
        #[test]
        fn never_panics(text in "[ a-z\"'\t\n\u{e9}]{0,64}") {
            let _ = quote_split(&text);
        }

        #[test]
        fn plain_words_round_trip(words in proptest::collection::vec("[a-z0-9:=/._-]{1,12}", 0..20)) {
            proptest::prop_assert_eq!(quote_split(&words.join(" ")).unwrap(), words);
        }
    }
}
