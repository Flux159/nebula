//! Minimal Docker Engine API client. Blocking, one connection per request, plus
//! a hijack path for attach/exec.
//!
//! Transport is unix-socket on macOS/Linux (where nebula exposes docker.sock /
//! slim-kube.sock) and **loopback TCP on Windows** (no AF_UNIX in std; nebula's
//! WHP host proxy maps the guest vsock ports to loopback TCP). `DOCKER_HOST` /
//! `SLIM_SOCKET` / `SLIM_KUBE_SOCKET` accept `tcp://host:port` on every platform
//! and `unix:///path` (or a bare path) on unix.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::PathBuf;

/// Where the engine API lives.
#[derive(Clone, Debug)]
pub enum Endpoint {
    #[cfg(unix)]
    Unix(PathBuf),
    Tcp(String), // host:port
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(unix)]
            Endpoint::Unix(p) => write!(f, "{}", p.display()),
            Endpoint::Tcp(a) => write!(f, "tcp://{a}"),
        }
    }
}

/// A connected engine stream — unix socket or TCP, cloneable for duplex IO.
pub enum Stream {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    pub fn try_clone(&self) -> io::Result<Stream> {
        Ok(match self {
            #[cfg(unix)]
            Stream::Unix(s) => Stream::Unix(s.try_clone()?),
            Stream::Tcp(s) => Stream::Tcp(s.try_clone()?),
        })
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.read(buf),
            Stream::Tcp(s) => s.read(buf),
        }
    }
}
impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.write(buf),
            Stream::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => s.flush(),
            Stream::Tcp(s) => s.flush(),
        }
    }
}

pub struct Client {
    pub endpoint: Endpoint,
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub stream: BufReader<Stream>,
    body_len: BodyLen,
}

enum BodyLen {
    Len(u64),
    Chunked,
    Eof,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for ApiError {}

/// Parse a transport string: `tcp://host:port` (all platforms), `unix:///path`
/// or a bare absolute path (unix only).
fn parse_endpoint(s: &str) -> Option<Endpoint> {
    if let Some(a) = s.strip_prefix("tcp://") {
        return Some(Endpoint::Tcp(a.to_string()));
    }
    #[cfg(unix)]
    {
        if let Some(p) = s.strip_prefix("unix://") {
            return Some(Endpoint::Unix(PathBuf::from(p)));
        }
        if s.starts_with('/') {
            return Some(Endpoint::Unix(PathBuf::from(s)));
        }
    }
    None
}

impl Client {
    /// Build a client for an explicit endpoint (e.g., a discovered tcp:// host).
    pub fn from_endpoint(ep: Endpoint) -> Client {
        Client { endpoint: ep }
    }

    pub fn discover() -> Client {
        Client {
            endpoint: discover_endpoint(
                &["DOCKER_HOST", "SLIM_SOCKET"],
                "docker.sock",
                "/var/run/docker.sock",
            ),
        }
    }

    /// The slim apiserver-lite endpoint (kubectl-slim/helm-slim). slimd serves it
    /// next to docker.sock; nebula's proxy mirrors it on the host.
    pub fn discover_kube() -> Client {
        Client {
            endpoint: discover_endpoint(
                &["SLIM_KUBE_SOCKET", "SLIM_KUBE_HOST"],
                "slim-kube.sock",
                "/var/run/slim-kube.sock",
            ),
        }
    }

    fn connect(&self) -> io::Result<Stream> {
        let s = match &self.endpoint {
            #[cfg(unix)]
            Endpoint::Unix(p) => Stream::Unix(UnixStream::connect(p)?),
            Endpoint::Tcp(a) => Stream::Tcp(TcpStream::connect(a)?),
        };
        Ok(s)
    }

