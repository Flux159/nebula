//! v1alpha1 REST API — nebula's embedding surface (SDKs, UI, plain HTTP).
//!
//! hyper 1.x on a dedicated tokio current-thread runtime; the rest of
//! nebulad stays sync. Endpoints:
//!
//!   GET  /healthz                    liveness (auth-exempt)
//!   GET  /v1alpha1/status            engine + agent + memory status
//!   GET  /v1alpha1/stats             balloon/footprint stats
//!   GET  /v1alpha1/kubeconfig        standalone kubeconfig (after `kube up`)
//!   POST /v1alpha1/exec              {"cmd": "...", "args": [...]} -> ExecResult
//!   POST /v1alpha1/balloon           {"target_mib": N}
//!   GET  /v1alpha1/containers        compat shim (full plane below)
//!   ANY  /docker/...                 verbatim container-engine proxy
//!   ANY  /k8s/...                    verbatim apiserver proxy (slim only:
//! slim's apiserver-lite speaks plain HTTP on a host socket; k3s is mTLS,
//! so full nebula answers 501 here — fetch /v1alpha1/kubeconfig instead).
//!
//! Binding: `api_host` in config.toml, overridden by NEBULA_API_HOST
//! (default 127.0.0.1). Auth: when NEBULA_API_TOKEN is set, every request
//! except /healthz must carry `Authorization: Bearer <token>`. Binding a
//! non-loopback address REQUIRES a token — refused otherwise.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::{header, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};

use nebula_core::proto::*;

use crate::balloon::BalloonState;
use crate::vessel::Vessel;

pub const DEFAULT_API_PORT: u16 = 7440;

const MAX_BODY: usize = 1024 * 1024;

struct Ctx {
    vessel: Arc<Vessel>,
    balloon: Arc<BalloonState>,
    docker_sock: PathBuf,
    /// slim's plain-HTTP apiserver socket (beside docker.sock); absent when
    /// the guest runs k3s.
    kube_sock: PathBuf,
    kubeconfig: PathBuf,
    token: Option<String>,
}

pub fn start(
    vessel: Arc<Vessel>,
    balloon: Arc<BalloonState>,
    docker_sock: PathBuf,
    kubeconfig: PathBuf,
    host: Option<String>,
    port: u16,
) {
    if port == 0 {
        tracing::info!("REST API disabled (api_port = 0)");
        return;
    }
    let host = std::env::var("NEBULA_API_HOST")
        .ok()
        .or(host)
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let token = std::env::var("NEBULA_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());

    let loopback = host
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false);
    if !loopback && token.is_none() {
        tracing::error!(
            "REST API: refusing to bind {host} without NEBULA_API_TOKEN — \
             a non-loopback API needs bearer auth"
        );
        return;
    }

    let kube_sock = docker_sock
        .parent()
        .map(|d| d.join("slim-kube.sock"))
        .unwrap_or_default();
    let ctx = Arc::new(Ctx {
        vessel,
        balloon,
        docker_sock,
        kube_sock,
        kubeconfig,
        token,
    });

    std::thread::Builder::new()
        .name("rest-api".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("api runtime: {e}");
                    return;
                }
            };
            rt.block_on(serve(host, port, ctx));
        })
        .ok();
}

async fn serve(host: String, port: u16, ctx: Arc<Ctx>) {
    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("api bind {host}:{port} failed: {e}");
            return;
        }
    };
    tracing::info!(
        "REST API on http://{host}:{port}/v1alpha1 (auth: {})",
        if ctx.token.is_some() {
            "bearer"
        } else {
            "off (loopback)"
        }
    );
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = hyper::service::service_fn(move |req| route(ctx.clone(), req));
            // with_upgrades: docker attach/exec hijack the connection.
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .with_upgrades()
                .await
            {
                tracing::debug!("api conn: {e}");
            }
        });
    }
}

type Resp = Response<BoxBody<Bytes, hyper::Error>>;

fn full(body: impl Into<Bytes>) -> BoxBody<Bytes, hyper::Error> {
    Full::new(body.into()).map_err(|n| match n {}).boxed()
}

fn json(status: u16, value: &serde_json::Value) -> Resp {
    let body = serde_json::to_vec(value).unwrap_or_default();
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(full(body))
        .unwrap()
}

fn err_json(status: u16, msg: impl std::fmt::Display) -> Resp {
    json(status, &serde_json::json!({ "error": msg.to_string() }))
}

