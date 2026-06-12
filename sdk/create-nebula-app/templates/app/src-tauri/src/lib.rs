//! {{APP_NAME}} — Tauri shell over the Nebula engine.
//!
//! The webview reads the engine API directly (loopback, CORS-open);
//! anything that deserves to live in Rust — secrets, model connectors,
//! multi-step engine flows — goes through [`nebula::Nebula`] and a Tauri
//! command, like `fork_demo` below. Components extend exactly this seam.

mod nebula;

use nebula::Nebula;
use serde_json::json;

struct AppState {
    nebula: Nebula,
}

/// The headline primitive end-to-end in the Rust layer: create a microVM,
/// write into its RAM, snapshot it live, fork it, prove the fork remembers.
#[tauri::command]
async fn fork_demo(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let n = &state.nebula;
    let backend = if cfg!(target_os = "macos") { "vz" } else { "krun" };
    let mut out = String::new();

    let created: serde_json::Value = n
        .post(
            "/v1alpha1/vessels",
            json!({"name": "demo", "backend": backend, "mem_mib": 1024}),
        )
        .await?;
    out.push_str(&format!(
        "created `demo` ({}ms boot)\n",
        created["start"]["boot_ms"]
    ));

    n.post::<serde_json::Value>(
        "/v1alpha1/vessels/demo/exec",
        json!({"cmd": "sh", "args": ["-c", "echo hello-from-the-past > /run/state"]}),
    )
    .await?;

    let snap: serde_json::Value = n
        .post("/v1alpha1/vessels/demo/snapshots", json!({"label": "t0"}))
        .await?;
    out.push_str(&format!(
        "live snapshot in {}ms ({} MiB)\n",
        snap["ms"], snap["state_mb"]
    ));

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
            .request(
                "DELETE",
                &format!("/v1alpha1/vessels/{name}?force=true"),
                None,
            )
            .await?;
    }
    Ok(out)
}

pub fn run() {
    // apiPort from nebula.config.json (compiled in — the file is the single
    // source of truth shared with scripts/engine.mjs and the frontend).
    let cfg: serde_json::Value =
        serde_json::from_str(include_str!("../../nebula.config.json")).expect("nebula.config.json");
    let api_port = cfg["apiPort"].as_u64().unwrap_or(7461) as u16;

    tauri::Builder::default()
        .manage(AppState {
            nebula: Nebula::new(api_port),
        })
        .invoke_handler(tauri::generate_handler![fork_demo])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
