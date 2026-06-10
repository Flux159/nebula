use std::process::Command;

/// Start the Nebula engine by invoking the CLI (`nebula up`), searched on
/// PATH and in common install/dev locations. Returns its combined output.
#[tauri::command]
fn start_engine() -> Result<String, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        "nebula".to_string(),
        "/opt/homebrew/bin/nebula".to_string(),
        format!("{home}/Projects/nebula/target/release/nebula"),
        format!("{home}/Projects/nebula/target/debug/nebula"),
    ];
    for bin in &candidates {
        match Command::new(bin).arg("up").output() {
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
            Err(_) => continue, // binary not at this path; try the next
        }
    }
    Err("nebula CLI not found (install it or add it to PATH)".into())
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
