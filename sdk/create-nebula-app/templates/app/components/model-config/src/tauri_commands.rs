//! Tauri-command surface — the wiring for the DEFAULT (Tauri) app template,
//! where the UI calls Rust via `invoke()` and there is no HTTP server.
//! (HTTP apps use routes.rs instead; both are thin shells over settings.rs.)
//!
//! Wiring (app template):
//! 1. main.rs / lib.rs: open the DB once and manage it:
//!      let db = model_config::tauri_commands::Db::open(app_data_dir.join("app.db"))?;
//!      .manage(db)
//! 2. add to your invoke_handler:
//!      tauri::generate_handler![…, model_config::tauri_commands::get_settings,
//!                                  model_config::tauri_commands::patch_settings]
//! 3. UI: pass the invoke transport to <ModelConfig/> (see ui/ModelConfig.tsx):
//!      import { invoke } from '@tauri-apps/api/core';
//!      <ModelConfig transport={{
//!        get: () => invoke('get_settings'),
//!        patch: (body) => invoke('patch_settings', { body }),
//!      }} />

use serde_json::{json, Value};
use std::sync::Mutex;

use super::settings::{self, KEYS, PLAIN};

/// App-managed DB handle (tauri `.manage(Db::open(...)?)`).
pub struct Db(pub Mutex<rusqlite::Connection>);

impl Db {
    pub fn open(path: std::path::PathBuf) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = rusqlite::Connection::open(path)?;
        settings::migrate(&conn)?;
        Ok(Db(Mutex::new(conn)))
    }
}

/// Same payload shape as the HTTP GET (camelCase plain settings +
/// connections with set/hint/unlocks — never the secret value).
#[tauri::command]
pub fn get_settings(db: tauri::State<'_, Db>) -> Value {
    let conn = db.0.lock().unwrap();
    let connections: Vec<Value> = KEYS
        .iter()
        .map(|(key, unlocks)| {
            let v = settings::get(&conn, key);
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
        out.insert(camel(key), json!(settings::get(&conn, key)));
    }
    out.insert("connections".into(), json!(connections));
    Value::Object(out)
}

/// Same contract as the HTTP PATCH: known keys only, unknowns ignored.
#[tauri::command]
pub fn patch_settings(db: tauri::State<'_, Db>, body: Value) -> Value {
    let conn = db.0.lock().unwrap();
    if let Some(obj) = body.as_object() {
        for (k, v) in obj {
            let known = KEYS.iter().any(|(key, _)| key == k) || PLAIN.contains(&k.as_str());
            if !known {
                continue;
            }
            if let Some(s) = v.as_str() {
                settings::set(&conn, k, s);
            } else if let Some(n) = v.as_i64() {
                settings::set(&conn, k, &n.to_string());
            }
        }
    }
    json!({ "success": true })
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
