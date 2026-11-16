//! Reader authentication (R28, D24): the operator's credential, the one each
//! publication token carries, Digest and Basic, and what is never asked for.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use common::{Answer, Client, Clip, Factory, config, encode, ffmpeg, hub, run, start};
use md5::{Digest, Md5};
use oxrtc_core::{Codec, ConsumerTag, MediaKind, ProducerSink, Want};
use oxrtc_rtsp_server::{ReadAuth, RtspServerConfig};

const OPERATOR: (&str, &str) = ("operator", "s3cret-pass");

fn with_auth(auth: ReadAuth) -> RtspServerConfig {
    RtspServerConfig { auth: Some(auth), ..config() }
}

fn operator() -> RtspServerConfig {
    with_auth(ReadAuth::new(OPERATOR.0, OPERATOR.1))
}

fn cam_hub() -> Arc<oxrtc_core::Hub> {
    let factory = Arc::new(Factory::default());
    factory.clip("cam", Clip::synthetic());
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    hub
}

fn basic(user: &str, password: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
    format!("Authorization: Basic {encoded}")
}

fn md5(text: &str) -> String {
    Md5::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The values of every header called `name`.
fn headers(answer: &Answer, name: &str) -> Vec<String> {
    answer
        .head
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
        .collect()
}

/// The `nonce="..."` of a Digest challenge.
fn nonce_of(challenge: &str) -> String {
    let rest = challenge.split("nonce=\"").nth(1).unwrap();
    rest.split('"').next().unwrap().to_owned()
}

fn digest(user: &str, password: &str, nonce: &str, method: &str, uri: &str) -> String {
    let a1 = md5(&format!("{user}:oxrtc:{password}"));
    let a2 = md5(&format!("{method}:{uri}"));
    let (nc, cnonce) = ("00000001", "0a4f113b");
    let response = md5(&format!("{a1}:{nonce}:{nc}:{cnonce}:auth:{a2}"));
    format!(
        "Authorization: Digest username=\"{user}\", realm=\"oxrtc\", nonce=\"{nonce}\", uri=\"{uri}\", \
         algorithm=MD5, qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\""
    )
}

/// DESCRIBE with a Digest answer to the challenge the server just gave this connection.
async fn describe_with_digest(c: &mut Client, url: &str, user: &str, password: &str) -> Answer {
    let challenge = c.request("DESCRIBE", url, &[], "").await.unwrap();
    assert_eq!(challenge.status, 401);
    let digest_challenge =
        headers(&challenge, "WWW-Authenticate").into_iter().find(|h| h.starts_with("Digest")).unwrap();
    let line = digest(user, password, &nonce_of(&digest_challenge), "DESCRIBE", url);
    c.request("DESCRIBE", url, &[line.as_str()], "").await.unwrap()
}

#[tokio::test]
async fn a_reader_with_no_credential_is_challenged_with_digest_and_basic_and_keeps_its_connection() {
    let server = start(cam_hub(), operator()).await;
    let url = server.url("cam");
    let mut c = Client::connect(server.addr).await;
    let answer = c.request("DESCRIBE", &url, &[], "").await.unwrap();
    assert_eq!(answer.status, 401);
    let challenges = headers(&answer, "WWW-Authenticate");
    assert_eq!(challenges.len(), 2, "{challenges:?}");
    assert!(challenges[0].starts_with("Digest realm=\"oxrtc\", nonce=\""), "{challenges:?}");
    assert!(challenges[0].contains("qop=\"auth\"") && challenges[0].contains("algorithm=MD5"));
    assert_eq!(challenges[1], "Basic realm=\"oxrtc\"");
    assert!(!challenges[0].contains("stale"));
    // The same connection answers the challenge.
    let line = basic(OPERATOR.0, OPERATOR.1);
    let played = c.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap();
    assert_eq!(played.status, 200, "{played:?}");
    assert!(played.body.contains("m=video"));
}

#[tokio::test]
async fn every_method_asks_for_it_as_go2rtc_does() {
    let server = start(cam_hub(), operator()).await;
    for method in ["OPTIONS", "DESCRIBE", "SETUP", "PLAY", "GET_PARAMETER", "TEARDOWN"] {
        let mut c = Client::connect(server.addr).await;
        let answer = c.request(method, &server.url("cam"), &[], "").await.unwrap();
        assert_eq!(answer.status, 401, "{method}");
    }
    let mut c = Client::connect(server.addr).await;
    let line = basic(OPERATOR.0, OPERATOR.1);
    assert_eq!(c.request("OPTIONS", &server.url("cam"), &[line.as_str()], "").await.unwrap().status, 200);
}

#[tokio::test]
async fn basic_is_checked_for_the_user_and_the_password() {
    let server = start(cam_hub(), operator()).await;
    let url = server.url("cam");
    for (user, password) in [(OPERATOR.0, "wrong"), ("someone", OPERATOR.1), ("", ""), (OPERATOR.0, "")] {
        let mut c = Client::connect(server.addr).await;
        let line = basic(user, password);
        let answer = c.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap();
        assert_eq!(answer.status, 401, "{user}:{password}");
        assert!(c.closed_within(Duration::from_secs(3)).await, "a wrong credential ends the connection");
    }
    // Not base64, no colon, not a scheme we know: all refused, none a panic.
    for line in
        ["Authorization: Basic !!!", "Authorization: Basic b3BlcmF0b3I=", "Authorization: Bearer x", "Authorization:"]
    {
        let mut c = Client::connect(server.addr).await;
        let answer = c.request("DESCRIBE", &url, &[line], "").await.unwrap();
        assert_eq!(answer.status, 401, "{line}");
    }
    // The scheme is not case sensitive; the password may hold a colon.
    let server = start(cam_hub(), with_auth(ReadAuth::new("op", "a:b"))).await;
    let mut c = Client::connect(server.addr).await;
    let encoded = base64::engine::general_purpose::STANDARD.encode("op:a:b");
    let line = format!("Authorization: basic {encoded}");
    assert_eq!(c.request("DESCRIBE", &server.url("cam"), &[line.as_str()], "").await.unwrap().status, 200);
}

#[tokio::test]
async fn digest_plays_with_the_right_password_only() {
    let server = start(cam_hub(), operator()).await;
    let url = server.url("cam");
    let mut c = Client::connect(server.addr).await;
    let played = describe_with_digest(&mut c, &url, OPERATOR.0, OPERATOR.1).await;
    assert_eq!(played.status, 200, "{played:?}");
    // The nonce is the connection's: the next request reuses it (FFmpeg does).
    let challenge_free = c
        .request("OPTIONS", &url, &[digest(OPERATOR.0, OPERATOR.1, "ignored", "OPTIONS", &url).as_str()], "")
        .await
        .unwrap();
    assert_eq!(challenge_free.status, 401, "a nonce the connection never issued is not accepted");

    let mut c = Client::connect(server.addr).await;
    let refused = describe_with_digest(&mut c, &url, OPERATOR.0, "wrong").await;
    assert_eq!(refused.status, 401);
    assert!(c.closed_within(Duration::from_secs(3)).await, "a wrong password ends the connection");

    let mut c = Client::connect(server.addr).await;
    let refused = describe_with_digest(&mut c, &url, "someone", OPERATOR.1).await;
    assert_eq!(refused.status, 401);
}

#[tokio::test]
async fn a_digest_for_the_nonce_of_another_connection_is_answered_stale_and_the_connection_stays() {
    let server = start(cam_hub(), operator()).await;
    let url = server.url("cam");
    let mut first = Client::connect(server.addr).await;
    let challenge = first.request("DESCRIBE", &url, &[], "").await.unwrap();
    let old = nonce_of(&headers(&challenge, "WWW-Authenticate")[0]);
    // A client that reuses the answer of its last connection.
    let mut second = Client::connect(server.addr).await;
    let line = digest(OPERATOR.0, OPERATOR.1, &old, "DESCRIBE", &url);
    let answer = second.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap();
    assert_eq!(answer.status, 401);
    let fresh = headers(&answer, "WWW-Authenticate");
    assert!(fresh[0].contains("stale=true"), "{fresh:?}");
    assert_ne!(nonce_of(&fresh[0]), old);
    let line = digest(OPERATOR.0, OPERATOR.1, &nonce_of(&fresh[0]), "DESCRIBE", &url);
    let played = second.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap();
    assert_eq!(played.status, 200, "{played:?}");
}

#[tokio::test]
async fn a_digest_without_qop_is_accepted_as_rfc_2069_and_a_half_qop_is_not() {
    let server = start(cam_hub(), operator()).await;
    let url = server.url("cam");
    let mut c = Client::connect(server.addr).await;
    let challenge = c.request("DESCRIBE", &url, &[], "").await.unwrap();
    let nonce = nonce_of(&headers(&challenge, "WWW-Authenticate")[0]);
    let a1 = md5(&format!("{}:oxrtc:{}", OPERATOR.0, OPERATOR.1));
    let a2 = md5(&format!("DESCRIBE:{url}"));
    let response = md5(&format!("{a1}:{nonce}:{a2}"));
    let line = format!(
        "Authorization: Digest username=\"{}\", realm=\"oxrtc\", nonce=\"{nonce}\", uri=\"{url}\", response=\"{response}\"",
        OPERATOR.0
    );
    assert_eq!(c.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap().status, 200);

    let mut c = Client::connect(server.addr).await;
    let challenge = c.request("DESCRIBE", &url, &[], "").await.unwrap();
    let nonce = nonce_of(&headers(&challenge, "WWW-Authenticate")[0]);
    let line = format!(
        "Authorization: Digest username=\"{}\", realm=\"oxrtc\", nonce=\"{nonce}\", uri=\"{url}\", qop=auth, response=\"{response}\"",
        OPERATOR.0
    );
    assert_eq!(
        c.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap().status,
        401,
        "qop without nc and cnonce"
    );
}

#[tokio::test]
async fn a_publication_on_a_token_is_not_a_read_and_asks_for_no_credential() {
    let factory = Arc::new(Factory::default());
    let mut sinks = factory.handoff("pub", vec![(MediaKind::Audio, Codec::Pcma)]);
    let hub = hub(factory);
    let stream = hub.upsert("pub", &["handoff:pub"]).unwrap();
    let _sub = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await.unwrap();
    let sink: ProducerSink = tokio::time::timeout(Duration::from_secs(5), sinks.recv()).await.unwrap().unwrap();
    let server = start(hub, operator()).await;
    let handle = server.publishers.register(sink);
    let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=test\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
               m=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=control:streamid=0\r\n";
    let mut c = Client::connect(server.addr).await;
    let answer = c.request("OPTIONS", &handle.url(), &[], "").await.unwrap();
    assert_eq!(answer.status, 200);
    let answer = c.request("ANNOUNCE", &handle.url(), &["Content-Type: application/sdp"], sdp).await.unwrap();
    assert_eq!(answer.status, 200, "{answer:?}");
    // The token opens nothing else.
    let mut c = Client::connect(server.addr).await;
    assert_eq!(c.request("DESCRIBE", &handle.url(), &[], "").await.unwrap().status, 403, "a token reads nothing");
    let mut c = Client::connect(server.addr).await;
    assert_eq!(
        c.request("ANNOUNCE", &server.url("cam"), &["Content-Type: application/sdp"], sdp).await.unwrap().status,
        401
    );
}

#[tokio::test]
async fn each_registration_reads_with_a_credential_of_its_own_until_it_is_dropped() {
    let factory = Arc::new(Factory::default());
    factory.clip("cam", Clip::synthetic());
    let mut sinks = factory.handoff("pub", vec![(MediaKind::Audio, Codec::Pcma)]);
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    let stream = hub.upsert("pub", &["handoff:pub"]).unwrap();
    let _sub = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await.unwrap();
    let sink: ProducerSink = tokio::time::timeout(Duration::from_secs(5), sinks.recv()).await.unwrap().unwrap();
    let server = start(hub, operator()).await;
    let one = server.publishers.register(sink.clone());
    let two = server.publishers.register(sink);
    let (user, password) = one.read_credential().expect("the server asks for credentials");
    let other = two.read_credential().unwrap();
    assert_ne!((user.clone(), password.clone()), other, "one per run");
    assert_ne!(password, OPERATOR.1, "never the operator's");
    assert_ne!(user, OPERATOR.0);
    let token = one.url().rsplit('/').next().unwrap().to_owned();
    assert!(!user.contains(&token[..8]) && !password.contains(&token[..8]), "no part of the publication token");
    assert!(!format!("{one:?}").contains(&password), "Debug prints no credential");

    let url = server.url("cam");
    let mut c = Client::connect(server.addr).await;
    let line = basic(&user, &password);
    assert_eq!(c.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap().status, 200);
    let mut c = Client::connect(server.addr).await;
    assert_eq!(describe_with_digest(&mut c, &url, &user, &password).await.status, 200);
    // The right user with the password of another run.
    let mut c = Client::connect(server.addr).await;
    let line = basic(&user, &other.1);
    assert_eq!(c.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap().status, 401);

    drop(one);
    let mut c = Client::connect(server.addr).await;
    let line = basic(&user, &password);
    assert_eq!(c.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap().status, 401, "its handle is gone");
    let mut c = Client::connect(server.addr).await;
    let line = basic(&other.0, &other.1);
    assert_eq!(
        c.request("DESCRIBE", &url, &[line.as_str()], "").await.unwrap().status,
        200,
        "the other run still reads"
    );
}

#[tokio::test]
async fn a_server_that_asks_for_nothing_issues_no_credential_and_serves_every_reader() {
    let factory = Arc::new(Factory::default());
    factory.clip("cam", Clip::synthetic());
    let mut sinks = factory.handoff("pub", vec![(MediaKind::Audio, Codec::Pcma)]);
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    let stream = hub.upsert("pub", &["handoff:pub"]).unwrap();
    let _sub = stream.subscribe(&Want::all(), ConsumerTag::new("test")).await.unwrap();
    let sink: ProducerSink = tokio::time::timeout(Duration::from_secs(5), sinks.recv()).await.unwrap().unwrap();
    let server = start(hub, config()).await;
    assert!(server.publishers.register(sink).read_credential().is_none());
    let mut c = Client::connect(server.addr).await;
    let answer = c.request("DESCRIBE", &server.url("cam"), &[], "").await.unwrap();
    assert_eq!(answer.status, 200);
    assert!(headers(&answer, "WWW-Authenticate").is_empty());
}

#[tokio::test]
async fn without_local_auth_a_loopback_reader_is_let_in_as_go2rtc_does() {
    let open = with_auth(ReadAuth::new(OPERATOR.0, OPERATOR.1).local(false));
    let server = start(cam_hub(), open).await;
    let mut c = Client::connect(server.addr).await;
    assert_eq!(c.request("DESCRIBE", &server.url("cam"), &[], "").await.unwrap().status, 200);
    // And a wrong credential is not even looked at, as on go2rtc.
    let mut c = Client::connect(server.addr).await;
    let line = basic(OPERATOR.0, "wrong");
    assert_eq!(c.request("DESCRIBE", &server.url("cam"), &[line.as_str()], "").await.unwrap().status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_reads_with_the_operators_credential_and_is_refused_with_another() {
    let Some(ffmpeg) = ffmpeg() else { return };
    let factory = Arc::new(Factory::default());
    factory.clip("cam", Clip::from_annex_b(Codec::H264, &encode(&ffmpeg, Codec::H264, 4).await));
    let hub = hub(factory);
    hub.upsert("cam", &["clip:cam"]).unwrap();
    let server = start(hub, operator()).await;
    let read = |url: String| {
        let ffmpeg = ffmpeg.clone();
        async move {
            run(
                &ffmpeg,
                &[
                    "-hide_banner",
                    "-nostats",
                    "-loglevel",
                    "error",
                    "-rtsp_transport",
                    "tcp",
                    "-i",
                    &url,
                    "-t",
                    "1",
                    "-c",
                    "copy",
                    "-f",
                    "null",
                    "-",
                ],
                Duration::from_secs(30),
            )
            .await
        }
    };
    let (ok, _, err) = read(format!("rtsp://{}:{}@{}/cam?video", OPERATOR.0, OPERATOR.1, server.addr)).await;
    assert!(ok, "FFmpeg with the operator's credential: {err}");
    let (ok, _, err) = read(format!("rtsp://{}:wrong@{}/cam?video", OPERATOR.0, server.addr)).await;
    assert!(!ok, "a wrong password plays");
    assert!(err.contains("401") || err.to_lowercase().contains("unauthorized"), "{err}");
    let (ok, _, err) = read(format!("rtsp://{}/cam?video", server.addr)).await;
    assert!(!ok, "no credential plays");
    assert!(err.contains("401") || err.to_lowercase().contains("unauthorized"), "{err}");
}
