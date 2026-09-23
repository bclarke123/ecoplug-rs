//! Minimal HTTP/1.1 client for the daemon's API, using only the standard library.
//!
//! The CLI never talks to the plug itself (except `discover`); if the daemon
//! is not running every command fails with a clear error.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::api::{DstRequest, ErrorBody, OverrideRequest, SetRequest, Status};
use crate::proto::{PlugClock, ScheduleTable};

/// Connect and read timeout for API calls.
const TIMEOUT: Duration = Duration::from_secs(5);

/// API client bound to one daemon.
#[derive(Debug, Clone)]
pub struct Client {
    addr: SocketAddr,
    token: Option<String>,
}

impl Client {
    /// Creates a client for the daemon at `addr`.
    pub fn new(addr: SocketAddr, token: Option<String>) -> Self {
        Self { addr, token }
    }

    fn request<T: DeserializeOwned>(&self, method: &str, path: &str, body: Option<&impl Serialize>) -> Result<T> {
        let mut stream = TcpStream::connect_timeout(&self.addr, TIMEOUT)
            .with_context(|| format!("connect to daemon at {} (is ecopumpd running?)", self.addr))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        let payload = match body {
            Some(b) => serde_json::to_vec(b)?,
            None => Vec::new(),
        };
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            self.addr,
            payload.len()
        );
        if let Some(t) = &self.token {
            head.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(&payload)?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).context("read daemon response")?;
        let (code, resp_body) = parse_response(&raw)?;
        if (200..300).contains(&code) {
            serde_json::from_slice(resp_body).context("decode daemon response")
        } else {
            let msg = serde_json::from_slice::<ErrorBody>(resp_body)
                .map(|e| e.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(resp_body).into_owned());
            bail!("daemon returned {code}: {msg}")
        }
    }

    /// `GET /status`.
    pub fn status(&self) -> Result<Status> {
        self.request("GET", "/status", None::<&()>)
    }

    /// `POST /set`: hold `power` until the next scheduled transition.
    pub fn set(&self, req: SetRequest) -> Result<Status> {
        self.request("POST", "/set", Some(&req))
    }

    /// `POST /override`.
    pub fn set_override(&self, req: OverrideRequest) -> Result<Status> {
        self.request("POST", "/override", Some(&req))
    }

    /// `DELETE /override`.
    pub fn clear_override(&self) -> Result<Status> {
        self.request("DELETE", "/override", None::<&()>)
    }

    /// `GET /clock`.
    pub fn clock(&self) -> Result<PlugClock> {
        self.request("GET", "/clock", None::<&()>)
    }

    /// `POST /clock/dst`.
    pub fn set_dst(&self, on: bool) -> Result<PlugClock> {
        self.request("POST", "/clock/dst", Some(&DstRequest { on }))
    }

    /// `GET /schedule`.
    pub fn schedule(&self) -> Result<ScheduleTable> {
        self.request("GET", "/schedule", None::<&()>)
    }

    /// `POST /schedule/sync`.
    pub fn sync_schedule(&self) -> Result<ScheduleTable> {
        self.request("POST", "/schedule/sync", None::<&()>)
    }

    /// `DELETE /schedule`.
    pub fn clear_schedule(&self) -> Result<ScheduleTable> {
        self.request("DELETE", "/schedule", None::<&()>)
    }
}

/// Splits a raw HTTP response into status code and body.
fn parse_response(raw: &[u8]) -> Result<(u16, &[u8])> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response"))?;
    let head = std::str::from_utf8(&raw[..split]).context("non-UTF-8 response header")?;
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line: {head:?}"))?;
    Ok((code, &raw[split + 4..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_response() {
        let (code, body) = parse_response(b"HTTP/1.1 404 Not Found\r\nX: y\r\n\r\n{\"error\":\"nope\"}").unwrap();
        assert_eq!(code, 404);
        assert_eq!(body, br#"{"error":"nope"}"#);
        assert!(parse_response(b"garbage").is_err());
    }
}
