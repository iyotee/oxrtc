//! `GET /api/frame.jpeg` (F5): go2rtc's `width`/`w`, `height`/`h` and
//! `rotate` handed to the snapshotter as one shape, what it cannot read
//! refused, go2rtc's headers, and `cache`: a picture made for a request that
//! asked for one is served again while younger than the cache asked, per
//! stream and shape.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode, header};
use bytes::Bytes;
use common::*;
use oxrtc_core::Codec;
use oxrtc_core::jpeg::{Rotate, Shape};

fn api() -> (Api, std::sync::Arc<FakeSnapshotter>) {
    let snapshotter = FakeSnapshotter::new(Ok(Bytes::from_static(b"\xff\xd8jpeg\xff\xd9")));
    let api = Api::build(|s| s.with_snapshots(snapshotter.clone()));
    api.factory.add("cam1", Script::video(Codec::H264, 0));
    (api, snapshotter)
}

#[tokio::test]
async fn the_shape_is_read_as_go2rtc_reads_it_and_answered_with_its_headers() {
    let (api, snapshotter) = api();
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    for uri in [
        "/api/frame.jpeg?src=cam&w=320",
        "/api/frame.jpeg?src=cam&height=240",
        "/api/frame.jpeg?src=cam&width=320&h=200&rotate=90",
        "/api/frame.jpeg?src=cam&rotate=-90",
        "/api/frame.jpeg?src=cam&rotate=180&hardware=vaapi",
    ] {
        let reply = api.call(Method::GET, uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(reply.content_type(), "image/jpeg");
        assert_eq!(reply.headers.get(header::CACHE_CONTROL).unwrap(), "no-cache");
        assert_eq!(reply.headers.get(header::PRAGMA).unwrap(), "no-cache");
        assert_eq!(reply.headers.get(header::CONTENT_LENGTH).unwrap(), "8");
    }
    let shapes = snapshotter.shapes.lock().unwrap().clone();
    assert_eq!(
        shapes,
        vec![
            Shape { width: Some(320), ..Shape::default() },
            Shape { height: Some(240), ..Shape::default() },
            Shape { width: Some(320), height: Some(200), rotate: Rotate::Quarter },
            Shape { rotate: Rotate::ThreeQuarters, ..Shape::default() },
            Shape { rotate: Rotate::Half, ..Shape::default() },
        ]
    );
    for bad in ["h=0", "height=7681", "rotate=45", "cache=ten", "cache=10"] {
        let reply = api.call(Method::GET, &format!("/api/frame.jpeg?src=cam&{bad}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    assert_eq!(snapshotter.shapes.lock().unwrap().len(), 5, "refused before any work");
}

#[tokio::test(start_paused = true)]
async fn a_cached_picture_is_served_while_younger_than_the_cache_asks() {
    let (api, snapshotter) = api();
    api.call(Method::PUT, "/api/streams?name=cam&src=fake:cam1").await;
    let made = || snapshotter.shapes.lock().unwrap().len();
    // Without `cache`, a picture each time, and none kept.
    api.call(Method::GET, "/api/frame.jpeg?src=cam").await;
    api.call(Method::GET, "/api/frame.jpeg?src=cam&cache=1m").await;
    assert_eq!(made(), 2, "nothing was kept for a request without cache");
    let reply = api.call(Method::GET, "/api/frame.jpeg?src=cam&cache=10s").await;
    assert_eq!((reply.status, made()), (StatusCode::OK, 2), "kept by the one that asked with a cache");
    assert_eq!(&reply.body[..], b"\xff\xd8jpeg\xff\xd9");
    api.call(Method::GET, "/api/frame.jpeg?src=cam&w=320&cache=10s").await;
    assert_eq!(made(), 3, "another shape is made, not served the other one");
    api.call(Method::GET, "/api/frame.jpeg?src=cam").await;
    assert_eq!(made(), 4, "a request without cache is always made");
    tokio::time::advance(Duration::from_secs(5)).await;
    api.call(Method::GET, "/api/frame.jpeg?src=cam&cache=10s").await;
    assert_eq!(made(), 4);
    api.call(Method::GET, "/api/frame.jpeg?src=cam&cache=4s").await;
    assert_eq!(made(), 5, "older than this request's cache");
    api.call(Method::GET, "/api/frame.jpeg?src=cam&cache=-1s").await;
    assert_eq!(made(), 6, "a negative cache is never served, as go2rtc's");
}
