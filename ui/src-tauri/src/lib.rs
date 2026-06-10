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

/// `kubectl apply -f -` with the textarea's YAML on stdin: the UI equivalent
/// of pasting a manifest in a terminal.
#[tauri::command]
fn kube_apply(yaml: String) -> Result<String, String> {
    if yaml.trim().is_empty() {
        return Err("empty manifest".into());
    }
    use std::io::Write;
    for bin in nebula_cli() {
        let child = Command::new(&bin)
            .args(["kubectl", "apply", "-f", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let Ok(mut child) = child else { continue };
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(yaml.as_bytes());
        }
        drop(child.stdin.take());
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return if out.status.success() {
            Ok(text)
        } else {
            Err(text)
        };
    }
    Err("nebula CLI not found".into())
}

/// Run one `docker …` command from the UI textarea, exactly as the terminal
/// would (through the sidecar wrapper, so the user's contexts are untouched).
#[tauri::command]
fn docker_command(command: String) -> Result<String, String> {
    let rest = command
        .trim()
        .strip_prefix("docker ")
        .ok_or("command must start with `docker `")?
        .trim()
        .to_string();
    if rest.is_empty() {
        return Err("empty docker command".into());
    }
    for bin in nebula_cli() {
        if !std::path::Path::new(&bin).is_file() && bin.contains('/') {
            continue;
        }
        // Through a shell so quotes/env vars behave exactly like the terminal
        // (this runs with the user's own privileges — same as the CLI).
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("exec '{bin}' docker {rest}"))
            .output();
        let Ok(out) = out else { continue };
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return if out.status.success() {
            Ok(text)
        } else {
            Err(text)
        };
    }
    Err("nebula CLI not found".into())
}

// --- Apps: curated single-container catalog --------------------------------

const CATALOG_URL: &str =
    "https://raw.githubusercontent.com/Flux159/nebula/main/apps/catalog.json";

/// Fetch the app catalog: GitHub first (instant updates without app
/// releases), bundled copy as the offline/private-repo fallback.
#[tauri::command]
fn apps_catalog(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let fetched = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "5", CATALOG_URL])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok());
    if let Some(v) = fetched {
        return Ok(v);
    }
    let res = app.path().resource_dir().map_err(|e| e.to_string())?;
    for p in [
        res.join("resources/apps-catalog.json"),
        // dev tree fallback
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/catalog.json"),
    ] {
        if let Ok(raw) = std::fs::read(&p) {
            return serde_json::from_slice(&raw).map_err(|e| e.to_string());
        }
    }
    Err("app catalog unavailable (offline and no bundled copy)".into())
}

/// Container/volume naming: everything an app owns is prefixed and labeled,
/// so status/uninstall can't touch anything else.
fn app_container(id: &str) -> Result<String, String> {
    if id.is_empty()
        || id.len() > 40
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("invalid app id".into());
    }
    Ok(format!("nebula-app-{id}"))
}

/// Install an app: one labeled container with restart policy + named volumes.
#[tauri::command]
fn app_install(spec: serde_json::Value) -> Result<String, String> {
    let id = spec["id"].as_str().ok_or("missing id")?;
    let image = spec["image"].as_str().ok_or("missing image")?;
    if !image
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/:.-_@".contains(c))
    {
        return Err("invalid image ref".into());
    }
    let name = app_container(id)?;
    let mut args: Vec<String> = vec![
        "docker".into(),
        "run".into(),
        "-d".into(),
        "--name".into(),
        name.clone(),
        "--label".into(),
        format!("nebula.app={id}"),
        "--restart".into(),
        "unless-stopped".into(),
    ];
    for p in spec["ports"].as_array().unwrap_or(&vec![]) {
        let (h, c) = (
            p["host"].as_u64().ok_or("bad port")?,
            p["container"].as_u64().ok_or("bad port")?,
        );
        args.push("-p".into());
        args.push(format!("{h}:{c}"));
    }
    for v in spec["volumes"].as_array().unwrap_or(&vec![]) {
        let vol = v["name"].as_str().ok_or("bad volume")?;
        let path = v["container"].as_str().ok_or("bad volume")?;
        if !vol.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || !path.starts_with('/') {
            return Err("invalid volume spec".into());
        }
        args.push("-v".into());
        args.push(format!("{name}-{vol}:{path}"));
    }
    if let Some(env) = spec["env"].as_object() {
        for (k, v) in env {
            let val = v.as_str().unwrap_or_default();
            if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err("invalid env key".into());
            }
            args.push("-e".into());
            args.push(format!("{k}={val}"));
        }
    }
    args.push(image.into());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run_cli(&argv)
}

/// start | stop | uninstall (containers only — named volumes survive an
/// uninstall so user data outlives the app, Synology-style).
#[tauri::command]
fn app_ctl(id: String, action: String) -> Result<String, String> {
    let name = app_container(&id)?;
    match action.as_str() {
        "start" => run_cli(&["docker", "start", &name]),
        "stop" => run_cli(&["docker", "stop", &name]),
        "uninstall" => run_cli(&["docker", "rm", "-f", &name]),
        _ => Err("action must be start|stop|uninstall".into()),
    }
}

/// State of every installed app: id -> {state, status}.
#[tauri::command]
fn apps_status() -> Result<serde_json::Value, String> {
    let out = run_cli(&[
        "docker",
        "ps",
        "-a",
        "--filter",
        "label=nebula.app",
        "--format",
        "{{.Label \"nebula.app\"}}\t{{.State}}\t{{.Status}}",
    ])?;
    let mut map = serde_json::Map::new();
    for line in out.lines() {
        let mut parts = line.splitn(3, '\t');
        if let (Some(id), Some(state), Some(status)) = (parts.next(), parts.next(), parts.next()) {
            map.insert(
                id.to_string(),
                serde_json::json!({ "state": state, "status": status }),
            );
        }
    }
    Ok(serde_json::Value::Object(map))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            start_engine,
            cli_tools_status,
            setup_cli_tools,
            container_logs,
            kube_get,
            kube_apply,
            docker_command,
            apps_catalog,
            app_install,
            app_ctl,
            apps_status
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
