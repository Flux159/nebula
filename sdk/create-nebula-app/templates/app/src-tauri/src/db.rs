//! App persistence: SQLite via rusqlite (bundled — no system dependency).
//!
//! The starter schema is a settings key-value table; add your domain tables
//! in `migrate`. Components (model-config stores API keys/model endpoints
//! here) follow the same pattern. The db lives at app.db in the OS-standard
//! application-data dir (see lib.rs) — NOT under .nebula/, which is the
//! disposable engine home.

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

pub struct Db(Mutex<Connection>);

impl Db {
    pub fn open(dir: &Path) -> rusqlite::Result<Self> {
        std::fs::create_dir_all(dir).ok();
        let conn = Connection::open(dir.join("app.db"))?;
        migrate(&conn)?;
        Ok(Db(Mutex::new(conn)))
    }

    pub fn get_setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        // ~/.{{APP_NAME}}/config wins over anything saved via the API/UI —
        // the CLI-first path (headless servers, dotfiles, CI).
        if let Some(v) = config_overlay().get(key) {
            if !v.is_empty() {
                return Ok(Some(v.clone()));
            }
        }
        let conn = self.0.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
}

/// `~/.{{APP_NAME}}/config` — optional KEY=VALUE file (env-file style,
/// `#` comments, optional quotes). Values OVERRIDE API/UI-saved settings.
/// NEBULA_APP_CONFIG relocates it (tests, portable installs).
pub fn config_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("NEBULA_APP_CONFIG") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".{{APP_NAME}}/config")
}

/// Parsed fresh each call — the file is tiny and local, and freshness means
/// edits apply without restarting the app.
pub fn config_overlay() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(raw) = std::fs::read_to_string(config_path()) else { return map };
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            map.insert(k.trim().to_string(), v.to_string());
        }
    }
    map
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
         );",
    )
}
