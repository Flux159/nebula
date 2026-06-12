//! Minimal HTTP/1.1 client for nebulad's loopback REST API — two endpoints,
//! no TLS, no redirects, so a std TcpStream beats pulling in a client stack.

use anyhow::{bail, Context};
use serde::Deserialize;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct GuestMem {
    pub total_kib: u64,
    pub available_kib: u64,
    pub psi_some_avg10: Option<f64>,
    pub psi_full_avg10: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub balloon_target_mib: u64,
    pub max_mib: u64,
    pub host_footprint_mib: u64,
    pub guest: Option<GuestMem>,
}

impl Stats {
    /// MiB the balloon is holding back from the guest.
    pub fn held_mib(&self) -> u64 {
        self.max_mib.saturating_sub(self.balloon_target_mib)
    }
    pub fn guest_used_mib(&self) -> u64 {
        self.guest
            .as_ref()
            .map(|g| g.total_kib.saturating_sub(g.available_kib) / 1024)
            .unwrap_or(0)
    }
    pub fn guest_avail_mib(&self) -> u64 {
        self.guest
            .as_ref()
            .map(|g| g.available_kib / 1024)
            .unwrap_or(0)
    }
    pub fn psi_some(&self) -> f64 {
        self.guest
            .as_ref()
            .and_then(|g| g.psi_some_avg10)
            .unwrap_or(0.0)
    }
}

pub fn get_stats(port: u16) -> anyhow::Result<Stats> {
    let body = http_get(port, "/v1alpha1/stats")?;
    serde_json::from_str(&body).context("parse /v1alpha1/stats")
}

pub fn get_status(port: u16) -> anyhow::Result<serde_json::Value> {
    let body = http_get(port, "/v1alpha1/status")?;
    serde_json::from_str(&body).context("parse /v1alpha1/status")
}

pub fn http_get(port: u16, path: &str) -> anyhow::Result<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("connect 127.0.0.1:{port}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    parse_response(&raw, path)
}

fn parse_response(raw: &[u8], path: &str) -> anyhow::Result<String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("malformed HTTP response (no header terminator)")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .context("malformed status line")?;
    if status != 200 {
        bail!("GET {path}: HTTP {status}");
    }
    let chunked = head
        .lines()
        .any(|l| l.to_ascii_lowercase().trim() == "transfer-encoding: chunked");
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn dechunk(mut b: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let nl = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("chunked: missing size line")?;
        let size_line = std::str::from_utf8(&b[..nl]).context("chunked: bad size line")?;
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or(""), 16)
            .context("chunked: bad size")?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            bail!("chunked: truncated chunk");
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        assert_eq!(parse_response(raw, "/x").unwrap(), "hi");
    }

    #[test]
    fn parses_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nfoo\r\n2\r\nba\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw, "/x").unwrap(), "fooba");
    }

    #[test]
    fn non_200_is_error() {
        let raw = b"HTTP/1.1 404 Not Found\r\n\r\nnope";
        assert!(parse_response(raw, "/x").is_err());
    }

    #[test]
    fn stats_parse_live_shape() {
        let j = r#"{"balloonTargetMib":3113,"guest":{"available_kib":1151892,"cached_kib":659696,"free_kib":674524,"psi_full_avg10":0.0,"psi_some_avg10":0.0,"total_kib":32808732},"hostFootprintMib":2659,"maxMib":32768}"#;
        let s: Stats = serde_json::from_str(j).unwrap();
        assert_eq!(s.held_mib(), 32768 - 3113);
        assert!(s.guest_used_mib() > 0);
    }
}
