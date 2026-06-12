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

    // --headless: same artifact, no window — the API server in the
    // foreground (CLI/daemon mode; pair with ~/.{{APP_NAME}}/config for
    // UI-less configuration). Default stays the Tauri window:
    //   <bundle>/Contents/MacOS/{{APP_NAME}} --headless [--port N]
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--headless") {
        let port = args
            .iter()
            .position(|a| a == "--port")
            .and_then(|i| args.get(i + 1))
            .and_then(|p| p.parse().ok())
            .unwrap_or(app_port);
        let data_dir = headless_data_dir();
        println!("app data: {}", data_dir.display());
        let db = db::Db::open(&data_dir).expect("open app db");
        server::start(
            Arc::new(server::Ctx {
                nebula: nebula::Nebula::new(api_port),
                db,
            }),
            port,
        );
        // server::start runs on its own thread; keep the process alive.
        loop {
            std::thread::park();
        }
    }

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

/// The same OS-standard app-data dir tauri's app_data_dir() resolves,
/// computed without a tauri handle (keyed by the bundle identifier).
/// NEBULA_APP_DATA overrides, as in the windowed path.
fn headless_data_dir() -> std::path::PathBuf {
    if let Some(d) = std::env::var_os("NEBULA_APP_DATA") {
        return std::path::PathBuf::from(d);
    }
    const IDENTIFIER: &str = "local.{{APP_NAME}}.app";
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    #[cfg(target_os = "macos")]
    return home.join("Library/Application Support").join(IDENTIFIER);
    #[cfg(target_os = "windows")]
    return std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or(home)
        .join(IDENTIFIER);
    #[cfg(all(unix, not(target_os = "macos")))]
    return std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join(IDENTIFIER);
}