    /// Public connect for duplex clients (e.g., the kube exec WebSocket).
    pub fn connect_stream(&self) -> io::Result<Stream> {
        self.connect().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Cannot connect to the slim engine at {}: {e}\nIs the engine running?",
                    self.endpoint
                ),
            )
        })
    }

    /// Send a request and parse response headers. Caller reads the body.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> io::Result<Response> {
        let mut stream = self.connect_stream()?;
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: slim\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        } else if method == "POST" || method == "PUT" {
            req.push_str("Content-Length: 0\r\n");
        }
        req.push_str("Connection: close\r\n\r\n");
        stream.write_all(req.as_bytes())?;
        if let Some(b) = body {
            stream.write_all(b)?;
        }
        stream.flush()?;
        Self::read_response(stream)
    }

    fn read_response(stream: Stream) -> io::Result<Response> {
        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let t = line.trim_end();
            if t.is_empty() {
                break;
            }
            if let Some((k, v)) = t.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let body_len = if headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        }) {
            BodyLen::Chunked
        } else if let Some((_, v)) = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        {
            BodyLen::Len(v.parse().unwrap_or(0))
        } else {
            BodyLen::Eof
        };
        Ok(Response {
            status,
            headers,
            stream: reader,
            body_len,
        })
    }

    /// Like `request`, but streams `len` bytes from `body` instead of holding
    /// the whole payload in memory — `docker load` of a multi-hundred-MB
    /// archive must not be an allocation.
    pub fn request_reader(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        len: u64,
        body: &mut dyn Read,
    ) -> io::Result<Response> {
        let mut stream = self.connect_stream()?;
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: slim\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str(&format!("Content-Length: {len}\r\n"));
        req.push_str("Connection: close\r\n\r\n");
        stream.write_all(req.as_bytes())?;
        io::copy(body, &mut stream)?;
        stream.flush()?;
        Self::read_response(stream)
    }

    /// Convenience: request and return the full decoded body bytes.
    pub fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> io::Result<(u16, Vec<u8>)> {
        let mut resp = self.request(method, path, headers, body)?;
        let bytes = resp.read_body()?;
        Ok((resp.status, bytes))
    }

    /// JSON request returning a parsed value, erroring on non-2xx.
    pub fn json<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, ApiError> {
        let raw = body.map(|b| serde_json::to_vec(b).unwrap_or_default());
        let (status, bytes) = self
            .call(
                method,
                path,
                &[("Content-Type", "application/json")],
                raw.as_deref(),
            )
            .map_err(|e| ApiError {
                status: 0,
                message: e.to_string(),
            })?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|e| ApiError {
            status,
            message: format!("bad response: {e}"),
        })
    }

    /// POST/DELETE expecting an empty 2xx; returns the status.
    pub fn action(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<u16, ApiError> {
        let raw = body.map(|b| serde_json::to_vec(b).unwrap_or_default());
        let (status, bytes) = self
            .call(
                method,
                path,
                &[("Content-Type", "application/json")],
                raw.as_deref(),
            )
            .map_err(|e| ApiError {
                status: 0,
                message: e.to_string(),
            })?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, &bytes));
        }
        Ok(status)
    }

    /// Hijack a connection (attach/exec). Returns the raw stream after the
    /// response headers; caller does bidirectional IO.
    pub fn hijack(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> io::Result<Stream> {
        let mut stream = self.connect_stream()?;
        let raw = body.map(|b| serde_json::to_vec(b).unwrap_or_default());
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: slim\r\nContent-Type: application/json\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n"
        );
        if let Some(b) = &raw {
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        } else {
            req.push_str("Content-Length: 0\r\n");
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes())?;
        if let Some(b) = &raw {
            stream.write_all(b)?;
        }
        stream.flush()?;
        // Consume response headers up to the blank line, then hand back the
        // socket positioned at the raw stream.
        let mut reader = BufReader::new(stream.try_clone()?);
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 || line.trim_end().is_empty() {
                break;
            }
        }
        Ok(stream)
    }
}

/// Discover an endpoint: first env var that parses wins; else (unix) the first
/// existing socket among nebula locations, else (windows) loopback TCP.
/// The loopback port recorded in an instance's `run/<leaf>` file.
///
/// On Windows that path holds a port rather than being a socket. Kept out of
/// the `cfg` blocks deliberately: the parsing is what regresses, and a
/// Windows-only function is one no test on Linux or macOS can reach.
// Called only from the Windows branch below, but deliberately compiled and
// tested everywhere: a `#[cfg(windows)]` helper is one no CI run on Linux can
// reach, and this parsing is the part that regresses.
#[allow(dead_code)]
fn port_from_port_file(text: &str) -> Option<&str> {
    let port = text.trim();
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // A port file holding something absurd is worse than none: it produces a
    // connection error naming an endpoint nobody configured.
    match port.parse::<u32>() {
        Ok(n) if n > 0 && n < 65536 => Some(port),
        _ => None,
    }
}

