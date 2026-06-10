//! `nebula autostart enable|disable|status` — run the engine at login via a
//! launchd LaunchAgent. KeepAlive also restarts nebulad if it ever dies
//! (pairs with the daemon's exit-on-vessel-death watchdog).
//!
//! `nebula ui` — launch the desktop app (bundled .app when installed, dev
//! binary from the repo otherwise).

use std::path::PathBuf;

use anyhow::{bail, Context};

const LABEL: &str = "dev.nebula.nebulad";

fn plist_path() -> anyhow::Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").context("HOME not set")?)
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
    )
}

fn nebulad_path() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let candidate = exe.parent().context("exe has no parent")?.join("nebulad");
    anyhow::ensure!(
        candidate.is_file(),
        "nebulad not found next to nebula ({})",
        candidate.display()
    );
    Ok(candidate)
}

pub fn enable() -> anyhow::Result<()> {
    let nebulad = nebulad_path()?;
    let logs = crate::client::nebula_home()?.join("logs");
    std::fs::create_dir_all(&logs)?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>{nebulad}</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict><key>SuccessfulExit</key><false/></dict>
    <key>StandardOutPath</key><string>{logs}/launchd.out.log</string>
    <key>StandardErrorPath</key><string>{logs}/launchd.err.log</string>
</dict>
</plist>
"#,
        nebulad = nebulad.display(),
        logs = logs.display(),
    );
    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, plist)?;

    // bootout first so re-enabling picks up a changed binary path.
    let uid = unsafe { libc::getuid() };
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .output();
    let status = std::process::Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}")])
        .arg(&path)
        .status()?;
    anyhow::ensure!(status.success(), "launchctl bootstrap failed");
    println!("autostart enabled — nebulad starts at login and restarts on failure");
    println!("  agent: {}", path.display());
    Ok(())
}

pub fn disable() -> anyhow::Result<()> {
    let uid = unsafe { libc::getuid() };
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .output();
    let path = plist_path()?;
    if path.is_file() {
        std::fs::remove_file(&path)?;
    }
    println!("autostart disabled (the running engine, if any, is untouched)");
    Ok(())
}

pub fn status() -> anyhow::Result<()> {
    let installed = plist_path()?.is_file();
    let uid = unsafe { libc::getuid() };
    let loaded = std::process::Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{LABEL}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!(
        "autostart: {}",
        match (installed, loaded) {
            (true, true) => "enabled (loaded)",
            (true, false) => "installed but not loaded (run `nebula autostart enable`)",
            (false, _) => "disabled",
        }
    );
    println!(
        "engine:    {}",
        if crate::client::daemon_running() {
            "running"
        } else {
            "stopped"
        }
    );
    Ok(())
}

/// Launch the desktop app: installed bundle first, then the dev build.
pub fn open_ui() -> anyhow::Result<()> {
    // Installed .app (open -a handles Spotlight lookup by bundle name).
    let try_open = std::process::Command::new("open")
        .args(["-a", "nebula-ui"])
        .output()?;
    if try_open.status.success() {
        println!("opened Nebula app");
        return Ok(());
    }
    // Repo-built bundle, then the bare dev binary.
    let exe = std::env::current_exe()?;
    for dir in exe.ancestors() {
        if dir.join("Cargo.toml").is_file() && dir.join("ui/src-tauri").is_dir() {
            let bundle = dir.join("ui/src-tauri/target/release/bundle/macos/nebula-ui.app");
            if bundle.is_dir() {
                let status = std::process::Command::new("open").arg(&bundle).status()?;
                if status.success() {
                    println!("opened Nebula app ({})", bundle.display());
                    return Ok(());
                }
            }
            for profile in ["release", "debug"] {
                let bin = dir.join(format!("ui/src-tauri/target/{profile}/nebula-ui"));
                if bin.is_file() {
                    let child = std::process::Command::new(&bin)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()?;
                    std::mem::forget(child);
                    println!("opened Nebula app (dev build: {})", bin.display());
                    return Ok(());
                }
            }
        }
    }
    bail!("Nebula app not found — install the .app, or build it: cd ui/src-tauri && cargo build");
}
