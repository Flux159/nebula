//! Settings store — SQLite-backed key/value with a secrets convention.
//!
//! EDIT THE TWO TABLES BELOW for your app. Everything else is generic.
//! Secrets are stored locally and never echoed back through the API
//! (GET returns set/hint only). Move to the OS keychain at packaging time.

use rusqlite::Connection;

/// Secret keys: (env-var-style name, what it unlocks — shown in the UI).
pub const KEYS: &[(&str, &str)] = &[
    ("OPENROUTER_API_KEY", "Cloud models (default provider)"),
    ("ANTHROPIC_API_KEY", "Anthropic direct"),
    ("OPENAI_API_KEY", "OpenAI direct"),
    ("GOOGLE_API_KEY", "Gemini models"),
];

/// Non-secret settings (returned verbatim).
pub const PLAIN: &[&str] = &[
    // Model routing: "openrouter" (cloud, default) or "local" — any
    // OpenAI-compatible server (llama.cpp / LM Studio / mock-model).
    "model_provider",
    "local_model_base_url", // server ROOT — clients append /v1/chat/completions
    "local_model_name",
];

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
           key TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );",
    )
}

pub fn get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
        .ok()
}

pub fn set(conn: &Connection, key: &str, value: &str) {
    let _ = conn.execute(
        "INSERT INTO settings(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    );
}

/// The resolved model endpoint for launching model work.
pub enum ModelInvocation {
    /// Inject these env vars (cloud providers; the client picks its key).
    Cloud { env: Vec<(String, String)> },
    /// Point the client at an OpenAI-compatible server.
    /// (luminal: `--base-url <base_url> --api openai --auth none --model <model>`)
    Local { base_url: String, model: String },
}

pub fn model_invocation(conn: &Connection) -> ModelInvocation {
    if get(conn, "model_provider").as_deref() == Some("local") {
        if let Some(base_url) = get(conn, "local_model_base_url").filter(|s| !s.is_empty()) {
            return ModelInvocation::Local {
                base_url,
                model: get(conn, "local_model_name").unwrap_or_else(|| "local".into()),
            };
        }
    }
    let env = KEYS
        .iter()
        .filter_map(|(k, _)| get(conn, k).filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
        .collect();
    ModelInvocation::Cloud { env }
}
