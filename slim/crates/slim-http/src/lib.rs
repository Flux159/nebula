//! Minimal threaded HTTP/1.1 server over unix sockets — the transport under
//! slimd's Engine API and the client side of slim-client.
//!
//! Why not hyper/tokio: the Engine API is HTTP/1.1 on a unix socket serving
//! one user and tens of containers. The hard requirement is connection
//! HIJACK (exec/attach raw streams) and per-line-flushed chunked streaming
//! (logs -f, events, pull progress), which are simplest with blocking
//! threads that own their socket. Decision logged in tasks/issues.md (S0).

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;

pub const MAX_HEAD: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct RequestHead {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub headers: Vec<(String, String)>,
}

impl RequestHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn query_bool(&self, name: &str) -> bool {
        matches!(
            self.query.get(name).map(|s| s.as_str()),
            Some("1") | Some("true") | Some("True")
        )
    }

    pub fn query_str(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(|s| s.as_str())
    }
}

/// One in-flight request on a connection. Exactly one terminal call must be
/// made: respond_*, stream(), or hijack().
pub struct Ctx {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    pub head: RequestHead,
    body_remaining: BodyLen,
    sent_continue: bool,
    pub responded: bool,
    hijacked: bool,
    keep_alive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BodyLen {
    None,
    Len(u64),
    Chunked,
}

impl Ctx {
    /// Read the full body (with a sanity cap).
    pub fn body_vec(&mut self, cap: u64) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut rd = self.body_reader().take(cap + 1);
        rd.read_to_end(&mut buf)?;
        if buf.len() as u64 > cap {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        Ok(buf)
    }

