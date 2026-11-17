//! A minimal HTTP/1.1 client: one request on its own connection.
#![allow(dead_code, missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub auth: Option<(&'a str, &'a str)>,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
    /// More request headers, as (name, value).
    pub headers: Vec<(&'a str, &'a str)>,
}

impl<'a> Request<'a> {
    pub fn new(method: &'a str, target: &'a str) -> Self {
        Self { method, target, auth: None, content_type: None, body: &[], headers: Vec::new() }
    }

    pub fn header(mut self, name: &'a str, value: &'a str) -> Self {
        self.headers.push((name, value));
        self
    }

    pub fn auth(mut self, user: &'a str, pass: &'a str) -> Self {
        self.auth = Some((user, pass));
        self
    }

    pub fn body(mut self, content_type: &'a str, body: &'a [u8]) -> Self {
        self.content_type = Some(content_type);
        self.body = body;
        self
    }

    /// One request on its own connection (`Connection: close`).
    pub async fn send(&self, addr: std::net::SocketAddr) -> Reply {
        tokio::time::timeout(Duration::from_secs(70), self.exchange(addr))
            .await
            .unwrap_or_else(|_| panic!("{} {} timed out", self.method, self.target))
    }

    async fn exchange(&self, addr: std::net::SocketAddr) -> Reply {
        let mut socket = TcpStream::connect(addr).await.unwrap();
        let mut head = format!("{} {} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n", self.method, self.target);
        if let Some((user, pass)) = self.auth {
            let credential = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            head.push_str(&format!("Authorization: Basic {credential}\r\n"));
        }
        if let Some(content_type) = self.content_type {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(self.body).await.unwrap();
        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await.unwrap();
        parse_reply(&raw)
    }
}

fn parse_reply(raw: &[u8]) -> Reply {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("no end of headers");
    let head = std::str::from_utf8(&raw[..split]).unwrap();
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned())).collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    if chunked {
        body = dechunk(&body);
    }
    Reply { status, headers, body }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let end = data.windows(2).position(|w| w == b"\r\n").expect("chunk size line");
        let size = usize::from_str_radix(std::str::from_utf8(&data[..end]).unwrap().trim(), 16).unwrap();
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[end + 2..end + 2 + size]);
        data = &data[end + 4 + size..];
    }
}
