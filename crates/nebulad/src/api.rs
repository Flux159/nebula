//! v1alpha1 REST API — nebula's embedding surface (SDKs, UI, plain HTTP).
//!
//! hyper 1.x on a dedicated tokio current-thread runtime; the rest of
//! nebulad stays sync. Endpoints:
//!
//! ```text
//!   GET  /healthz                    liveness (auth-exempt)
//!   GET  /v1alpha1/status            engine + agent + memory status
//!   GET  /v1alpha1/stats             balloon/footprint stats
//!   GET  /v1alpha1/kubeconfig        standalone kubeconfig (after `kube up`)
//!   POST /v1alpha1/exec              {"cmd": "...", "args": [...]} -> ExecResult
//!   POST /v1alpha1/balloon           {"target_mib": N}
//!   GET  /v1alpha1/containers        compat shim (full plane below)
//!   ANY  /v1alpha1/vessels[...]      vessel lifecycle (list/create/start/
//!                                    stop/rm/exec/snapshots/restore/branch)
//!   ANY  /docker/...                 verbatim container-engine proxy
//!   ANY  /k8s/...                    verbatim apiserver proxy (slim only;
//!                                    k3s guests answer 501 — fetch
//!                                    /v1alpha1/kubeconfig instead)
//! ```
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
    // Already resolved (config + NEBULA_API_HOST) and preflighted by
    // `ports::PortPlan`; re-deriving it here is how the check and the bind
    // drift apart.
    host: String,
    port: u16,
) {
    if port == 0 {
        tracing::info!("REST API disabled (api_port = 0)");
        return;
    }
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
        crate::ports::set_bind(
            "api",
            format!("{host}:{port}"),
            Some("refused: non-loopback bind needs NEBULA_API_TOKEN".into()),
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
            crate::ports::set_bind("api", format!("{host}:{port}"), Some(e.to_string()));
            return;
        }
    };
    crate::ports::set_bind("api", format!("{host}:{port}"), None);
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

    if let Some(rest) = path.strip_prefix("/v1alpha1/vessels") {
        let rest = rest.trim_start_matches('/').to_string();
        let query = req.uri().query().unwrap_or_default().to_string();
        return vessels_route(ctx.clone(), method, rest, query, req).await;
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
                    // Embedders point clients at these; a listener that
                    // failed to bind is why an otherwise healthy instance
                    // serves nothing.
                    "ports": crate::ports::binds(),
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

// --- vessels (named microVMs) -------------------------------------------------

/// Routes under /v1alpha1/vessels. `rest` is the path after the prefix with
/// the leading slash stripped: "", "<name>", or "<name>/<action>[/<label>]".
async fn vessels_route(
    ctx: Arc<Ctx>,
    method: Method,
    rest: String,
    query: String,
    req: Request<Incoming>,
) -> Result<Resp, hyper::Error> {
    use nebula_core::vessels as v;

    let force = query.split('&').any(|kv| kv == "force=true");

    let segs: Vec<String> = if rest.is_empty() {
        vec![]
    } else {
        rest.split('/').map(str::to_string).collect()
    };

    // Everything here drives the same nebula_core::vessels the CLI uses;
    // all of it blocks (worker spawns, agent waits), so run off-loop.
    macro_rules! blocking {
        ($body:expr) => {{
            let out = tokio::task::spawn_blocking(move || $body).await;
            Ok(match out {
                Ok(Ok(value)) => json(200, &serde_json::json!(value)),
                Ok(Err(e)) => err_json(409, format!("{e:#}")),
                Err(e) => err_json(500, e),
            })
        }};
    }

    match (method, segs.as_slice()) {
        (Method::GET, []) => blocking!(v::list()),
        (Method::POST, []) => {
            #[derive(serde::Deserialize)]
            struct CreateBody {
                name: String,
                #[serde(default = "d_cpus")]
                cpus: u32,
                #[serde(default = "d_mem")]
                mem_mib: u64,
                #[serde(default)]
                gpu: bool,
                #[serde(default = "d_disk")]
                data_gib: u64,
                #[serde(default = "d_backend")]
                backend: String,
                /// "name:GiB" strings, same shape as the CLI's --volume.
                #[serde(default)]
                volumes: Vec<String>,
                /// Host directories to share in at their identical absolute
                /// paths, same shape as the CLI's --mount ("/path" or "/path:ro").
                #[serde(default)]
                mounts: Vec<String>,
                /// Build the rootfs from a docker image ref (pulled into the
                /// engine if absent) — `vessels new --from-image` over HTTP.
                #[serde(default)]
                from_image: Option<String>,
                /// Clone the rootfs from a raw .img file on the HOST (made by
                /// `vessels convert-image`). Not gzip — send the raw image.
                #[serde(default)]
                rootfs_img: Option<String>,
                /// Rootfs size in MiB when building from an image.
                #[serde(default = "d_rootfs_mb")]
                rootfs_mb: u64,
                /// Create only — don't boot it.
                #[serde(default)]
                no_start: bool,
            }
            fn d_cpus() -> u32 {
                2
            }
            fn d_mem() -> u64 {
                2048
            }
            fn d_disk() -> u64 {
                16
            }
            fn d_rootfs_mb() -> u64 {
                4096
            }
            fn d_backend() -> String {
                "krun".into()
            }
            let Ok(bytes) = read_body(req).await else {
                return Ok(err_json(400, "body too large"));
            };
            let body: CreateBody = match serde_json::from_slice(&bytes) {
                Ok(b) => b,
                Err(e) => return Ok(err_json(400, e)),
            };
            if body.from_image.is_some() && body.rootfs_img.is_some() {
                return Ok(err_json(
                    400,
                    "from_image and rootfs_img are mutually exclusive",
                ));
            }

            let dir = match v::dir_of(&body.name) {
                Ok(d) => d,
                Err(e) => return Ok(err_json(400, format!("{e:#}"))),
            };
            if dir.exists() {
                return Ok(err_json(
                    409,
                    format!("vessel `{}` already exists", body.name),
                ));
            }

            // Engine-built rootfs (async docker REST + in-engine mkfs) before
            // the blocking create.
            let rootfs = if let Some(image) = body.from_image.clone() {
                if std::fs::create_dir_all(&dir).is_err() {
                    return Ok(err_json(500, "cannot create vessel dir"));
                }
                let built = crate::images::build_rootfs_from_image(
                    ctx.vessel.clone(),
                    ctx.docker_sock.clone(),
                    image,
                    body.name.clone(),
                    dir.clone(),
                    body.rootfs_mb,
                    body.data_gib,
                )
                .await;
                if let Err(e) = built {
                    let _ = std::fs::remove_dir_all(&dir);
                    return Ok(err_json(409, format!("{e:#}")));
                }
                v::Rootfs::Prepared
            } else if let Some(src) = body.rootfs_img.clone() {
                let src = std::path::PathBuf::from(src);
                if !src.is_file() {
                    return Ok(err_json(
                        400,
                        format!("no rootfs image at {}", src.display()),
                    ));
                }
                if std::fs::create_dir_all(&dir).is_err() {
                    return Ok(err_json(500, "cannot create vessel dir"));
                }
                let dst = dir.join("rootfs.img");
                if let Err(e) = v::clone_file(&src, &dst) {
                    let _ = std::fs::remove_dir_all(&dir);
                    return Ok(err_json(409, format!("{e:#}")));
                }
                v::Rootfs::Prepared
            } else {
                v::Rootfs::BaseImage
            };

            blocking!((|| -> anyhow::Result<serde_json::Value> {
                let volumes = v::parse_volumes(&body.volumes)?;
                let mounts = v::parse_mounts(&body.mounts)?;
                let opts = v::CreateOpts {
                    name: body.name.clone(),
                    cpus: body.cpus,
                    mem: body.mem_mib,
                    gpu: body.gpu,
                    data_gib: body.data_gib,
                    backend: body.backend.clone(),
                    volumes,
                    mounts,
                };
                let created = v::create(&opts, rootfs);
                if created.is_err() {
                    let _ = std::fs::remove_dir_all(&dir);
                }
                created?;
                if body.no_start {
                    return Ok(serde_json::json!({"created": body.name}));
                }
                let started = v::start(&body.name)?;
                Ok(serde_json::json!({"created": body.name, "start": started}))
            })())
        }
        (Method::GET, [name, action]) if action == "console" => {
            let name = name.clone();
            let tail: u64 = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("tail="))
                .and_then(|v| v.parse().ok())
                .unwrap_or(16 * 1024);
            blocking!((|| -> anyhow::Result<serde_json::Value> {
                let dir = v::dir_of(&name)?;
                let log = std::fs::read(dir.join("console.log")).unwrap_or_default();
                let start = log.len().saturating_sub(tail as usize);
                Ok(serde_json::json!({
                    "console": String::from_utf8_lossy(&log[start..]),
                }))
            })())
        }
        (Method::GET, [name]) => {
            let name = name.clone();
            blocking!((|| -> anyhow::Result<serde_json::Value> {
                let all = v::list()?;
                let one = all
                    .into_iter()
                    .find(|s| s.name == name)
                    .ok_or_else(|| anyhow::anyhow!("no vessel named `{name}`"))?;
                Ok(serde_json::json!(one))
            })())
        }
        (Method::DELETE, [name]) => {
            let name = name.clone();
            blocking!(v::rm(&name, force).map(|()| serde_json::json!({"removed": name})))
        }
        (Method::POST, [name, action]) if action == "start" => {
            let name = name.clone();
            blocking!(v::start(&name))
        }
        (Method::POST, [name, action]) if action == "stop" => {
            let name = name.clone();
            blocking!(v::stop(&name))
        }
        (Method::POST, [name, action]) if action == "exec" => {
            #[derive(serde::Deserialize)]
            struct ExecBody {
                cmd: String,
                #[serde(default)]
                args: Vec<String>,
                #[serde(default = "d_timeout")]
                timeout_ms: u64,
            }
            fn d_timeout() -> u64 {
                30_000
            }
            let name = name.clone();
            let Ok(bytes) = read_body(req).await else {
                return Ok(err_json(400, "body too large"));
            };
            let body: ExecBody = match serde_json::from_slice(&bytes) {
                Ok(b) => b,
                Err(e) => return Ok(err_json(400, e)),
            };
            blocking!((|| -> anyhow::Result<serde_json::Value> {
                let dir = v::dir_of(&name)?;
                anyhow::ensure!(
                    v::live_pid(&dir).is_some(),
                    "vessel `{name}` is not running"
                );
                match v::agent_request(
                    &dir,
                    &nebula_core::proto::AgentRequest::Exec {
                        cmd: body.cmd,
                        args: body.args,
                        env: vec![],
                        timeout_ms: body.timeout_ms,
                    },
                )? {
                    nebula_core::proto::AgentResponse::Exec(r) => Ok(serde_json::json!(r)),
                    nebula_core::proto::AgentResponse::Error { message } => {
                        anyhow::bail!("{message}")
                    }
                    other => anyhow::bail!("unexpected: {other:?}"),
                }
            })())
        }
        (Method::GET, [name, action]) if action == "snapshots" => {
            let name = name.clone();
            blocking!(v::snapshots(&name))
        }
        (Method::POST, [name, action]) if action == "snapshots" => {
            #[derive(serde::Deserialize)]
            struct SnapBody {
                label: String,
                /// "auto" (default) | "memory" | "disk"
                #[serde(default)]
                mode: Option<String>,
            }
            let name = name.clone();
            let Ok(bytes) = read_body(req).await else {
                return Ok(err_json(400, "body too large"));
            };
            let body: SnapBody = match serde_json::from_slice(&bytes) {
                Ok(b) => b,
                Err(e) => return Ok(err_json(400, e)),
            };
            let mode = match body.mode.as_deref() {
                None | Some("auto") => v::SnapMode::Auto,
                Some("memory") => v::SnapMode::Memory,
                Some("disk") => v::SnapMode::DiskOnly,
                Some(other) => {
                    return Ok(err_json(
                        400,
                        format!("mode must be auto|memory|disk, got {other}"),
                    ))
                }
            };
            blocking!(v::snapshot(&name, &body.label, mode))
        }
        (Method::DELETE, [name, action, label]) if action == "snapshots" => {
            let (name, label) = (name.clone(), label.clone());
            blocking!(v::snapshot_rm(&name, &label).map(|()| serde_json::json!({"removed": label})))
        }
        (Method::POST, [name, action]) if action == "restore" => {
            #[derive(serde::Deserialize)]
            struct RestoreBody {
                label: String,
            }
            let name = name.clone();
            let Ok(bytes) = read_body(req).await else {
                return Ok(err_json(400, "body too large"));
            };
            let body: RestoreBody = match serde_json::from_slice(&bytes) {
                Ok(b) => b,
                Err(e) => return Ok(err_json(400, e)),
            };
            blocking!(v::restore(&name, &body.label))
        }
        (Method::POST, [name, action]) if action == "branch" => {
            #[derive(serde::Deserialize)]
            struct BranchBody {
                new_name: String,
                #[serde(default)]
                label: Option<String>,
                #[serde(default = "d_count")]
                count: u32,
            }
            fn d_count() -> u32 {
                1
            }
            let name = name.clone();
            let Ok(bytes) = read_body(req).await else {
                return Ok(err_json(400, "body too large"));
            };
            let body: BranchBody = match serde_json::from_slice(&bytes) {
                Ok(b) => b,
                Err(e) => return Ok(err_json(400, e)),
            };
            if body.count > 64 {
                return Ok(err_json(400, "count must be <= 64 per request"));
            }
            blocking!(v::branch(
                &name,
                &body.new_name,
                body.label.as_deref(),
                body.count
            ))
        }
        _ => Ok(err_json(404, "not found")),
    }
}