    pub fn body_json<T: serde::de::DeserializeOwned + Default>(&mut self) -> io::Result<T> {
        let body = self.body_vec(32 * 1024 * 1024)?;
        if body.is_empty() || body == b"null" {
            return Ok(T::default());
        }
        serde_json::from_slice(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    /// Streaming body access (build context, cp archives). Sends
    /// `100 Continue` first if the client asked for it.
    pub fn body_reader(&mut self) -> BodyReader<'_> {
        if !self.sent_continue {
            self.sent_continue = true;
            if self
                .head
                .header("Expect")
                .map(|v| v.to_ascii_lowercase().contains("100-continue"))
                .unwrap_or(false)
            {
                let _ = self.stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            }
        }
        BodyReader {
            ctx: self,
            chunk_left: 0,
            chunked_done: false,
        }
    }

    fn drain_body(&mut self) {
        if self.body_remaining != BodyLen::None {
            let mut rd = self.body_reader();
            let _ = io::copy(&mut rd, &mut io::sink());
        }
    }

    // ---- responses ----

    pub fn respond_json<T: serde::Serialize>(&mut self, status: u16, body: &T) -> io::Result<()> {
        let json = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
        self.respond_bytes(status, "application/json", &json)
    }

    pub fn respond_error(&mut self, status: u16, msg: impl Into<String>) -> io::Result<()> {
        #[derive(serde::Serialize)]
        struct E {
            message: String,
        }
        self.respond_json(
            status,
            &E {
                message: msg.into(),
            },
        )
    }

    pub fn respond_empty(&mut self, status: u16) -> io::Result<()> {
        self.respond_bytes(status, "application/json", b"")
    }

    /// Escape hatch for responses that need custom headers (e.g. _ping's
    /// Api-Version). Caller writes the full HTTP response. Marks responded.
    pub fn raw_writer(&mut self) -> io::Result<UnixStream> {
        self.drain_body();
        self.responded = true;
        self.stream.try_clone()
    }

    /// Has the client hung up? Non-blocking orderly-shutdown probe (MSG_PEEK)
    /// for long-held endpoints like /wait — docker `run -d` opens /wait and
    /// exits without ever reading the body, so without this every running
    /// container pinned a connection (plus a proxied fd pair in nebulad)
    /// until container exit: the 500-container wall the battle-test found.
    pub fn client_gone(&self) -> bool {
        use std::os::unix::io::AsRawFd;
        let mut b = [0u8; 1];
        let n = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                b.as_mut_ptr() as *mut libc::c_void,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        n == 0 // 0 = peer closed; -1 (EWOULDBLOCK) or pending data = alive
    }

    pub fn respond_bytes(&mut self, status: u16, ctype: &str, body: &[u8]) -> io::Result<()> {
        self.drain_body();
        self.responded = true;
        // One request per connection (Connection: close): a docker Engine API
        // serves one local client; keep-alive reuse with the Go HTTP client
        // was desyncing responses, and closing per-request is simpler and
        // robust at this scale.
        self.keep_alive = false;
        let head = format!(
            "HTTP/1.1 {} {}\r\nServer: nebula-slim\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status,
            status_text(status),
            ctype,
            body.len()
        );
        self.stream.write_all(head.as_bytes())?;
        self.stream.write_all(body)?;
        self.stream.flush()
    }

    /// Start a chunked streaming response (logs -f, events, pull progress).
    /// Each write is flushed; drop or finish() to terminate.
    pub fn stream(&mut self, status: u16, ctype: &str) -> io::Result<ChunkedWriter> {
        self.drain_body();
        self.responded = true;
        self.keep_alive = false; // simplest: one streamed response per conn
        let head = format!(
            "HTTP/1.1 {} {}\r\nServer: nebula-slim\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\n\r\n",
            status,
            status_text(status),
            ctype
        );
        self.stream.write_all(head.as_bytes())?;
        self.stream.flush()?;
        Ok(ChunkedWriter {
            stream: self.stream.try_clone()?,
            done: false,
        })
    }

    /// Take over the raw socket for exec/attach. Replies 101 (if the client
    /// asked to upgrade) or 200 with the raw-stream content type, then hands
    /// back the socket. Any buffered-but-unread bytes (pipelined stdin) are
    /// returned too.
    pub fn hijack(&mut self, multiplexed: bool) -> io::Result<(UnixStream, Vec<u8>)> {
        self.responded = true;
        self.hijacked = true;
        let ctype = if multiplexed {
            "application/vnd.docker.multiplexed-stream"
        } else {
            "application/vnd.docker.raw-stream"
        };
        let upgrade = self
            .head
            .header("Connection")
            .map(|v| v.to_ascii_lowercase().contains("upgrade"))
            .unwrap_or(false);
        let head = if upgrade {
            format!(
                "HTTP/1.1 101 UPGRADED\r\nContent-Type: {ctype}\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n"
            )
        } else {
            format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n\r\n")
        };
        self.stream.write_all(head.as_bytes())?;
        self.stream.flush()?;
        let buffered = self.reader.buffer().to_vec();
        Ok((self.stream.try_clone()?, buffered))
    }
}

pub struct BodyReader<'a> {
    ctx: &'a mut Ctx,
    chunk_left: u64,
    chunked_done: bool,
}

impl Read for BodyReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.ctx.body_remaining {
            BodyLen::None => Ok(0),
            BodyLen::Len(0) => Ok(0),
            BodyLen::Len(n) => {
                let want = buf.len().min(n as usize);
                let got = self.ctx.reader.read(&mut buf[..want])?;
                self.ctx.body_remaining = BodyLen::Len(n - got as u64);
                Ok(got)
            }
            BodyLen::Chunked => {
                if self.chunked_done {
                    return Ok(0);
                }
                if self.chunk_left == 0 {
                    let mut line = String::new();
                    self.ctx.reader.read_line(&mut line)?;
                    let line = line.trim();
                    if line.is_empty() {
                        // CRLF between chunks
                        let mut l2 = String::new();
                        self.ctx.reader.read_line(&mut l2)?;
                        return self.read(buf);
                    }
                    let size = u64::from_str_radix(line.split(';').next().unwrap_or("0"), 16)
                        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk"))?;
                    if size == 0 {
                        // trailing CRLF
                        let mut l2 = String::new();
                        let _ = self.ctx.reader.read_line(&mut l2);
                        self.chunked_done = true;
                        self.ctx.body_remaining = BodyLen::None;
                        return Ok(0);
                    }
                    self.chunk_left = size;
                }
                let want = buf.len().min(self.chunk_left as usize);
                let got = self.ctx.reader.read(&mut buf[..want])?;
                self.chunk_left -= got as u64;
                if self.chunk_left == 0 {
                    let mut crlf = [0u8; 2];
                    let _ = self.ctx.reader.read_exact(&mut crlf);
                }
                Ok(got)
            }
        }
    }
}

