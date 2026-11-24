//! `GET /api/openapi.yaml`: this server's own OpenAPI description, held to
//! the router both ways (M5 F7, D48).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use serde_norway::Value;

async fn description(api: &Api) -> Value {
    let reply = api.call(Method::GET, "/api/openapi.yaml").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type(), "application/yaml");
    let text = reply.text();
    assert!(!text.contains("{{"), "every placeholder filled: {text}");
    serde_norway::from_str(&text).unwrap()
}

/// The HLS session's routes, outside `ROUTES` (read with the session's id).
const HLS: [&str; 7] = [
    "/api/hls/playlist.m3u8",
    "/api/hls/init.mp4",
    "/api/hls/segment.m4s",
    "/api/hls/segment.ts",
    "/api/hls/part.m4s",
    "/api/hls/part.ts",
    "/api/hls/manifest.mpd",
];

#[tokio::test]
async fn the_description_is_openapi_3_1_with_this_servers_version() {
    let api = Api::new();
    let doc = description(&api).await;
    assert_eq!(doc["openapi"], Value::from("3.1.0"));
    assert_eq!(doc["info"]["version"], Value::from("1.2.3-test"));
    assert_eq!(doc["info"]["title"], Value::from("oxrtc"));
}

#[tokio::test]
async fn every_route_of_the_router_is_described() {
    let api = Api::new();
    let doc = description(&api).await;
    let paths = &doc["paths"];
    let mut routes: Vec<(String, String)> = ROUTES
        .iter()
        .filter(|(_, uri)| uri.starts_with("/api"))
        .map(|(method, uri)| {
            let path = uri.split('?').next().unwrap();
            let path =
                if path.starts_with("/api/webrtc/") && path != "/api/webrtc/ice" { "/api/webrtc/{id}" } else { path };
            (method.to_ascii_lowercase(), path.to_owned())
        })
        .collect();
    routes.extend(HLS.iter().map(|path| ("get".to_owned(), (*path).to_owned())));
    routes.push(("get".into(), "/api/openapi.yaml".into()));
    for (method, path) in routes {
        assert!(paths[path.as_str()][method.as_str()].is_mapping(), "{method} {path} is not described");
    }
}

#[tokio::test]
async fn every_described_route_is_answered() {
    let api = Api::new();
    let doc = description(&api).await;
    let Value::Mapping(paths) = &doc["paths"] else { panic!("no paths") };
    let mut seen = 0;
    for (path, operations) in paths {
        let path = path.as_str().unwrap().replace("{id}", "abc");
        let Value::Mapping(operations) = operations else { panic!("{path}") };
        for method in operations.keys().filter_map(Value::as_str).filter(|m| *m != "parameters") {
            let method = Method::from_bytes(method.to_ascii_uppercase().as_bytes()).unwrap();
            let reply = api.call(method.clone(), &path).await;
            assert_ne!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
            assert!(
                !(reply.status == StatusCode::NOT_FOUND && reply.text() == "not found"),
                "{method} {path}: no such route"
            );
            seen += 1;
        }
    }
    assert!(seen >= 37, "{seen} operations");
}
