use super::*;

fn query(text: &str) -> Query {
    Query::of(&format!("/api/frame.jpeg?{text}").parse::<Uri>().unwrap()).unwrap()
}

#[test]
fn durations_read_as_go_reads_them() {
    let secs = Duration::from_secs_f64;
    for (text, want) in [
        ("10s", secs(10.0)),
        ("1m", secs(60.0)),
        ("1.5s", secs(1.5)),
        (".5s", secs(0.5)),
        ("500ms", secs(0.5)),
        ("1h30m", secs(5400.0)),
        ("2m3.5s", secs(123.5)),
        ("250us", secs(0.000_25)),
        ("250µs", secs(0.000_25)),
        ("0", Duration::ZERO),
        ("+5s", secs(5.0)),
        ("-5s", Duration::ZERO),
        ("1000h", MAX_CACHE_AGE),
    ] {
        assert_eq!(go_duration(text), Some(want), "{text}");
    }
    for text in ["", "10", "s", "1x", "1.2.3s", "1 s", "--1s", "1sm", ".s", &"9".repeat(100)] {
        assert_eq!(go_duration(text), None, "{text:?}");
    }
}

#[test]
fn the_query_reads_go2rtc_s_names() {
    let shape = |text: &str| -> Result<Shape, Vec<Ignored>> {
        let (asked, ignored) = asked(&query(text));
        if ignored.is_empty() { Ok(asked.shape) } else { Err(ignored) }
    };
    assert_eq!(shape("src=x"), Ok(Shape::default()));
    assert_eq!(shape("src=x&w=320").unwrap().width, Some(320));
    assert_eq!(shape("src=x&width=320&w=640").unwrap().width, Some(320), "the long name first");
    assert_eq!(shape("src=x&h=240").unwrap().height, Some(240));
    // Go's Atoi takes a plus sign: go2rtc scales `width=+160` to 160.
    assert_eq!(shape("src=x&width=%2B160").unwrap().width, Some(160));
    assert_eq!(shape("src=x&h=%2B120").unwrap().height, Some(120));
    for (value, rotate) in [
        ("0", Rotate::None),
        ("90", Rotate::Quarter),
        ("180", Rotate::Half),
        ("270", Rotate::ThreeQuarters),
        ("-90", Rotate::ThreeQuarters),
    ] {
        assert_eq!(shape(&format!("src=x&rotate={value}")).unwrap().rotate, rotate, "{value}");
    }
    assert_eq!(asked(&query("src=x&cache=10s")).0.cache, Some(Duration::from_secs(10)));
    assert_eq!(asked(&query("src=x&cache=")), (Asked::default(), vec![]), "an empty value is none, as go2rtc's");
}

/// go2rtc 1.9.15, measured on the binary (D2, `d2-work/stills.py`): what it
/// cannot read is as if not given, and nothing is refused. Each case here is
/// the plain picture (or the shape of what could be read) and one value set
/// aside, named.
#[test]
fn what_cannot_be_read_is_answered_as_go2rtc_answers_it() {
    let plain = Asked::default();
    for (bad, key) in [
        ("rotate=45", "rotate"),
        ("rotate=x", "rotate"),
        ("width=0", "width"),
        ("w=abc", "w"),
        ("width=-1", "width"),
        ("width=1.5", "width"),
        ("h=-1", "h"),
        ("h=0", "h"),
        ("cache=abc", "cache"),
        ("cache=10", "cache"),
        ("hardware=vaapi", "hardware"),
        ("hw=x", "hw"),
    ] {
        let (got, ignored) = asked(&query(&format!("src=x&{bad}")));
        assert_eq!(got, plain, "{bad}: the plain picture, as go2rtc");
        assert_eq!(ignored.len(), 1, "{bad}");
        assert_eq!(ignored[0].key, key, "{bad}");
        assert_eq!(ignored[0].value, bad.split_once('=').unwrap().1, "{bad}");
    }
    // A side that cannot be read is not given; the other one keeps its
    // meaning, the picture its shape (go2rtc hands FFmpeg 0, the source's own
    // side, and distorts it: width=160&height=abc was 160x240 of a 320x240).
    let (got, ignored) = asked(&query("src=x&width=160&height=abc"));
    assert_eq!(got.shape, Shape { width: Some(160), ..Shape::default() });
    assert_eq!(ignored.len(), 1);
    // A whole number past the largest side is held to it (go2rtc: no bound;
    // height=7681 was a 10241x7681 picture, width=99999 a 500).
    for (big, side) in [("height=7681", (None, Some(7680))), ("width=99999", (Some(7680), None))] {
        let (got, ignored) = asked(&query(&format!("src=x&{big}")));
        assert_eq!((got.shape.width, got.shape.height), side, "{big}");
        assert!(ignored[0].instead.starts_with("read as 7680"), "{big}: {ignored:?}");
    }
    let (got, ignored) = asked(&query("src=x&w=18446744073709551616000"));
    assert_eq!((got.shape.width, ignored.len()), (Some(7680), 1), "longer than any integer");
    // Several at once: each named, what can be read keeps its meaning.
    let (got, ignored) = asked(&query("src=x&rotate=45&width=0&cache=abc&h=120"));
    assert_eq!(got, Asked { shape: Shape { height: Some(120), ..Shape::default() }, cache: None });
    let keys: Vec<&str> = ignored.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, ["rotate", "width", "cache"]);
    // cache=-1s is a duration: never served, as go2rtc; nothing set aside.
    assert_eq!(asked(&query("src=x&cache=-1s")), (Asked { cache: Some(Duration::ZERO), ..plain }, vec![]));
}

