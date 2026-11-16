//! Who may publish: a peer on loopback, seen as the same peer however its
//! address is spelled (R28 review).

use std::net::SocketAddr;

use oxrtc_core::{Drivers, Hub, Limits, Refusals};
use rtsp_types::headers::CSEQ;
use rtsp_types::{Method, Request, StatusCode, Version};

use super::*;
use crate::publish::Publishers;
use crate::{RtspServerConfig, Shared};

fn shared() -> Shared {
    Shared {
        hub: Hub::new(Drivers::new(), Limits::default()),
        publishers: Publishers::new(SocketAddr::from(([127, 0, 0, 1], 8554)), false),
        config: RtspServerConfig::default(),
        auth: None,
        bad_credentials: Refusals::new(),
    }
}

/// The status of an `ANNOUNCE` on a token nobody registered, with a body that is no SDP: a
/// peer let past the loopback check is told `400`, one that is not is told `403`.
fn announce_from(peer: &str) -> StatusCode {
    let request = Request::builder(Method::Announce, Version::V1_0)
        .request_uri(rtsp_types::Url::parse("rtsp://127.0.0.1:8554/_publish/0123").unwrap())
        .header(CSEQ, "1")
        .build(b"not sdp".to_vec());
    match Recorder::announce(&shared(), peer.parse::<SocketAddr>().unwrap(), &request) {
        Err(response) => response.status(),
        Ok(_) => StatusCode::Ok,
    }
}

#[tokio::test]
async fn a_peer_on_loopback_may_publish_whatever_the_spelling_of_its_address() {
    for peer in ["127.0.0.1:1", "[::1]:1", "[::ffff:127.0.0.1]:1"] {
        assert_eq!(announce_from(peer), StatusCode::BadRequest, "{peer}");
    }
}

#[tokio::test]
async fn a_peer_off_loopback_may_not() {
    for peer in ["192.168.1.9:1", "[2001:db8::1]:1", "[::ffff:192.168.1.9]:1"] {
        assert_eq!(announce_from(peer), StatusCode::Forbidden, "{peer}");
    }
}
