//! A WebSocket client of `/api/ws` over real TCP, and a server to point it at.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use oxrtc_api::{ApiState, ServeLimits};
use oxrtc_core::CancellationToken;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

/// The API served on `127.0.0.1:0`.
pub struct Served {
    pub addr: SocketAddr,
    pub cancel: CancellationToken,
    pub task: JoinHandle<std::io::Result<()>>,
}

pub async fn serve(state: ApiState, limits: ServeLimits) -> Served {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(oxrtc_api::serve_with_limits(listener, oxrtc_api::router(state), cancel.clone(), limits));
    Served { addr, cancel, task }
}

/// A connected socket, and the local address the server saw it from.
pub struct Client {
    pub ws: WebSocketStream<TcpStream>,
    pub local: SocketAddr,
}

/// What a handshake can be given.
#[derive(Default, Clone)]
pub struct Handshake<'a> {
    pub authorization: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

/// Opens `/api/ws` + `query` (`?src=cam`); `Err` with the status of a refused
/// handshake.
pub async fn open(addr: SocketAddr, query: &str, handshake: Handshake<'_>) -> Result<Client, u16> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let local = stream.local_addr().unwrap();
    let mut request = format!("ws://{addr}/api/ws{query}").into_client_request().unwrap();
    let headers = request.headers_mut();
    for (name, value) in
        [("authorization", handshake.authorization), ("origin", handshake.origin), ("user-agent", handshake.user_agent)]
    {
        if let Some(value) = value {
            headers.insert(name, HeaderValue::from_str(value).unwrap());
        }
    }
    match tokio_tungstenite::client_async(request, stream).await {
        Ok((ws, _)) => Ok(Client { ws, local }),
        Err(WsError::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("handshake: {other}"),
    }
}

/// The status and body of a handshake that must be refused.
pub async fn refusal(addr: SocketAddr, query: &str, handshake: Handshake<'_>) -> (u16, String) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!("ws://{addr}/api/ws{query}").into_client_request().unwrap();
    for (name, value) in [("authorization", handshake.authorization), ("origin", handshake.origin)] {
        if let Some(value) = value {
            request.headers_mut().insert(name, HeaderValue::from_str(value).unwrap());
        }
    }
    match tokio_tungstenite::client_async(request, stream).await {
        Err(WsError::Http(response)) => {
            let body = response.body().clone().unwrap_or_default();
            (response.status().as_u16(), String::from_utf8(body).unwrap())
        }
        other => panic!("not refused: {:?}", other.map(|_| ())),
    }
}

/// [`open`] with the test credential, which must succeed.
pub async fn open_ok(addr: SocketAddr, query: &str) -> Client {
    let auth = super::basic(super::USER, super::PASS);
    open(addr, query, Handshake { authorization: Some(&auth), ..Handshake::default() }).await.unwrap()
}

/// What the server sent next.
#[derive(Debug, PartialEq)]
pub enum Next {
    Json(serde_json::Value),
    /// A close frame with its code and reason.
    Closed(u16, String),
    /// The socket ended without a close frame.
    Ended,
    /// Nothing within the wait.
    Nothing,
}

impl Client {
    pub async fn send(&mut self, value: serde_json::Value) {
        self.ws.send(Message::Text(value.to_string().into())).await.unwrap();
    }

    pub async fn send_text(&mut self, text: &str) {
        self.ws.send(Message::Text(text.to_owned().into())).await.unwrap();
    }

    /// Sends `value` and does not stop at a failure (the server may have
    /// closed already).
    pub async fn try_send_text(&mut self, text: String) -> bool {
        self.ws.send(Message::Text(text.into())).await.is_ok()
    }

    /// The next text message or the end, pings and pongs skipped.
    pub async fn next(&mut self, wait: Duration) -> Next {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let Ok(item) = tokio::time::timeout_at(deadline, self.ws.next()).await else { return Next::Nothing };
            match item {
                Some(Ok(Message::Text(text))) => return Next::Json(serde_json::from_str(text.as_str()).unwrap()),
                Some(Ok(Message::Close(frame))) => {
                    return frame.map_or(Next::Closed(1005, String::new()), |f| {
                        Next::Closed(u16::from(f.code), f.reason.as_str().to_owned())
                    });
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(other)) => panic!("unexpected {other:?}"),
                Some(Err(_)) | None => return Next::Ended,
            }
        }
    }

    /// The next message, which must be JSON.
    pub async fn json(&mut self) -> serde_json::Value {
        match self.next(Duration::from_secs(5)).await {
            Next::Json(value) => value,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// The close the server sends, skipping messages before it.
    pub async fn closed(&mut self) -> (u16, String) {
        loop {
            match self.next(Duration::from_secs(5)).await {
                Next::Json(_) => {}
                Next::Closed(code, reason) => return (code, reason),
                other => panic!("expected a close, got {other:?}"),
            }
        }
    }
}

/// What the server sent next, binary frames included.
#[derive(Debug, PartialEq)]
pub enum Item {
    Json(serde_json::Value),
    Binary(Vec<u8>),
    /// The socket closed or ended, or nothing came within the wait.
    Over(Next),
}

impl Client {
    /// The next text or binary message, pings and pongs skipped.
    pub async fn item(&mut self, wait: Duration) -> Item {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let Ok(item) = tokio::time::timeout_at(deadline, self.ws.next()).await else {
                return Item::Over(Next::Nothing);
            };
            match item {
                Some(Ok(Message::Text(text))) => return Item::Json(serde_json::from_str(text.as_str()).unwrap()),
                Some(Ok(Message::Binary(data))) => return Item::Binary(data.to_vec()),
                Some(Ok(Message::Close(frame))) => {
                    return Item::Over(frame.map_or(Next::Closed(1005, String::new()), |f| {
                        Next::Closed(u16::from(f.code), f.reason.as_str().to_owned())
                    }));
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(other)) => panic!("unexpected {other:?}"),
                Some(Err(_)) | None => return Item::Over(Next::Ended),
            }
        }
    }

    /// The next binary message, which must come within 5 s.
    pub async fn binary(&mut self) -> Vec<u8> {
        match self.item(Duration::from_secs(5)).await {
            Item::Binary(data) => data,
            other => panic!("expected a binary message, got {other:?}"),
        }
    }
}

/// An error message's value.
pub fn error_of(value: &serde_json::Value) -> &str {
    assert_eq!(value["type"], "error", "{value}");
    value["value"].as_str().unwrap()
}
