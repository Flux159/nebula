//! Minimal Docker Engine API client over a unix socket. Blocking, one
//! connection per request, plus a hijack path for attach/exec.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

pub struct Client {
    pub socket: PathBuf,
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub stream: BufReader<UnixStream>,
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

impl Client {
    pub fn discover() -> Client {
        // DOCKER_HOST=unix:///path wins; else common slim/nebula locations.
        if let Ok(h) = std::env::var("DOCKER_HOST") {
            if let Some(p) = h.strip_prefix("unix://") {
                return Client { socket: PathBuf::from(p) };
            }
        }
        let candidates = [
            std::env::var("SLIM_SOCKET").ok(),
            std::env::var("NEBULA_HOME").ok().map(|h| format!("{h}/run/docker.sock")),
            Some(format!(
                "{}/.nebula/run/docker.sock",
                std::env::var("HOME").unwrap_or_default()
            )),
            Some("/var/run/docker.sock".into()),
        ];
        for c in candidates.into_iter().flatten() {
            if std::path::Path::new(&c).exists() {
                return Client { socket: PathBuf::from(c) };
            }
        }
        Client { socket: PathBuf::from("/var/run/docker.sock") }
    }

    fn connect(&self) -> io::Result<UnixStream> {
        UnixStream::connect(&self.socket).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Cannot connect to the slim engine at {}: {e}\nIs the engine running?",
                    self.socket.display()
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
        let mut stream = self.connect()?;
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
            k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
        }) {
            BodyLen::Chunked
        } else if let Some((_, v)) =
            headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        {
            BodyLen::Len(v.parse().unwrap_or(0))
        } else {
            BodyLen::Eof
        };
        Ok(Response { status, headers, stream: reader, body_len })
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
            .call(method, path, &[("Content-Type", "application/json")], raw.as_deref())
            .map_err(|e| ApiError { status: 0, message: e.to_string() })?;
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
            .call(method, path, &[("Content-Type", "application/json")], raw.as_deref())
            .map_err(|e| ApiError { status: 0, message: e.to_string() })?;
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
    ) -> io::Result<UnixStream> {
        let mut stream = self.connect()?;
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
        // Anything buffered after headers belongs to the stream; with our
        // server hijack there is typically none. Return the original stream.
        Ok(stream)
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
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned());
    ApiError { status, message }
}

/// stdcopy demux: split a multiplexed docker stream into stdout/stderr.
pub fn demux_stdcopy(data: &[u8], mut on_stdout: impl FnMut(&[u8]), mut on_stderr: impl FnMut(&[u8])) {
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