/// Constant-time token comparison (length leak is fine; content isn't).
fn token_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

async fn route(ctx: Arc<Ctx>, req: Request<Incoming>) -> Result<Resp, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Liveness probes stay auth-exempt and CORS preflights carry no auth.
    if method == Method::GET && path == "/healthz" {
        return Ok(json(200, &serde_json::json!({"ok": true})));
    }
    if method == Method::OPTIONS {
        return Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                "GET, POST, DELETE, OPTIONS",
            )
            .header(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                "authorization, content-type",
            )
            .body(full(Bytes::new()))
            .unwrap());
    }

    if let Some(token) = &ctx.token {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| token_eq(t, token));
        if !ok {
            return Ok(err_json(401, "unauthorized"));
        }
    }

    // Verbatim container plane: the engine's own Docker API (dockerd in
    // full nebula, slimd's reimplementation in slim) behind our auth.
    if let Some(path_q) = strip_plane(&req, &path, "/docker") {
        return Ok(sock_proxy(&ctx.docker_sock, req, &path_q).await);
    }
    // Verbatim kubernetes plane — slim only (plain-HTTP apiserver-lite
    // socket). k3s speaks mTLS, which can't be meaningfully re-proxied:
    // those clients fetch /v1alpha1/kubeconfig and dial the apiserver.
    if let Some(path_q) = strip_plane(&req, &path, "/k8s") {
        if !ctx.kube_sock.exists() {
            return Ok(err_json(
                501,
                "no plain-HTTP apiserver (k3s guest?) — use /v1alpha1/kubeconfig",
            ));
        }
        return Ok(sock_proxy(&ctx.kube_sock, req, &path_q).await);
    }

    match (method, path.as_str()) {
        (Method::GET, "/v1alpha1/status") => {
            let vessel = ctx.vessel.clone();
            let out = tokio::task::spawn_blocking(move || {
                let agent = match vessel.agent_request(&AgentRequest::Health) {
                    Ok(AgentResponse::Health(h)) => Some(h),
                    _ => None,
                };
                let mem = match vessel.agent_request(&AgentRequest::MemStats) {
                    Ok(AgentResponse::MemStats(m)) => Some(m),
                    _ => None,
                };
                serde_json::json!({
                    "apiVersion": "v1alpha1",
                    "vmState": format!("{:?}", vessel.state()),
                    "cpus": vessel.spec.cpus,
                    "memMib": vessel.spec.mem_mib,
                    "agent": agent,
                    "memory": mem,
                    "uptimeSecs": vessel.started_at.elapsed().as_secs(),
                })
            })
            .await;
            Ok(match out {
                Ok(v) => json(200, &v),
                Err(e) => err_json(500, e),
            })
        }
        (Method::GET, "/v1alpha1/stats") => {
            let vessel = ctx.vessel.clone();
            let target = ctx
                .balloon
                .target_mib
                .load(std::sync::atomic::Ordering::Relaxed);
            let max = ctx.balloon.max_mib;
            let out = tokio::task::spawn_blocking(move || {
                let guest = match vessel.agent_request(&AgentRequest::MemStats) {
                    Ok(AgentResponse::MemStats(m)) => Some(m),
                    _ => None,
                };
                serde_json::json!({
                    "guest": guest,
                    "balloonTargetMib": target,
                    "maxMib": max,
                    "hostFootprintMib": crate::balloon::host_footprint_mib(),
                })
            })
            .await;
            Ok(match out {
                Ok(v) => json(200, &v),
                Err(e) => err_json(500, e),
            })
        }
        (Method::GET, "/v1alpha1/kubeconfig") => match std::fs::read_to_string(&ctx.kubeconfig) {
            Ok(yaml) => Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/yaml")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(full(yaml))
                .unwrap()),
            Err(_) => Ok(err_json(
                404,
                "no kubeconfig — run `nebula kube up` (or POST /v1alpha1/exec) first",
            )),
        },
        (Method::POST, "/v1alpha1/exec") => {
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
            let Ok(bytes) = read_body(req).await else {
                return Ok(err_json(400, "body too large"));
            };
            let body: ExecBody = match serde_json::from_slice(&bytes) {
                Ok(b) => b,
                Err(e) => return Ok(err_json(400, e)),
            };
            let vessel = ctx.vessel.clone();
            let out = tokio::task::spawn_blocking(move || {
                vessel.agent_request(&AgentRequest::Exec {
                    cmd: body.cmd,
                    args: body.args,
                    env: vec![],
                    timeout_ms: body.timeout_ms,
                })
            })
            .await;
            Ok(match out {
                Ok(Ok(AgentResponse::Exec(r))) => json(200, &serde_json::json!(r)),
                Ok(Ok(AgentResponse::Error { message })) => err_json(500, message),
                Ok(Ok(other)) => err_json(500, format!("unexpected: {other:?}")),
                Ok(Err(e)) => err_json(502, format!("{e:#}")),
                Err(e) => err_json(500, e),
            })
        }
        (Method::POST, "/v1alpha1/balloon") => {
            #[derive(serde::Deserialize)]
            struct BalloonBody {
                target_mib: u64,
            }
            let Ok(bytes) = read_body(req).await else {
                return Ok(err_json(400, "body too large"));
            };
            let body: BalloonBody = match serde_json::from_slice(&bytes) {
                Ok(b) => b,
                Err(e) => return Ok(err_json(400, e)),
            };
            let vessel = ctx.vessel.clone();
            let out =
                tokio::task::spawn_blocking(move || vessel.balloon_set(body.target_mib)).await;
            Ok(match out {
                Ok(Ok(())) => json(200, &serde_json::json!({"ok": true})),
                Ok(Err(e)) => err_json(502, format!("{e:#}")),
                Err(e) => err_json(500, e),
            })
        }
        (Method::GET, "/v1alpha1/containers") => {
            // Compat shim from before the verbatim /docker plane existed.
            Ok(sock_proxy(&ctx.docker_sock, req, "/v1.43/containers/json?all=true").await)
        }
        _ => Ok(err_json(404, "not found")),
    }
}

