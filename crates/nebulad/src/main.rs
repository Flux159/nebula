//! nebulad: owns the Vessel VM and serves the control socket.
//!
//! v0 lifecycle: start -> boot Vessel -> serve until `down` -> stop VM -> exit.
//! Crash recovery / auto-restart hardening lands in Phase 6.

mod api;
mod balloon;
mod config;
mod net;
mod paths;
mod server;
mod vessel;

use clap::Parser;

#[derive(Parser)]
#[command(name = "nebulad", version, about = "Nebula host daemon")]
struct Args {
    /// Stay in the foreground (default; flag kept for launchd compatibility).
    #[arg(long)]
    foreground: bool,
}

fn main() -> anyhow::Result<()> {
    // The krun backend re-execs the CURRENT binary as its VM worker
    // (krun_start_enter takes over the process) — when the engine runs on
    // krun (Linux/Windows), that binary is nebulad, so route the arg before clap.
    {
        let mut args = std::env::args().skip(1);
        let first = args.next();
        if first.as_deref() == Some("krun-worker") {
            let spec = match (args.next().as_deref(), args.next()) {
                (Some("--spec"), Some(json)) => json,
                (Some("--spec-file"), Some(path)) => match std::fs::read_to_string(&path) {
                    Ok(json) => json,
                    Err(e) => {
                        eprintln!("krun-worker: read {path}: {e}");
                        std::process::exit(2);
                    }
                },
                _ => {
                    eprintln!(
                        "krun-worker: usage: nebulad krun-worker --spec <json>|--spec-file <path>"
                    );
                    std::process::exit(2);
                }
            };
            eprintln!("krun-worker: starting (pid {})", std::process::id());
            match nebula_core::backend::krun::run_worker(&spec) {
                Ok(never) => match never {},
                Err(e) => {
                    eprintln!("krun-worker: {e}");
                    std::process::exit(1);
                }
            }
        }
        // vz vessels started through the REST API spawn the current exe
        // (nebulad) as their worker, mirroring the CLI's vz-worker.
        #[cfg(target_os = "macos")]
        if first.as_deref() == Some("vz-worker") {
            let mut spec = None;
            let mut restore: Option<std::path::PathBuf> = None;
            let mut control: Option<std::path::PathBuf> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--spec" => spec = args.next(),
                    "--restore" => restore = args.next().map(Into::into),
                    "--control" => control = args.next().map(Into::into),
                    other => {
                        eprintln!("vz-worker: unknown arg {other}");
                        std::process::exit(2);
                    }
                }
            }
            let Some(spec) = spec else {
                eprintln!("vz-worker: usage: nebulad vz-worker --spec <json> [--restore <p>] [--control <p>]");
                std::process::exit(2);
            };
            if let Err(e) =
                nebula_core::backend::vz::run_worker(&spec, restore.as_deref(), control.as_deref())
            {
                eprintln!("vz-worker: {e}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
    }
    let _ = Args::parse();
    let paths = paths::Paths::new()?;
    paths.ensure_dirs()?;

    // Log to file (the daemon usually runs detached).
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.daemon_log())?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(log_file)
        .with_ansi(false)
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "nebulad starting");

    // Single-instance guard.
    if let Some(pid) = read_live_pid(&paths) {
        anyhow::bail!("nebulad already running (pid {pid})");
    }
    std::fs::write(paths.pid_file(), std::process::id().to_string())?;

    let result = run(&paths);
    let _ = std::fs::remove_file(paths.pid_file());
    let _ = std::fs::remove_file(paths.control_sock());
    if let Err(e) = &result {
        tracing::error!("nebulad exiting with error: {e:#}");
    }
    result
}

fn run(paths: &paths::Paths) -> anyhow::Result<()> {
    let cfg = config::Config::load(&paths.config_toml())?;
    let vessel = vessel::Vessel::boot(paths, &cfg)?;
    server::serve(paths, vessel)
}

fn read_live_pid(paths: &paths::Paths) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(paths.pid_file())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // Liveness probe; stale pid files are common after crashes.
    #[cfg(unix)]
    return (unsafe { libc::kill(pid as i32, 0) } == 0).then_some(pid);
    // Windows: ask tasklist whether that pid is a live nebulad. A stale pid
    // file must not block startup (force-kills don't run our cleanup).
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        (text.contains(&format!("\"{pid}\"")) && text.to_lowercase().contains("nebulad"))
            .then_some(pid)
    }
}
