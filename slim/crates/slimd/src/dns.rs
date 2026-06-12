//! Tiny container DNS: answers A queries for known container names/aliases,
//! forwards everything else to an upstream (the vessel-agent relay at the
//! guest's 127.0.0.1:53, or $NEBULA_DNS_UPSTREAM). Bound on each bridge
//! gateway IP so containers can use the gateway as their nameserver.
//!
//! Hand-rolled DNS message parsing (A/AAAA only) — not worth a crate.

use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};

pub type NameTable = Arc<Mutex<BTreeMap<String, String>>>; // lower(name) -> ipv4

pub struct DnsServer {
    pub names: NameTable,
    upstream: String,
}

impl DnsServer {
    pub fn new() -> Self {
        let upstream =
            std::env::var("NEBULA_DNS_UPSTREAM").unwrap_or_else(|_| "127.0.0.1:53".into());
        Self {
            names: Arc::new(Mutex::new(BTreeMap::new())),
            upstream,
        }
    }

    /// Bind a listener on `gateway_ip:53` (best-effort; logs on failure).
    pub fn listen(&self, gateway_ip: &str) {
        let addr = format!("{gateway_ip}:53");
        let sock = match UdpSocket::bind(&addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("slimd dns: bind {addr} failed: {e}");
                return;
            }
        };
        let names = self.names.clone();
        let upstream = self.upstream.clone();
        std::thread::spawn(move || serve(sock, names, upstream));
    }

    pub fn set(&self, name: &str, ip: &str) {
        self.names
            .lock()
            .unwrap()
            .insert(name.to_ascii_lowercase(), ip.to_string());
    }

    pub fn remove_ip(&self, ip: &str) {
        self.names.lock().unwrap().retain(|_, v| v != ip);
    }
}

impl Default for DnsServer {
    fn default() -> Self {
        Self::new()
    }
}

fn serve(sock: UdpSocket, names: NameTable, upstream: String) {
    let up = UdpSocket::bind("0.0.0.0:0").ok();
    let mut buf = [0u8; 1500];
    loop {
        let Ok((n, peer)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let query = buf[..n].to_vec();
        if let Some((name, qtype)) = parse_question(&query) {
            if qtype == 1 {
                // A
                if let Some(ip) = names
                    .lock()
                    .unwrap()
                    .get(&name.to_ascii_lowercase())
                    .cloned()
                {
                    if let Some(resp) = build_a_response(&query, &ip) {
                        let _ = sock.send_to(&resp, peer);
                        continue;
                    }
                }
            }
        }
        // Forward upstream.
        if let Some(up) = &up {
            if up.send_to(&query, &upstream).is_ok() {
                up.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();
                let mut rbuf = [0u8; 1500];
                if let Ok((rn, _)) = up.recv_from(&mut rbuf) {
                    let _ = sock.send_to(&rbuf[..rn], peer);
                    continue;
                }
            }
        }
        // No upstream: NXDOMAIN-ish (set rcode 3).
        if let Some(resp) = build_error_response(&query) {
            let _ = sock.send_to(&resp, peer);
        }
    }
}

fn parse_question(msg: &[u8]) -> Option<(String, u16)> {
    if msg.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([msg[4], msg[5]]);
    if qdcount == 0 {
        return None;
    }
    let mut i = 12;
    let mut name = String::new();
    while i < msg.len() {
        let len = msg[i] as usize;
        if len == 0 {
            i += 1;
            break;
        }
        if i + 1 + len > msg.len() {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(&msg[i + 1..i + 1 + len]).ok()?);
        i += 1 + len;
    }
    if i + 4 > msg.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([msg[i], msg[i + 1]]);
    Some((name, qtype))
}

fn question_end(msg: &[u8]) -> Option<usize> {
    let mut i = 12;
    while i < msg.len() {
        let len = msg[i] as usize;
        i += 1;
        if len == 0 {
            break;
        }
        i += len;
    }
    Some(i + 4) // qtype + qclass
}

fn build_a_response(query: &[u8], ip: &str) -> Option<Vec<u8>> {
    let qend = question_end(query)?;
    if qend > query.len() {
        return None;
    }
    let octets: Vec<u8> = ip.split('.').filter_map(|o| o.parse().ok()).collect();
    if octets.len() != 4 {
        return None;
    }
    let mut r = query[..qend].to_vec();
    // Flags: response, recursion available, no error.
    r[2] = 0x81;
    r[3] = 0x80;
    // ANCOUNT = 1.
    r[6] = 0;
    r[7] = 1;
    // Answer: name pointer to 0x0c, type A, class IN, ttl 30, rdlen 4, ip.
    r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4]);
    r.extend_from_slice(&octets);
    Some(r)
}

fn build_error_response(query: &[u8]) -> Option<Vec<u8>> {
    let qend = question_end(query)?;
    if qend > query.len() {
        return None;
    }
    let mut r = query[..qend].to_vec();
    r[2] = 0x81;
    r[3] = 0x83; // response + NXDOMAIN
    r[6] = 0;
    r[7] = 0;
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_and_answer() {
        // Query for "db" type A.
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.push(2);
        q.extend_from_slice(b"db");
        q.push(0);
        q.extend_from_slice(&[0, 1, 0, 1]);
        let (name, qt) = parse_question(&q).unwrap();
        assert_eq!(name, "db");
        assert_eq!(qt, 1);
        let resp = build_a_response(&q, "10.88.0.5").unwrap();
        assert_eq!(resp[3], 0x80);
        assert_eq!(&resp[resp.len() - 4..], &[10, 88, 0, 5]);
    }
}