#[tokio::test(start_paused = true)]
async fn a_cached_picture_is_served_while_younger_than_asked_and_per_shape() {
    let stills = Stills::default();
    let small = Shape { width: Some(320), ..Shape::default() };
    stills.put("cam", Shape::default(), Bytes::from_static(b"full"));
    stills.put("cam", small, Bytes::from_static(b"small"));
    let age = Duration::from_secs(10);
    assert_eq!(stills.get("cam", Shape::default(), age), Some(Bytes::from_static(b"full")));
    assert_eq!(stills.get("cam", small, age), Some(Bytes::from_static(b"small")));
    assert_eq!(stills.get("other", small, age), None);
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(stills.get("cam", small, age), None, "as old as asked: made again");
    assert_eq!(stills.get("cam", small, Duration::from_secs(11)), Some(Bytes::from_static(b"small")));
    assert_eq!(stills.get("cam", small, Duration::ZERO), None);
    stills.put("cam", small, Bytes::from_static(b"newer"));
    assert_eq!(stills.get("cam", small, age), Some(Bytes::from_static(b"newer")), "in place of the older");
}

#[test]
fn the_cache_is_bounded_in_pictures_and_bytes() {
    let stills = Stills::default();
    for i in 0..MAX_CACHED + 10 {
        stills.put(&format!("cam{i}"), Shape::default(), Bytes::from_static(b"x"));
    }
    let held = |s: &Stills| s.entries.lock().unwrap().len();
    assert_eq!(held(&stills), MAX_CACHED);
    assert_eq!(stills.get("cam0", Shape::default(), MAX_CACHE_AGE), None, "the oldest went first");
    let big = Bytes::from(vec![0u8; MAX_CACHED_BYTES / 2 + 1]);
    stills.put("a", Shape::default(), big.clone());
    stills.put("b", Shape::default(), big.clone());
    let bytes: usize = stills.entries.lock().unwrap().iter().map(|e| e.jpeg.len()).sum();
    assert!(bytes <= MAX_CACHED_BYTES, "{bytes}");
    assert_eq!(stills.get("b", Shape::default(), MAX_CACHE_AGE), Some(big));
    stills.put("huge", Shape::default(), Bytes::from(vec![0u8; MAX_CACHED_BYTES + 1]));
    assert_eq!(stills.get("huge", Shape::default(), MAX_CACHE_AGE), None, "never kept");
}

proptest::proptest! {
    /// Any query a client writes (D2 review): never refused nor a panic; a
    /// side given is 1 to the largest, and only when its value reads as a
    /// count; every value set aside is one the query holds, under its name.
    #[test]
    fn any_query_gives_a_picture_and_names_what_it_set_aside(
        pairs in proptest::collection::vec(
            (
                proptest::prop_oneof![
                    proptest::strategy::Just("width"), proptest::strategy::Just("w"),
                    proptest::strategy::Just("height"), proptest::strategy::Just("h"),
                    proptest::strategy::Just("rotate"), proptest::strategy::Just("cache"),
                    proptest::strategy::Just("hardware"), proptest::strategy::Just("src"),
                ],
                "(-|\\+)?[0-9]{0,22}(\\.[0-9]{0,2})?(s|ms|m|h|x)?|\\PC{0,20}",
            ),
            0..8,
        )
    ) {
        // Encoded as a client encodes them: a `+` sent as `%2B`.
        let text = form_urlencoded::Serializer::new(String::new()).extend_pairs(&pairs).finish();
        let query = query(&text);
        let (got, ignored) = asked(&query);
        for side in [got.shape.width, got.shape.height].into_iter().flatten() {
            proptest::prop_assert!((1..=MAX_SNAPSHOT_WIDTH).contains(&side), "{side}");
        }
        for set_aside in &ignored {
            proptest::prop_assert_eq!(query.first(&set_aside.key), Some(set_aside.value.as_str()));
        }
        if let Some(width) = got.shape.width {
            let value = query.first("width").or_else(|| query.first("w")).unwrap_or_default();
            let read = crate::unread::whole(value).unwrap_or(0);
            proptest::prop_assert_eq!(u64::from(width), read.min(u64::from(MAX_SNAPSHOT_WIDTH)));
        }
    }
}
