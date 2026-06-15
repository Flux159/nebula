//! Minimal WebSocket client for the apiserver's pod `exec` subresource
//! (v4.channel.k8s.io) over the kube unix socket. Channel framing: each binary
//! frame is [channel byte] ++ data — 0=stdin, 1=stdout, 2=stderr, 3=error
//! (a Status carrying the exit code), 4=resize. Client→server frames are
//! masked (RFC 6455); server→client frames are not.

use slim_client::http::Client;
use std::io::{self, BufRead, BufReader, Read, Write};

/// Run `cmd` in `pod` via the apiserver and stream stdout/stderr to this
/// process. Returns the command's exit code. With `stdin`, pumps this process's
/// stdin into the exec; with `tty`, requests a pty and forwards window size.
pub fn exec(
    client: &Client,
    ns: &str,
    pod: &str,
    container: &str,
    cmd: &[String],
    tty: bool,
    stdin: bool,
) -> io::Result<i32> {
    let conn = client.connect_stream()?;

    let mut q = String::new();
    for c in cmd {
        q.push_str("&command=");
        q.push_str(&urlenc(c));
    }
    if !container.is_empty() {
        q.push_str("&container=");
        q.push_str(&urlenc(container));
    }
    q.push_str("&stdout=true");
    if !tty {
        q.push_str("&stderr=true");
    }
    if tty {
        q.push_str("&tty=true");
    }
    if stdin {
        q.push_str("&stdin=true");
    }
    let path = format!(
        "/api/v1/namespaces/{ns}/pods/{pod}/exec?{}",
        q.trim_start_matches('&')
    );

    let key = ws_key();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: slim\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Protocol: v4.channel.k8s.io\r\n\r\n"
    );
    let mut wsock = conn.try_clone()?;
    wsock.write_all(req.as_bytes())?;
    wsock.flush()?;

    // Read the upgrade response headers.
    let mut reader = BufReader::new(conn);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    if !status_line.contains(" 101 ") {
        return Err(io::Error::other(format!(
            "exec upgrade failed: {}",
            status_line.trim()
        )));
    }
    loop {
        let mut l = String::new();
        reader.read_line(&mut l)?;
        if l.trim_end().is_empty() {
            break;
        }
    }

    // Raw terminal for interactive ttys; restored on drop.
    let _raw = if tty && stdin && slim_client::tty::is_stdin_tty() {
        slim_client::tty::enter_raw()
    } else {
        None
    };

    if stdin {
        let mut sink = wsock.try_clone()?;
        if tty {
            let (cols, rows) = slim_client::tty::term_size();
            let _ = send_frame(
                &mut sink,
                4,
                format!("{{\"Width\":{cols},\"Height\":{rows}}}").as_bytes(),
            );
        }
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut input = io::stdin();
            loop {
                match input.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = send_close(&mut sink);
                        break;
                    }
                    Ok(n) => {
                        if send_frame(&mut sink, 0, &buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    let mut out = io::stdout();
    let mut err = io::stderr();
    let mut exit_code = 0;
    loop {
        match read_frame(&mut reader)? {
            None => break,
            Some((op, payload)) => {
                if op == 0x8 {
                    break; // close
                }
                // only data frames (text/binary/continuation) carry channels
                if op != 0x0 && op != 0x1 && op != 0x2 {
                    continue;
                }
                if payload.is_empty() {
                    continue;
                }
                let (channel, data) = (payload[0], &payload[1..]);
                match channel {
                    1 => {
                        let _ = out.write_all(data);
                        let _ = out.flush();
                    }
                    2 => {
                        let _ = err.write_all(data);
                        let _ = err.flush();
                    }
                    3 => exit_code = parse_exit(data),
                    _ => {}
                }
            }
        }
    }
    Ok(exit_code)
}

/// Parse the channel-3 Status object for the command's exit code.
fn parse_exit(data: &[u8]) -> i32 {
    let v: serde_json::Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    if v.get("status").and_then(|s| s.as_str()) == Some("Success") {
        return 0;
    }
    if let Some(causes) = v.pointer("/details/causes").and_then(|c| c.as_array()) {
        for c in causes {
            if c.get("reason").and_then(|r| r.as_str()) == Some("ExitCode") {
                if let Some(code) = c
                    .get("message")
                    .and_then(|m| m.as_str())
                    .and_then(|m| m.parse::<i32>().ok())
                {
                    return code;
                }
            }
        }
    }
    1
}

fn read_frame<R: Read + ?Sized>(r: &mut R) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut hdr = [0u8; 2];
    if !read_full(r, &mut hdr)? {
        return Ok(None);
    }
    let opcode = hdr[0] & 0x0f;
    let masked = hdr[1] & 0x80 != 0;
    let mut len = (hdr[1] & 0x7f) as usize;
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as usize;
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b)?;
        len = u64::from_be_bytes(b) as usize;
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }
    Ok(Some((opcode, payload)))
}

fn read_full<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut read = 0;
    while read < buf.len() {
        match r.read(&mut buf[read..]) {
            Ok(0) => return Ok(false),
            Ok(n) => read += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Send a masked channel frame (client→server).
fn send_frame<W: Write>(w: &mut W, channel: u8, data: &[u8]) -> io::Result<()> {
    let mut payload = Vec::with_capacity(1 + data.len());
    payload.push(channel);
    payload.extend_from_slice(data);
    let mask = rand4();
    let mut frame = vec![0x82u8]; // FIN + binary
    let len = payload.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len < 65536 {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        frame.push(b ^ mask[i & 3]);
    }
    w.write_all(&frame)?;
    w.flush()
}

/// Masked close frame with status 1000 (normal).
fn send_close<W: Write>(w: &mut W) -> io::Result<()> {
    let mask = rand4();
    let body = [0x03u8, 0xE8];
    let mut frame = vec![0x88u8, 0x80 | 2];
    frame.extend_from_slice(&mask);
    frame.push(body[0] ^ mask[0]);
    frame.push(body[1] ^ mask[1]);
    w.write_all(&frame)?;
    w.flush()
}

fn ws_key() -> String {
    b64(&rand_bytes::<16>())
}

fn rand4() -> [u8; 4] {
    rand_bytes::<4>()
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    b
}

fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
