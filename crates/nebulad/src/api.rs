//! v1alpha1 REST API on 127.0.0.1:7440 — the embedding surface (SDKs, UI).
//!
//! Minimal HTTP/1.1 server (no framework): one thread per connection,
//! JSON in/out, Connection: close. Endpoints:
//!
//!   GET  /v1alpha1/status            engine + agent + memory status
//!   GET  /v1alpha1/stats             balloon/footprint stats
//!   POST /v1alpha1/exec              {"cmd": "...", "args": [...]} -> ExecResult
//!   POST /v1alpha1/balloon           {"target_mib": N}
//!   GET  /v1alpha1/containers        docker containers (proxied)
//!   GET  /healthz                    liveness
//!
//! Auth: localhost-only bind. Token auth + TLS arrive with remote support.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use nebula_core::proto::*;

use crate::balloon::BalloonState;
use crate::vessel::Vessel;

pub const DEFAULT_API_PORT: u16 = 7440;

pub fn start(
    vessel: Arc<Vessel>,
    balloon: Arc<BalloonState>,
    docker_sock: std::path::PathBuf,
    port: u16,
) {
    if port == 0 {
        tracing::info!("REST API disabled (api_port = 0)");
        return;
    }
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("api bind 127.0.0.1:{port} failed: {e}");
                return;
            }
        };
        tracing::info!("REST API on http://127.0.0.1:{port}/v1alpha1");
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let vessel = vessel.clone();
            let balloon = balloon.clone();
            let docker_sock = docker_sock.clone();
            std::thread::spawn(move || {
                let _ = handle(conn, &vessel, &balloon, &docker_sock);
            });
        }
    });
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn handle(
    conn: TcpStream,
    vessel: &Vessel,
    balloon: &BalloonState,
    docker_sock: &std::path::Path,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut writer = conn;

    let req = match parse_request(&mut reader) {
        Ok(r) => r,
        Err(_) => {
            return respond(
                &mut writer,
                400,
                &serde_json::json!({"error": "bad request"}),
            )
        }
    };

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/healthz") => respond(&mut writer, 200, &serde_json::json!({"ok": true})),
        ("GET", "/v1alpha1/status") => {
            let agent = match vessel.agent_request(&AgentRequest::Health) {
                Ok(AgentResponse::Health(h)) => Some(h),
                _ => None,
            };
            let mem = match vessel.agent_request(&AgentRequest::MemStats) {
                Ok(AgentResponse::MemStats(m)) => Some(m),
                _ => None,
            };
            respond(
                &mut writer,
                200,
                &serde_json::json!({
                    "apiVersion": "v1alpha1",
                    "vmState": format!("{:?}", vessel.state()),
                    "cpus": vessel.spec.cpus,
                    "memMib": vessel.spec.mem_mib,
                    "agent": agent,
                    "memory": mem,
                    "uptimeSecs": vessel.started_at.elapsed().as_secs(),
                }),
            )
        }
        ("GET", "/v1alpha1/stats") => {
            let guest = match vessel.agent_request(&AgentRequest::MemStats) {
                Ok(AgentResponse::MemStats(m)) => Some(m),
                _ => None,
            };
            respond(
                &mut writer,
                200,
                &serde_json::json!({
                    "guest": guest,
                    "balloonTargetMib": balloon.target_mib.load(std::sync::atomic::Ordering::Relaxed),
                    "maxMib": balloon.max_mib,
                    "hostFootprintMib": crate::balloon::host_footprint_mib(),
                }),
            )
        }
        ("POST", "/v1alpha1/exec") => {
            #[derive(serde::Deserialize)]
            struct ExecBody {
                cmd: String,
                #[serde(default)]
                args: Vec<String>,
                #[serde(default = "default_timeout")]
                timeout_ms: u64,
            }
            fn default_timeout() -> u64 {
                30_000
            }
            let body: ExecBody = match serde_json::from_slice(&req.body) {
                Ok(b) => b,
                Err(e) => {
                    return respond(
                        &mut writer,
                        400,
                        &serde_json::json!({"error": e.to_string()}),
                    )
                }
            };
            match vessel.agent_request(&AgentRequest::Exec {
                cmd: body.cmd,
                args: body.args,
                env: vec![],
                timeout_ms: body.timeout_ms,
            }) {
                Ok(AgentResponse::Exec(r)) => respond(&mut writer, 200, &serde_json::json!(r)),
                Ok(AgentResponse::Error { message }) => {
                    respond(&mut writer, 500, &serde_json::json!({"error": message}))
                }
                Ok(other) => respond(
                    &mut writer,
                    500,
                    &serde_json::json!({"error": format!("unexpected: {other:?}")}),
                ),
                Err(e) => respond(
                    &mut writer,
                    502,
                    &serde_json::json!({"error": format!("{e:#}")}),
                ),
            }
        }
        ("POST", "/v1alpha1/balloon") => {
            #[derive(serde::Deserialize)]
            struct BalloonBody {
                target_mib: u64,
            }
            let body: BalloonBody = match serde_json::from_slice(&req.body) {
                Ok(b) => b,
                Err(e) => {
                    return respond(
                        &mut writer,
                        400,
                        &serde_json::json!({"error": e.to_string()}),
                    )
                }
            };
            match vessel.balloon_set(body.target_mib) {
                Ok(()) => respond(&mut writer, 200, &serde_json::json!({"ok": true})),
                Err(e) => respond(
                    &mut writer,
                    502,
                    &serde_json::json!({"error": format!("{e:#}")}),
                ),
            }
        }
        ("GET", "/v1alpha1/containers") => {
            // Proxy docker's own API: the SDK gets the full container objects.
            match docker_get(docker_sock, "/v1.43/containers/json?all=true") {
                Ok(body) => respond_raw(&mut writer, 200, &body),
                Err(e) => respond(
                    &mut writer,
                    502,
                    &serde_json::json!({"error": format!("{e:#}")}),
                ),
            }
        }
        _ => respond(&mut writer, 404, &serde_json::json!({"error": "not found"})),
    }
}

fn parse_request(reader: &mut BufReader<TcpStream>) -> anyhow::Result<Request> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    anyhow::ensure!(
        !method.is_empty() && path.starts_with('/'),
        "malformed request line"
    );

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    anyhow::ensure!(content_length <= 1024 * 1024, "body too large");
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Request { method, path, body })
}

fn respond(writer: &mut TcpStream, status: u16, body: &serde_json::Value) -> anyhow::Result<()> {
    respond_raw(writer, status, &serde_json::to_vec(body)?)
}

fn respond_raw(writer: &mut TcpStream, status: u16, body: &[u8]) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        writer,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    writer.write_all(body)?;
    Ok(())
}

/// GET against the proxied docker unix socket (Connection: close).
fn docker_get(sock: &std::path::Path, path: &str) -> anyhow::Result<Vec<u8>> {
    let mut stream = nebula_core::ipc::connect(sock)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n"
    )?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("no header terminator"))?;
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
    let body = &raw[header_end + 4..];
    if headers.contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut pos = 0;
        loop {
            let line_end = body[pos..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or_else(|| anyhow::anyhow!("bad chunk"))?;
            let size = usize::from_str_radix(
                String::from_utf8_lossy(&body[pos..pos + line_end]).trim(),
                16,
            )?;
            pos += line_end + 2;
            if size == 0 {
                break;
            }
            out.extend_from_slice(&body[pos..pos + size]);
            pos += size + 2;
        }
        Ok(out)
    } else {
        Ok(body.to_vec())
    }
}
