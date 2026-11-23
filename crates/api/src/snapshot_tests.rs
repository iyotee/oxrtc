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
    let shape = |text: &str| asked(&query(text)).map(|a| a.shape);
    assert_eq!(shape("src=x"), Ok(Shape::default()));
    assert_eq!(shape("src=x&w=320").unwrap().width, Some(320));
    assert_eq!(shape("src=x&width=320&w=640").unwrap().width, Some(320), "the long name first");
    assert_eq!(shape("src=x&h=240").unwrap().height, Some(240));
    for (value, rotate) in [
        ("0", Rotate::None),
        ("90", Rotate::Quarter),
        ("180", Rotate::Half),
        ("270", Rotate::ThreeQuarters),
        ("-90", Rotate::ThreeQuarters),
    ] {
        assert_eq!(shape(&format!("src=x&rotate={value}")).unwrap().rotate, rotate, "{value}");
    }
    for bad in ["width=0", "w=abc", "height=7681", "h=-1", "rotate=45", "rotate=x", "cache=10"] {
        assert!(asked(&query(&format!("src=x&{bad}"))).is_err(), "{bad}");
    }
    assert_eq!(asked(&query("src=x&cache=10s")).unwrap().cache, Some(Duration::from_secs(10)));
    assert_eq!(asked(&query("src=x&cache=")).unwrap().cache, None, "an empty value is none, as go2rtc's");
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
