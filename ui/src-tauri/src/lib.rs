use std::path::PathBuf;
use std::process::Command;

use tauri::Manager;

/// Bundled sidecar CLI (Contents/MacOS/nebula), falling back to PATH and
/// well-known dev locations when running unbundled.
fn nebula_cli() -> Vec<String> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("nebula").display().to_string());
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    candidates.extend([
        "nebula".to_string(),
        "/opt/homebrew/bin/nebula".to_string(),
        format!("{home}/Projects/nebula/target/release/nebula"),
        format!("{home}/Projects/nebula/target/debug/nebula"),
    ]);
    candidates
}

fn run_cli(args: &[&str]) -> Result<String, String> {
    for bin in nebula_cli() {
        match Command::new(&bin).args(args).output() {
            Ok(out) if out.status.success() => {
                return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            Ok(out) => {
                return Err(format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            Err(_) => continue, // not at this path; try the next
        }
    }
    Err("nebula CLI not found (bundle is missing its sidecar and PATH has none)".into())
}

fn images_installed() -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(&home).join(".nebula/kernel/Image").is_file()
        && PathBuf::from(&home)
            .join(".nebula/disks/rootfs.img")
            .is_file()
}

/// Start the engine. First run: install the guest images bundled in the app
/// (offline) before bringing it up; without bundled images the CLI falls back
/// to downloading the released artifacts.
#[tauri::command]
fn start_engine(app: tauri::AppHandle) -> Result<String, String> {
    if !images_installed() {
        if let Ok(res) = app.path().resource_dir() {
            let kernel = res.join("resources/kernel-Image.gz");
            let rootfs = res.join("resources/rootfs.img.gz");
            if kernel.is_file() && rootfs.is_file() {
                run_cli(&[
                    "install-image",
                    "--kernel",
                    &kernel.display().to_string(),
                    "--rootfs",
                    &rootfs.display().to_string(),
                ])?;
            }
        }
    }
    run_cli(&["up"])
}

/// Which of docker/kubectl/helm are missing from PATH, and whether the
/// bundled copies + profile line are set up.
#[tauri::command]
fn cli_tools_status() -> Result<serde_json::Value, String> {
    let which = |tool: &str| {
        Command::new("/usr/bin/which")
            .arg(tool)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    let missing: Vec<&str> = ["docker", "kubectl", "helm"]
        .into_iter()
        .filter(|t| !which(t))
        .collect();
    let home = std::env::var("HOME").unwrap_or_default();
    let linked = PathBuf::from(&home).join(".nebula/bin/docker").exists();
    Ok(serde_json::json!({
        "missing": missing,
        "linked": linked,
    }))
}

/// Run `nebula setup path --yes` via the sidecar CLI.
#[tauri::command]
fn setup_cli_tools() -> Result<String, String> {
    run_cli(&["setup", "path", "--yes"])
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Tail a container's logs through the sidecar (`nebula docker logs`).
/// docker writes the container's stderr stream to stderr, so both streams
/// are returned regardless of exit status.
#[tauri::command]
fn container_logs(id: String) -> Result<String, String> {
    if !valid_id(&id) {
        return Err("invalid container id".into());
    }
    for bin in nebula_cli() {
        match Command::new(&bin)
            .args(["docker", "logs", "--tail", "400", "--timestamps", &id])
            .output()
        {
            Ok(out) => {
                let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
                s.push_str(&String::from_utf8_lossy(&out.stderr));
                return Ok(s);
            }
            Err(_) => continue,
        }
    }
    Err("nebula CLI not found".into())
}

/// One kubectl read through the sidecar (`nebula kubectl get …`), allow-listed
/// so the UI can't be talked into arbitrary cluster mutations.
#[tauri::command]
fn kube_get(kind: String) -> Result<String, String> {
    const ALLOWED: &[&str] = &["pods", "deployments", "services", "nodes", "namespaces"];
    if !ALLOWED.contains(&kind.as_str()) {
        return Err(format!("kind must be one of {ALLOWED:?}"));
    }
    run_cli(&["kubectl", "get", &kind, "-A", "-o", "wide"])
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            start_engine,
            cli_tools_status,
            setup_cli_tools,
            container_logs,
            kube_get
        ])
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
