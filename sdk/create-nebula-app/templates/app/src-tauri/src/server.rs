//! The app's own API: a hyper server on 127.0.0.1:<appPort>.
//!
//! The React frontend (and anything else — a browser tab, a script, a test)
//! talks plain HTTP to this; this layer talks to the Nebula engine
//! (src/nebula.rs) and the app db (src/db.rs). Components add routes here.
//! Same server pattern as nebulad's own API (hyper 1.x, current-thread
//! tokio runtime on a dedicated thread).
//!
//! Starter routes:
//!   GET  /api/health                      liveness
//!   GET  /api/settings/<key>              sqlite-backed app settings
//!   PUT  /api/settings/<key>  {"value"}   (model-config stores keys here)
//!   POST /api/fork-demo                   the headline primitive, end-to-end

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::{header, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;

use crate::db::Db;
use crate::nebula::Nebula;

pub struct Ctx {
    pub nebula: Nebula,
    pub db: Db,
}

pub fn start(ctx: Arc<Ctx>, port: u16) {
    std::thread::Builder::new()
        .name("app-api".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("app api runtime");
            rt.block_on(serve(ctx, port));
        })
        .ok();
}

async fn serve(ctx: Arc<Ctx>, port: u16) {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("app api bind 127.0.0.1:{port}: {e}");
            return;
        }
    };
    println!("app api on http://127.0.0.1:{port}/api");
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req| route(ctx.clone(), req));
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}

type Resp = Response<BoxBody<Bytes, hyper::Error>>;

fn json_resp(status: u16, value: &serde_json::Value) -> Resp {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, "application/json")
        // The vite dev server (5173) and the packaged webview both pass.
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(
            Full::new(Bytes::from(value.to_string()))
                .map_err(|n| match n {})
                .boxed(),
        )
        .unwrap()
}

fn err_resp(status: u16, msg: impl std::fmt::Display) -> Resp {
    json_resp(status, &json!({ "error": msg.to_string() }))
}

async fn route(ctx: Arc<Ctx>, req: Request<Incoming>) -> Result<Resp, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::OPTIONS {
        return Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, PUT, DELETE, OPTIONS")
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type")
            .body(Full::new(Bytes::new()).map_err(|n| match n {}).boxed())
            .unwrap());
    }

    match (method, path.as_str()) {
        (Method::GET, "/api/health") => Ok(json_resp(200, &json!({"ok": true}))),

        (Method::GET, p) if p.starts_with("/api/settings/") => {
            let key = p["/api/settings/".len()..].to_string();
            match ctx.db.get_setting(&key) {
                Ok(Some(value)) => Ok(json_resp(200, &json!({"key": key, "value": value}))),
                Ok(None) => Ok(err_resp(404, format!("no setting `{key}`"))),
                Err(e) => Ok(err_resp(500, e)),
            }
        }
        (Method::PUT, p) if p.starts_with("/api/settings/") => {
            let key = p["/api/settings/".len()..].to_string();
            let Ok(bytes) = Limited::new(req.into_body(), 64 * 1024).collect().await else {
                return Ok(err_resp(400, "body too large"));
            };
            let body: serde_json::Value = match serde_json::from_slice(&bytes.to_bytes()) {
                Ok(v) => v,
                Err(e) => return Ok(err_resp(400, e)),
            };
            let Some(value) = body.get("value").and_then(|v| v.as_str()) else {
                return Ok(err_resp(400, "want {\"value\": \"...\"}"));
            };
            match ctx.db.set_setting(&key, value) {
                Ok(()) => Ok(json_resp(200, &json!({"key": key, "value": value}))),
                Err(e) => Ok(err_resp(500, e)),
            }
        }

        (Method::POST, "/api/fork-demo") => match fork_demo(&ctx.nebula).await {
            Ok(out) => Ok(json_resp(200, &json!({"output": out}))),
            Err(e) => Ok(err_resp(502, e)),
        },

        _ => Ok(err_resp(404, "not found")),
    }
}

/// The headline primitive end-to-end: create a microVM, write into its RAM,
/// snapshot it live, fork it, prove the fork remembers.
async fn fork_demo(n: &Nebula) -> Result<String, String> {
    let backend = if cfg!(target_os = "macos") { "vz" } else { "krun" };
    let mut out = String::new();

    let created: serde_json::Value = n
        .post(
            "/v1alpha1/vessels",
            json!({"name": "demo", "backend": backend, "mem_mib": 1024}),
        )
        .await?;
    out.push_str(&format!("created `demo` ({}ms boot)\n", created["start"]["boot_ms"]));

    n.post::<serde_json::Value>(
        "/v1alpha1/vessels/demo/exec",
        json!({"cmd": "sh", "args": ["-c", "echo hello-from-the-past > /run/state"]}),
    )
    .await?;

    let snap: serde_json::Value = n
        .post("/v1alpha1/vessels/demo/snapshots", json!({"label": "t0"}))
        .await?;
    out.push_str(&format!("live snapshot in {}ms ({} MiB)\n", snap["ms"], snap["state_mb"]));

    let fork: serde_json::Value = n
        .post(
            "/v1alpha1/vessels/demo/branch",
            json!({"new_name": "fork", "label": "t0", "count": 2}),
        )
        .await?;
    out.push_str(&format!("forked 2 live clones in {}ms\n", fork["ms"]));

    let mem: serde_json::Value = n
        .post(
            "/v1alpha1/vessels/fork-1/exec",
            json!({"cmd": "cat", "args": ["/run/state"]}),
        )
        .await?;
    out.push_str(&format!(
        "fork-1 remembers: {}",
        mem["stdout"].as_str().unwrap_or("").trim()
    ));

    for name in ["demo", "fork-1", "fork-2"] {
        let _: serde_json::Value = n
            .request("DELETE", &format!("/v1alpha1/vessels/{name}?force=true"), None)
            .await?;
    }
    Ok(out)
}
