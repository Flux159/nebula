//! Settings routes — hyper 1.x (the component standard for Rust HTTP).
//!
//! Mount by calling `handle` early in your service fn; it returns
//! `Some(response)` when it owned the request, `None` to fall through:
//!
//! ```ignore
//! // inside your `service_fn(|req| async move { ... })`, after buffering
//! // the body (these are small JSON requests):
//! let (parts, body) = req.into_parts();
//! let bytes = body.collect().await?.to_bytes();
//! if let Some(resp) = model_config::routes::handle(&parts, &bytes, &db) {
//!     return Ok(resp);
//! }
//! ```
//!
//! Axum hosts: each fn is plain (method, path, json) → json — wrap in a
//! route handler directly.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};
use serde_json::{json, Value};

use super::settings::{self, KEYS, PLAIN};

type Resp = Response<Full<Bytes>>;

fn json_response(status: StatusCode, body: &Value) -> Resp {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

/// Route the request if it's ours. `db` is your app's rusqlite handle
/// (wrap in your own mutex/pool as you do elsewhere).
pub fn handle(
    parts: &hyper::http::request::Parts,
    body: &[u8],
    conn: &rusqlite::Connection,
) -> Option<Resp> {
    match (parts.method.as_str(), parts.uri.path()) {
        ("GET", "/api/settings") => Some(get_settings(conn)),
        ("PATCH", "/api/settings") => Some(patch_settings(conn, body)),
        _ => None,
    }
}

/// GET — connections (set/hint/unlocks, never the secret) + plain settings
/// (snake_case keys → camelCase fields).
pub fn get_settings(conn: &rusqlite::Connection) -> Resp {
    let connections: Vec<Value> = KEYS
        .iter()
        .map(|(key, unlocks)| {
            let v = settings::get(conn, key);
            let set = v.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
            json!({
                "key": key,
                "set": set,
                "unlocks": unlocks,
                "hint": v.filter(|s| s.len() > 4).map(|s| format!("…{}", &s[s.len() - 4..])),
            })
        })
        .collect();
    let mut out = serde_json::Map::new();
    for key in PLAIN {
        out.insert(camel(key), json!(settings::get(conn, key)));
    }
    out.insert("connections".into(), json!(connections));
    json_response(StatusCode::OK, &Value::Object(out))
}

/// PATCH — accepts any known KEYS / PLAIN entries (string or number values).
/// Unknown keys are ignored, not errors (forward compatibility).
pub fn patch_settings(conn: &rusqlite::Connection, body: &[u8]) -> Resp {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &json!({ "error": format!("bad json: {e}") })),
    };
    if let Some(obj) = parsed.as_object() {
        for (k, v) in obj {
            let known = KEYS.iter().any(|(key, _)| key == k) || PLAIN.contains(&k.as_str());
            if !known {
                continue;
            }
            if let Some(s) = v.as_str() {
                settings::set(conn, k, s);
            } else if let Some(n) = v.as_i64() {
                settings::set(conn, k, &n.to_string());
            }
        }
    }
    // Optional pattern (Galaxy does this): after saving a provider key,
    // push it to already-running workloads over their own HTTP API so
    // users don't have to respawn. See COMPONENT.md.
    json_response(StatusCode::OK, &json!({ "success": true }))
}

fn camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut up = false;
    for c in s.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}
