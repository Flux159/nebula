//! `nebula` command implementations (Phase 1 set).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use nebula_core::proto::*;

use crate::client;

pub fn up() -> anyhow::Result<()> {
    if client::daemon_running() {
        println!("nebula is already running");
        return status();
    }
    ensure_images_installed()?;

    // Spawn nebulad (same dir as this binary) detached.
    let exe = std::env::current_exe()?;
    let nebulad = exe
        .parent()
        .context("exe has no parent dir")?
        .join("nebulad");
    anyhow::ensure!(
        nebulad.is_file(),
        "nebulad binary not found next to nebula ({})",
        nebulad.display()
    );
    let child = std::process::Command::new(&nebulad)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // Intentionally not waited on: nebulad outlives the CLI.
    std::mem::forget(child);

    let t0 = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if client::daemon_running() {
            if let Ok(DaemonResponse::Status(s)) = client::request(&DaemonRequest::Status) {
                if s.agent.is_some() {
                    println!(
                        "nebula up in {:?} (vm {}, agent healthy)",
                        t0.elapsed(),
                        s.vm_state
                    );
                    println!();
                    println!("next steps:");
                    println!("  nebula setup docker                # point `docker` here (undo: nebula revert docker)");
                    println!(
                        "  docker run -d -p 8080:80 nginx     # then open http://localhost:8080"
                    );
                    println!("  nebula setup kubectl               # local Kubernetes (k3s)");
                    println!("  nebula --help                      # full quickstart");
                    return Ok(());
                }
            }
        }
        if Instant::now() > deadline {
            bail!(
                "nebulad did not become ready within 60s — check {}/logs/nebulad.log",
                client::nebula_home()?.display()
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

pub fn down(force: bool) -> anyhow::Result<()> {
    if !client::daemon_running() {
        println!("nebula is not running");
        return Ok(());
    }
    match client::request(&DaemonRequest::Down { force })? {
        DaemonResponse::Ok => {
            println!("nebula stopped");
            Ok(())
        }
        DaemonResponse::Error { message } => bail!("stop failed: {message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

pub fn status() -> anyhow::Result<()> {
    if !client::daemon_running() {
        println!("nebula: stopped (daemon not running)");
        println!("  start it:          nebula up");
        println!("  start at login:    nebula autostart enable");
        return Ok(());
    }
    match client::request(&DaemonRequest::Status)? {
        DaemonResponse::Status(s) => {
            println!("nebula: {}", s.vm_state.to_lowercase());
            println!(
                "  backend:  {} | cpus {} | max ram {} MiB",
                s.backend, s.cpus, s.mem_mib
            );
            println!(
                "  daemon:   v{} (pid {}), up {}s",
                s.daemon_version, s.daemon_pid, s.uptime_secs
            );
            match s.agent {
                Some(a) => println!(
                    "  agent:    v{} healthy | kernel {} | guest uptime {}s",
                    a.agent_version, a.kernel, a.uptime_secs
                ),
                None => println!("  agent:    UNREACHABLE"),
            }
            let pointing = crate::contexts::pointing_at_nebula();
            if !pointing.is_empty() {
                println!("  contexts: {} → nebula", pointing.join(", "));
            }
            if let Some(m) = s.mem {
                println!(
                    "  guest mem: {} / {} MiB available{}",
                    m.available_kib / 1024,
                    m.total_kib / 1024,
                    m.psi_some_avg10
                        .map(|p| format!(" | pressure {p:.1}%"))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        DaemonResponse::Error { message } => bail!("status failed: {message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

pub fn exec(cmd: Vec<String>) -> anyhow::Result<()> {
    anyhow::ensure!(!cmd.is_empty(), "usage: nebula exec <cmd> [args…]");
    let req = DaemonRequest::Agent {
        request: AgentRequest::Exec {
            cmd: cmd[0].clone(),
            args: cmd[1..].to_vec(),
            env: vec![],
            timeout_ms: 60_000,
        },
    };
    match client::request(&req)? {
        DaemonResponse::Agent {
            response: AgentResponse::Exec(r),
        } => {
            print!("{}", r.stdout);
            eprint!("{}", r.stderr);
            if r.timed_out {
                eprintln!("nebula: command timed out");
            }
            std::process::exit(r.exit_code);
        }
        DaemonResponse::Agent {
            response: AgentResponse::Error { message },
        } => {
            bail!("exec failed: {message}")
        }
        DaemonResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

pub fn shell() -> anyhow::Result<()> {
    let (cols, rows) = term_size().unwrap_or((80, 24));
    let open = ShellOpen {
        cmd: "/bin/sh".into(),
        args: vec![],
        cols,
        rows,
    };
    let stream = client::connect()?;
    let (resp, (mut reader, writer)) = client::request_on(stream, &DaemonRequest::Shell { open })?;
    match resp {
        DaemonResponse::ShellStarted => {}
        DaemonResponse::Error { message } => bail!("shell failed: {message}"),
        other => bail!("unexpected response: {other:?}"),
    }

    let _raw = RawTerm::enable()?;

    // stdin -> daemon
    let mut writer_in = writer.try_clone()?;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer_in.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = writer_in.shutdown(std::net::Shutdown::Write);
    });

    // daemon -> stdout
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
        }
    }
    let _ = writer.shutdown(std::net::Shutdown::Both);
    Ok(())
}

pub fn stats(watch: bool) -> anyhow::Result<()> {
    loop {
        match client::request(&DaemonRequest::Stats)? {
            DaemonResponse::Stats(s) => {
                let line = match &s.guest {
                    Some(g) => format!(
                        "guest {used}/{target} MiB used (avail {avail}) | balloon holds {balloon} MiB of {max} | host footprint {host} MiB{psi}",
                        used = ((g.total_kib - g.available_kib) / 1024)
                            .saturating_sub(s.max_mib - s.balloon_target_mib)
                            .min(s.balloon_target_mib),
                        target = s.balloon_target_mib,
                        avail = g.available_kib / 1024,
                        balloon = s.max_mib - s.balloon_target_mib,
                        max = s.max_mib,
                        host = s.host_footprint_mib,
                        psi = g
                            .psi_some_avg10
                            .map(|p| format!(" | pressure {p:.1}%"))
                            .unwrap_or_default(),
                    ),
                    None => "guest stats unavailable".to_string(),
                };
                println!("{line}");
            }
            DaemonResponse::Error { message } => bail!("stats failed: {message}"),
            other => bail!("unexpected response: {other:?}"),
        }
        if !watch {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

pub fn logs(follow: bool) -> anyhow::Result<()> {
    let console = client::nebula_home()?.join("logs/vessel-console.log");
    anyhow::ensure!(console.is_file(), "no console log at {}", console.display());
    let mut cmd = std::process::Command::new("tail");
    cmd.arg(if follow { "-f" } else { "-n" });
    if !follow {
        cmd.arg("200");
    }
    let status = cmd.arg(&console).status()?;
    anyhow::ensure!(status.success(), "tail failed");
    Ok(())
}

pub fn doctor() -> anyhow::Result<()> {
    let mut problems = 0;
    let mut check = |name: &str, ok: bool, hint: &str| {
        println!("{} {}", if ok { "✓" } else { "✗" }, name);
        if !ok {
            println!("    ↳ {hint}");
            problems += 1;
        }
    };

    check(
        "Apple Silicon (arm64)",
        cfg!(target_arch = "aarch64"),
        "Nebula requires an Apple Silicon Mac",
    );
    let home = client::nebula_home()?;
    check(
        "kernel installed",
        home.join("kernel/Image").is_file(),
        "run `nebula up` (dev: vessel/build-kernel.sh then re-run nebula up)",
    );
    check(
        "rootfs installed",
        home.join("disks/rootfs.img").is_file(),
        "run `nebula up` (dev: vessel/build-rootfs.sh then re-run nebula up)",
    );
    let exe = std::env::current_exe()?;
    let entitled = std::process::Command::new("codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(&exe)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("com.apple.security.virtualization"))
        .unwrap_or(false);
    check(
        "binary signed with virtualization entitlement",
        entitled,
        "run scripts/sign-dev.sh on the nebula + nebulad binaries",
    );
    let rosetta = std::path::Path::new("/Library/Apple/usr/share/rosetta").exists()
        || std::process::Command::new("arch")
            .args(["-x86_64", "/usr/bin/true"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    check(
        "Rosetta installed (amd64 containers)",
        rosetta,
        "run: softwareupdate --install-rosetta --agree-to-license",
    );
    check(
        "daemon running",
        client::daemon_running(),
        "run `nebula up`",
    );
    if client::daemon_running() {
        let agent_ok = matches!(
            client::request(&DaemonRequest::Status),
            Ok(DaemonResponse::Status(DaemonStatus { agent: Some(_), .. }))
        );
        check(
            "guest agent healthy",
            agent_ok,
            "check `nebula logs` for boot errors",
        );
    }

    if problems == 0 {
        println!("\nall checks passed");
        Ok(())
    } else {
        bail!("{problems} problem(s) found")
    }
}

/// Copy kernel + rootfs from the dev build tree (vessel/out) or explicit paths
/// into ~/.nebula. v1 will download signed images instead.
pub fn install_image(kernel: Option<PathBuf>, rootfs: Option<PathBuf>) -> anyhow::Result<()> {
    let home = client::nebula_home()?;
    std::fs::create_dir_all(home.join("kernel"))?;
    std::fs::create_dir_all(home.join("disks"))?;

    let (kernel_src, rootfs_src) = match (kernel, rootfs) {
        (Some(k), Some(r)) => (k, r),
        (k, r) => {
            let repo_out = find_repo_vessel_out()?;
            (
                k.unwrap_or_else(|| repo_out.join("Image")),
                r.unwrap_or_else(|| repo_out.join("rootfs.img")),
            )
        }
    };
    anyhow::ensure!(
        kernel_src.is_file(),
        "kernel not found: {}",
        kernel_src.display()
    );
    anyhow::ensure!(
        rootfs_src.is_file(),
        "rootfs not found: {}",
        rootfs_src.display()
    );

    std::fs::copy(&kernel_src, home.join("kernel/Image"))?;
    std::fs::copy(&rootfs_src, home.join("disks/rootfs.img"))?;
    println!("installed kernel:  {}", kernel_src.display());
    println!("installed rootfs:  {}", rootfs_src.display());
    Ok(())
}

fn ensure_images_installed() -> anyhow::Result<()> {
    let home = client::nebula_home()?;
    if home.join("kernel/Image").is_file() && home.join("disks/rootfs.img").is_file() {
        return Ok(());
    }
    // Dev convenience: auto-install from the repo build tree when present.
    if let Ok(out) = find_repo_vessel_out() {
        if out.join("Image").is_file() && out.join("rootfs.img").is_file() {
            println!("installing guest images from {}", out.display());
            return install_image(None, None);
        }
    }
    bail!(
        "guest images missing — build them first:\n  \
         vessel/build-kernel.sh && vessel/build-rootfs.sh && nebula install-image"
    );
}

/// Dev-mode discovery of <repo>/vessel/out relative to the running binary.
fn find_repo_vessel_out() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    for dir in exe.ancestors() {
        let candidate = dir.join("vessel/out");
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    bail!("not in a nebula repo (vessel/out not found)")
}

fn term_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
}

/// RAII raw-mode terminal guard.
struct RawTerm {
    original: libc::termios,
}

impl RawTerm {
    fn enable() -> anyhow::Result<Self> {
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        anyhow::ensure!(
            unsafe { libc::tcgetattr(0, &mut original) } == 0,
            "stdin is not a terminal"
        );
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) };
        Ok(Self { original })
    }
}

impl Drop for RawTerm {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.original) };
    }
}
