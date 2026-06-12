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

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
         );",
    )
}