fn discover_endpoint(envs: &[&str], leaf: &str, default_unix: &str) -> Endpoint {
    for var in envs {
        if let Ok(v) = std::env::var(var) {
            if let Some(ep) = parse_endpoint(&v) {
                return ep;
            }
        }
    }
    #[cfg(unix)]
    {
        let candidates = [
            std::env::var("NEBULA_HOME")
                .ok()
                .map(|h| format!("{h}/run/{leaf}")),
            Some(format!(
                "{}/.nebula/run/{leaf}",
                std::env::var("HOME").unwrap_or_default()
            )),
            Some(default_unix.to_string()),
        ];
        for c in candidates.into_iter().flatten() {
            if std::path::Path::new(&c).exists() {
                return Endpoint::Unix(PathBuf::from(c));
            }
        }
        Endpoint::Unix(PathBuf::from(default_unix))
    }
    #[cfg(not(unix))]
    {
        let _ = default_unix;
        // Windows: nebula publishes the engine on a loopback TCP port and
        // records it in the instance directory, at the same path the unix
        // socket would occupy -- run/docker.sock is a text file holding the
        // port rather than a socket. Read it, mirroring the unix branch above.
        //
        // Without this the only way to reach an engine was to set DOCKER_HOST
        // by hand, and the port changes on every boot, so an embedder had to
        // read this file themselves -- which is exactly what the client is
        // supposed to do for them. The failure was also misleading: falling
        // through to 2375 reports "cannot connect to the engine" while the
        // engine is running.
        let candidates = [
            std::env::var("NEBULA_HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join("run").join(leaf)),
            std::env::var("USERPROFILE").ok().map(|h| {
                std::path::PathBuf::from(h)
                    .join(".nebula")
                    .join("run")
                    .join(leaf)
            }),
        ];
        for c in candidates.into_iter().flatten() {
            if let Ok(text) = std::fs::read_to_string(&c) {
                if let Some(port) = port_from_port_file(&text) {
                    return Endpoint::Tcp(format!("127.0.0.1:{port}"));
                }
            }
        }
        let port = if envs.iter().any(|v| v.contains("KUBE")) {
            "6443"
        } else {
            "2375"
        };
        Endpoint::Tcp(format!("127.0.0.1:{port}"))
    }
}

impl Response {
    pub fn read_body(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        match self.body_len {
            BodyLen::Len(n) => {
                let mut buf = vec![0u8; n as usize];
                self.stream.read_exact(&mut buf)?;
                out = buf;
            }
            BodyLen::Eof => {
                self.stream.read_to_end(&mut out)?;
            }
            BodyLen::Chunked => {
                self.read_chunked(&mut |c| out.extend_from_slice(c))?;
            }
        }
        Ok(out)
    }

    /// Stream the (possibly chunked) body, invoking `sink` per chunk.
    pub fn stream_body(&mut self, mut sink: impl FnMut(&[u8])) -> io::Result<()> {
        match self.body_len {
            BodyLen::Chunked => self.read_chunked(&mut sink),
            BodyLen::Len(n) => {
                let mut remaining = n;
                let mut buf = [0u8; 8192];
                while remaining > 0 {
                    let want = buf.len().min(remaining as usize);
                    let got = self.stream.read(&mut buf[..want])?;
                    if got == 0 {
                        break;
                    }
                    sink(&buf[..got]);
                    remaining -= got as u64;
                }
                Ok(())
            }
            BodyLen::Eof => {
                let mut buf = [0u8; 8192];
                loop {
                    let got = self.stream.read(&mut buf)?;
                    if got == 0 {
                        break;
                    }
                    sink(&buf[..got]);
                }
                Ok(())
            }
        }
    }

    fn read_chunked(&mut self, sink: &mut dyn FnMut(&[u8])) -> io::Result<()> {
        loop {
            let mut size_line = String::new();
            self.stream.read_line(&mut size_line)?;
            let size = u64::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
                .unwrap_or(0);
            if size == 0 {
                let mut trailer = String::new();
                let _ = self.stream.read_line(&mut trailer);
                break;
            }
            let mut buf = vec![0u8; size as usize];
            self.stream.read_exact(&mut buf)?;
            sink(&buf);
            let mut crlf = [0u8; 2];
            let _ = self.stream.read_exact(&mut crlf);
        }
        Ok(())
    }
}

fn api_error(status: u16, bytes: &[u8]) -> ApiError {
    let message = serde_json::from_slice::<slim_api::ErrorResponse>(bytes)
        .map(|e| e.message)
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).trim().to_string());
    // A bodyless error (a socket proxy in front of an engine that isn't
    // listening yet) would otherwise print as a bare "Error: ".
    let message = if message.is_empty() {
        format!("engine returned HTTP {status} with no message")
    } else {
        message
    };
    ApiError { status, message }
}

/// stdcopy demux: split a multiplexed docker stream into stdout/stderr.
pub fn demux_stdcopy(
    data: &[u8],
    mut on_stdout: impl FnMut(&[u8]),
    mut on_stderr: impl FnMut(&[u8]),
) {
    let mut i = 0;
    while i + 8 <= data.len() {
        let stream = data[i];
        let len = u32::from_be_bytes([data[i + 4], data[i + 5], data[i + 6], data[i + 7]]) as usize;
        i += 8;
        if i + len > data.len() {
            break;
        }
        let payload = &data[i..i + len];
        match stream {
            2 => on_stderr(payload),
            _ => on_stdout(payload),
        }
        i += len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Runs on every platform on purpose: this is the parsing behind Windows
    // endpoint discovery, and nothing in CI runs Windows tests.
    #[test]
    fn port_files_are_read_and_junk_is_rejected() {
        assert_eq!(port_from_port_file("63692"), Some("63692"));
        assert_eq!(port_from_port_file("  63692\n"), Some("63692"));
        assert_eq!(port_from_port_file("1"), Some("1"));
        assert_eq!(port_from_port_file("65535"), Some("65535"));

        for junk in [
            "",
            "   ",
            "0",
            "65536",
            "99999999",
            "abc",
            "tcp://127.0.0.1:1",
            "12a4",
            "-5",
        ] {
            assert_eq!(
                port_from_port_file(junk),
                None,
                "{junk:?} should be rejected"
            );
        }
    }
}