pub struct ChunkedWriter {
    stream: UnixStream,
    done: bool,
}

impl ChunkedWriter {
    pub fn finish(mut self) -> io::Result<()> {
        self.finish_inner()
    }

    fn finish_inner(&mut self) -> io::Result<()> {
        if !self.done {
            self.done = true;
            self.stream.write_all(b"0\r\n\r\n")?;
            self.stream.flush()?;
        }
        Ok(())
    }

    /// Lets long-lived streams (logs -f) notice the client hanging up.
    pub fn peer_alive(&self) -> bool {
        // UnixStream::peek is unstable; recv(MSG_PEEK|MSG_DONTWAIT) directly.
        use std::os::unix::io::AsRawFd;
        let mut b = [0u8; 1];
        let n = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                b.as_mut_ptr() as *mut libc::c_void,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        n != 0 // 0 = orderly shutdown; -1/EAGAIN = still open, no data
    }
}

impl Write for ChunkedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.stream, "{:x}\r\n", buf.len())?;
        self.stream.write_all(buf)?;
        self.stream.write_all(b"\r\n")?;
        self.stream.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

impl Drop for ChunkedWriter {
    fn drop(&mut self) {
        let _ = self.finish_inner();
    }
}

/// Raise this process's open-file limit (RLIMIT_NOFILE) soft cap up to the hard
/// cap, like dockerd/containerd do at startup. Each running container holds a
/// few persistent fds (stdout/stderr pipes, log file, ns handles), so the
/// default 1024 soft limit walls density at only a few hundred containers — and
/// hitting it turns benign operations (accept, file open, thread spawn) into
/// hard failures. Best-effort: returns the resulting soft limit, or the error.
pub fn raise_open_file_limit() -> io::Result<u64> {
    // SAFETY: plain libc getrlimit/setrlimit on a zeroed rlimit struct.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return Err(io::Error::last_os_error());
        }
        if lim.rlim_cur < lim.rlim_max {
            lim.rlim_cur = lim.rlim_max;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(lim.rlim_cur as u64)
    }
}

/// Serve forever: one thread per connection, keep-alive within a connection.
/// The handler must terminal-respond on every request (the server 500s if it
/// forgets, and catches panics so a bad request can't kill the daemon).
///
/// Robustness: the accept loop must never die. Under fd/thread exhaustion,
/// `accept()` returns EMFILE/ENFILE and thread spawning fails — we degrade
/// (back off briefly, refuse the one connection) instead of crashing the whole
/// listener, which would drop the socket (EOF) for every client at once.
pub fn serve<F>(path: &Path, handler: F) -> io::Result<()>
where
    F: Fn(&mut Ctx) + Send + Sync + 'static,
{
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)?;
    let handler = Arc::new(handler);
    loop {
        let conn = match listener.accept() {
            Ok((conn, _)) => conn,
            Err(e) => {
                // Transient fd exhaustion: don't break the loop (that would kill
                // the daemon) and don't busy-spin on the still-pending backlog.
                if matches!(e.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE)) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                continue;
            }
        };
        let handler = handler.clone();
        // Builder::spawn returns Err on resource exhaustion instead of
        // panicking (which std::thread::spawn does, taking the listener with
        // it). On failure, reject this one connection and keep serving — and
        // log the errno loudly, since this is the likely culprit behind the
        // "socket dies at ~500 containers" wall (EAGAIN = thread/pids cap) and
        // the panic message would otherwise be lost to a tmpfs log.
        if let Err(e) = std::thread::Builder::new()
            .name("slim-conn".into())
            .spawn(move || handle_conn(conn, &*handler))
        {
            eprintln!(
                "slimd: refusing connection — thread spawn failed: {e} (raw {:?})",
                e.raw_os_error()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
    }
}

