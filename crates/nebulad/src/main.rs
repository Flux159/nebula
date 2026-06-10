//! nebulad: owns the Vessel VM and serves the control socket.
//!
//! v0 lifecycle: start -> boot Vessel -> serve until `down` -> stop VM -> exit.
//! Crash recovery / auto-restart hardening lands in Phase 6.

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
    (unsafe { libc::kill(pid as i32, 0) } == 0).then_some(pid)
}