async fn read_body(req: Request<Incoming>) -> Result<Bytes, ()> {
    Limited::new(req.into_body(), MAX_BODY)
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|_| ())
}

/// `/docker/foo?q` -> `Some("/foo?q")` when `path` is under `prefix`.
fn strip_plane(req: &Request<Incoming>, path: &str, prefix: &str) -> Option<String> {
    if path != prefix && !path.starts_with(&format!("{prefix}/")) {
        return None;
    }
    let rest = &path[prefix.len()..];
    let target = if rest.is_empty() { "/" } else { rest };
    Some(match req.uri().query() {
        Some(q) => format!("{target}?{q}"),
        None => target.to_string(),
    })
}

/// Forward a request verbatim to an engine socket, streaming the response
/// back. `path_q` replaces the URI (the plane prefix stripped).
async fn sock_proxy(sock: &std::path::Path, req: Request<Incoming>, path_q: &str) -> Resp {
    #[cfg(unix)]
    let stream = match tokio::net::UnixStream::connect(sock).await {
        Ok(s) => s,
        Err(e) => return err_json(502, format!("engine socket: {e}")),
    };
    #[cfg(windows)]
    let stream = {
        // The ipc convention on Windows: the "socket" is a file holding a
        // loopback TCP port.
        let port = match std::fs::read_to_string(sock)
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
        {
            Some(p) => p,
            None => return err_json(502, "engine port file unreadable"),
        };
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => s,
            Err(e) => return err_json(502, format!("engine connect: {e}")),
        }
    };
    proxy_over(stream, req, path_q).await
}

async fn proxy_over<S>(stream: S, req: Request<Incoming>, path_q: &str) -> Resp
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(io).await {
        Ok(x) => x,
        Err(e) => return err_json(502, format!("docker handshake: {e}")),
    };
    tokio::spawn(async move {
        // with_upgrades keeps hijacked attach/exec streams alive.
        if let Err(e) = conn.with_upgrades().await {
            tracing::debug!("docker proxy conn: {e}");
        }
    });

    let (mut parts, body) = req.into_parts();
    parts.uri = match path_q.parse() {
        Ok(u) => u,
        Err(e) => return err_json(400, format!("bad path: {e}")),
    };
    parts
        .headers
        .insert(header::HOST, "docker".parse().unwrap());

    match sender.send_request(Request::from_parts(parts, body)).await {
        Ok(resp) => {
            let (mut parts, body) = resp.into_parts();
            parts
                .headers
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
            Response::from_parts(parts, body.boxed())
        }
        Err(e) => err_json(502, format!("docker request: {e}")),
    }
}
