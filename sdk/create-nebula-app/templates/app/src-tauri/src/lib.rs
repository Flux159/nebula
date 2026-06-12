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
use tauri::Manager;

pub fn run() {
    // nebula.config.json is the single source of truth (engine.mjs, the
    // frontend, and this binary all read the same file — compiled in here).
    let cfg: serde_json::Value =
        serde_json::from_str(include_str!("../../nebula.config.json")).expect("nebula.config.json");
    let api_port = cfg["apiPort"].as_u64().unwrap_or(7461) as u16;
    let app_port = cfg["appPort"].as_u64().unwrap_or(7470) as u16;

    tauri::Builder::default()
        .setup(move |app| {
            // App data (sqlite) lives in the OS-standard application data
            // dir, keyed by the bundle identifier in tauri.conf.json:
            //   macOS    ~/Library/Application Support/<identifier>/
            //   Linux    $XDG_DATA_HOME/<identifier>/ (~/.local/share/…)
            //   Windows  %APPDATA%\<identifier>\
            // NOT under .nebula/ — app data survives engine factory-resets.
            // NEBULA_APP_DATA overrides (tests, portable installs).
            let data_dir = match std::env::var_os("NEBULA_APP_DATA") {
                Some(d) => std::path::PathBuf::from(d),
                None => app.path().app_data_dir()?,
            };
            println!("app data: {}", data_dir.display());
            let db = db::Db::open(&data_dir)?;

            server::start(
                Arc::new(server::Ctx {
                    nebula: nebula::Nebula::new(api_port),
                    db,
                }),
                app_port,
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
