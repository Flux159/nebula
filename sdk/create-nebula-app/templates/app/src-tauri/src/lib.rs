//! {{APP_NAME}} — Tauri shell over the app's own hyper API server.
//!
//! Architecture: React (webview or any browser) -> the app server
//! (src/server.rs, hyper + rusqlite) -> the Nebula engine (src/nebula.rs).
//! Everything is plain HTTP — no invoke() coupling — so the same backend
//! serves the Tauri window, `npm run web:dev` in a browser, scripts and
//! tests. Components extend server routes + db tables.

mod db;
mod nebula;
mod server;

use std::sync::Arc;

pub fn run() {
    // nebula.config.json is the single source of truth (engine.mjs, the
    // frontend, and this binary all read the same file — compiled in here).
    let cfg: serde_json::Value =
        serde_json::from_str(include_str!("../../nebula.config.json")).expect("nebula.config.json");
    let api_port = cfg["apiPort"].as_u64().unwrap_or(7461) as u16;
    let app_port = cfg["appPort"].as_u64().unwrap_or(7470) as u16;

    // App data (sqlite) lives beside the project in data/, NOT under the
    // disposable .nebula/ engine home. Packaged apps should move this to
    // the OS app-data dir (tauri's path resolver).
    let data_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("data"))
        .expect("data dir");
    let db = db::Db::open(&data_dir).expect("open app.db");

    server::start(
        Arc::new(server::Ctx {
            nebula: nebula::Nebula::new(api_port),
            db,
        }),
        app_port,
    );

    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
