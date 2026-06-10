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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![start_engine])
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
