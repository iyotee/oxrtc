//! `PUT /api/webrtc/ice` on the running server: the call reaches the WebRTC
//! egress, which keeps the servers it can use and says how many.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use common::Request;
use oxrtc::App;
use oxrtc_api::config::parse;
use serde_json::json;

const ANY: &str = "127.0.0.1:0";

fn text() -> String {
    format!(
        "api:\n  listen: \"{ANY}\"\n  username: \"u\"\n  password: \"p\"\n\
         rtsp:\n  listen: \"{ANY}\"\nwebrtc:\n  listen: \"{ANY}\"\n"
    )
}

#[tokio::test]
async fn the_running_server_takes_new_ice_servers_and_counts_the_ones_it_can_use() {
    let app = App::start(&parse(&text()).unwrap().0, None).await.unwrap();
    let api = app.addresses().api;
    let body = json!([
        {
            "urls": ["stun:127.0.0.1:9", "turn:127.0.0.1:9?transport=udp", "turn:127.0.0.1:9?transport=tcp", "turns:relay.example:5349"],
            "username": "1700003600:site",
            "credential": "c2VjcmV0",
        },
        // A TURN server with no credential is not used.
        {"urls": ["turn:127.0.0.2:9"]},
    ])
    .to_string();
    let put = |body: &str| {
        let body = body.to_owned();
        async move {
            Request::new("PUT", "/api/webrtc/ice")
                .auth("u", "p")
                .body("application/json", body.as_bytes())
                .send(api)
                .await
        }
    };
    let reply = put(&body).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(
        reply.json(),
        json!({"turn": 2, "stun": 1}),
        "the TLS URL and the entry without a credential are left out"
    );
    // Again, the same: nothing changes, and an empty list removes them all.
    assert_eq!(put(&body).await.status, 200);
    let reply = put("[]").await;
    assert_eq!(reply.json(), json!({"turn": 0, "stun": 0}));
    assert_eq!(put("{}").await.status, 400);
    let denied = Request::new("PUT", "/api/webrtc/ice").body("application/json", b"[]").send(api).await;
    assert_eq!(denied.status, 401);
    assert!(app.shutdown().await);
}