fn handle_conn<F: Fn(&mut Ctx)>(stream: UnixStream, handler: &F) {
    let Ok(rstream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::with_capacity(64 * 1024, rstream);
    loop {
        let head = match read_head(&mut reader) {
            Ok(Some(h)) => h,
            _ => return,
        };
        let body_remaining = body_len(&head);
        let keep_alive = head
            .header("Connection")
            .map(|v| !v.eq_ignore_ascii_case("close"))
            .unwrap_or(true);
        let Ok(wstream) = stream.try_clone() else {
            return;
        };
        let Ok(rs2) = reader.get_ref().try_clone() else {
            return;
        };
        let mut ctx = Ctx {
            stream: wstream,
            reader: std::mem::replace(&mut reader, BufReader::new(rs2)),
            head,
            body_remaining,
            sent_continue: false,
            responded: false,
            hijacked: false,
            keep_alive,
        };
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&mut ctx)));
        if res.is_err() && !ctx.responded {
            let _ = ctx.respond_error(500, "internal error in slimd handler");
        }
        if !ctx.responded {
            let _ = ctx.respond_error(500, "handler produced no response");
        }
        if ctx.hijacked || !ctx.keep_alive {
            return;
        }
        ctx.drain_body();
        reader = ctx.reader; // hand the buffer back for the next request
    }
}

fn read_head(reader: &mut BufReader<UnixStream>) -> io::Result<Option<RequestHead>> {
    let mut buf = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = read_until_crlf(reader, &mut line)?;
        if n == 0 {
            return Ok(None); // EOF between requests: clean close
        }
        buf.extend_from_slice(&line);
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too large"));
        }
        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(&buf) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request")),
    }
    let full_path = req.path.unwrap_or("/").to_string();
    let (path, query) = parse_query(&full_path);
    Ok(Some(RequestHead {
        method: req.method.unwrap_or("GET").to_string(),
        path,
        query,
        headers: req
            .headers
            .iter()
            .map(|h| {
                (
                    h.name.to_string(),
                    String::from_utf8_lossy(h.value).into_owned(),
                )
            })
            .collect(),
    }))
}

fn read_until_crlf(reader: &mut BufReader<UnixStream>, out: &mut Vec<u8>) -> io::Result<usize> {
    reader.read_until(b'\n', out)
}

fn body_len(head: &RequestHead) -> BodyLen {
    if head
        .header("Transfer-Encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        return BodyLen::Chunked;
    }
    match head
        .header("Content-Length")
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(0) | None => BodyLen::None,
        Some(n) => BodyLen::Len(n),
    }
}

pub fn parse_query(full: &str) -> (String, BTreeMap<String, String>) {
    let mut query = BTreeMap::new();
    let (path, qs) = match full.split_once('?') {
        Some((p, q)) => (p, q),
        None => (full, ""),
    };
    for pair in qs.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(url_decode(k), url_decode(v));
    }
    (url_decode(path), query)
}

pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn status_text(code: u16) -> &'static str {
    match code {
        101 => "UPGRADED",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        let (p, q) = parse_query("/v1.43/containers/json?all=1&filters=%7B%22a%22%3A1%7D");
        assert_eq!(p, "/v1.43/containers/json");
        assert_eq!(q.get("all").unwrap(), "1");
        assert_eq!(q.get("filters").unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn url_roundtrip() {
        assert_eq!(url_decode("a%2Fb+c"), "a/b c");
        assert_eq!(url_encode("library/alpine:3.19"), "library/alpine%3A3.19");
    }
}
