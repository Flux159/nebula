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
        .join(format!("nebulad{}", std::env::consts::EXE_SUFFIX));
    anyhow::ensure!(
        nebulad.is_file(),
        "nebulad binary not found next to nebula ({})",
        nebulad.display()
    );
    // The daemon leaves the reason for a failed start here; drop any stale
    // copy so what we read back can only be this run's.
    let startup_error = client::nebula_home()?.join("run/startup-error.txt");
    let _ = std::fs::remove_file(&startup_error);

    let mut child = std::process::Command::new(&nebulad)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let t0 = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if client::daemon_running() {
            if let Ok(DaemonResponse::Status(s)) = client::request(&DaemonRequest::Status) {
                if s.agent.is_some() {
                    // Intentionally not waited on: nebulad outlives the CLI.
                    std::mem::forget(child);
                    println!(
                        "nebula up in {:?} (vm {}, agent healthy)",
                        t0.elapsed(),
                        s.vm_state
                    );
                    for p in s.ports.iter().filter(|p| !p.ok) {
                        println!(
                            "  warning: {} did not bind on {}{}",
                            p.service,
                            p.addr,
                            p.error
                                .as_ref()
                                .map(|e| format!(" ({e})"))
                                .unwrap_or_default()
                        );
                    }
                    println!(
                        "next: nebula setup docker — or run `nebula quickstart` for the guide"
                    );
                    return Ok(());
                }
            }
        }
        // nebulad's stderr is closed here, so a daemon that refuses to start
        // (a port conflict, a bad config) would otherwise show up only as the
        // 60s timeout below, pointing at a log the user then has to go read.
        if let Ok(Some(exit)) = child.try_wait() {
            let reason = std::fs::read_to_string(&startup_error).unwrap_or_default();
            if !reason.trim().is_empty() {
                bail!("nebulad failed to start:\n\n{}", reason.trim_end());
            }
            bail!(
                "nebulad exited before it was ready ({exit}) — check {}",
                client::nebula_home()?.join("logs/nebulad.log").display()
            );
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
            // Which ports this instance actually owns — the question that
            // matters when two of them are running (issue #22).
            let api = if s.net.api_port == 0 {
                "off".to_string()
            } else {
                format!("{}:{}", s.net.api_host, s.net.api_port)
            };
            println!(
                "  ports:    api {} | dns udp {} | k8s {} | zone {}",
                api, s.net.dns_port, s.net.k8s_port, s.net.dns_zone
            );
            let failed: Vec<_> = s.ports.iter().filter(|p| !p.ok).collect();
            if failed.is_empty() {
                let bound = s.ports.len();
                if bound > 0 {
                    println!("  listeners: {bound} bound, all healthy");
                }
            } else {
                // The state that used to be invisible: healthy VM, healthy
                // agent, and nothing listening where clients are pointed.
                println!("  listeners: {} FAILED to bind", failed.len());
                for p in failed {
                    println!(
                        "    {} on {}{}",
                        p.service,
                        p.addr,
                        p.error
                            .as_ref()
                            .map(|e| format!(" — {e}"))
                            .unwrap_or_default()
                    );
                }
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
    let (resp, (reader, writer)) = client::request_on(stream, &DaemonRequest::Shell { open })?;
    match resp {
        DaemonResponse::ShellStarted => {}
        DaemonResponse::Error { message } => bail!("shell failed: {message}"),
        other => bail!("unexpected response: {other:?}"),
    }

    interactive_pump(reader, writer)
}

/// Raw-mode TTY <-> stream pump shared by `nebula shell` and
/// `nebula vessels shell`. Consumes the connection.
pub fn interactive_pump(
    mut reader: std::io::BufReader<nebula_core::ipc::IpcStream>,
    writer: nebula_core::ipc::IpcStream,
) -> anyhow::Result<()> {
    let _raw = RawTerm::enable()?;

    // stdin -> remote
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

    // remote -> stdout
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

pub fn quickstart() -> anyhow::Result<()> {
    let bold = |s: &str| format!("\x1b[1m{s}\x1b[0m");
    let running = client::daemon_running();

    println!("{}", bold("NEBULA QUICKSTART"));
    println!();
    println!("{}", bold("1. Engine"));
    if running {
        println!("   ✓ the engine is running");
    }
    println!("   nebula up                          start the engine (~0.6s)");
    println!("   nebula down                        stop it (containers stop too)");
    println!("   nebula autostart enable            start it at login + restart on failure");
    println!("   nebula status                      see where you stand anytime");
    println!();
    println!("{}", bold("2. Docker"));
    println!("   nebula setup docker                point `docker` at Nebula");
    println!("   docker run -d -p 8080:80 nginx     test it: http://localhost:8080");
    println!("   nebula revert docker               put docker back exactly as it was");
    println!("   nebula docker ps                   one-off, never touches your contexts");
    println!();
    println!("{}", bold("3. Kubernetes (k3s, starts on demand)"));
    println!("   nebula setup kubectl               point `kubectl` at Nebula");
    println!("   kubectl create deployment web --image=nginx");
    println!("   kubectl expose deployment web --port 80 --type NodePort");
    println!("   nebula revert kubectl              restore your previous context");
    println!("   nebula kubectl get nodes           one-off, never touches your contexts");
    println!();
    println!("{}", bold("4. Helm"));
    println!("   helm uses your kubectl context — `nebula setup kubectl` covers it too");
    println!("   nebula helm install my-redis oci://registry-1.docker.io/bitnamicharts/redis");
    println!();
    println!("{}", bold("5. Your own VMs"));
    println!("   nebula vessels new dev             persistent named microVM (--gpu for GPU)");
    println!("   nebula vessels shell dev           drop into it (ls/stop/rm to manage)");
    println!("   nebula vessels snapshot dev s1     disks + live RAM/processes on vz vessels");
    println!("                                      (--no-memory: ~10ms disk-only clone)");
    println!("   nebula sandbox run -- uname -a     or ephemeral: boots+runs+exits in ~250ms");
    println!();
    println!("{}", bold("6. Desktop app & stats"));
    println!("   nebula ui                          open the app");
    println!("   nebula stats --watch               watch the memory balloon breathe");
    println!();
    println!("{}", bold("7. Working with AI agents"));
    println!("   nebula skill > SKILL.md            full machine-readable usage guide");
    println!("                                      (vessels, snapshots, tree-search patterns)");
    println!();

    let cli_st = crate::pathsetup::status();
    if !cli_st.missing_from_path.is_empty() && cli_st.bundle_dir.is_some() {
        println!(
            "{} not on your PATH — `nebula setup path` links the bundled copies",
            cli_st.missing_from_path.join(", ")
        );
        println!();
    }
    // End with live status so the user knows their immediate next move.
    if running {
        status()?;
    } else {
        println!("engine is {} — start with: nebula up", bold("stopped"));
    }
    Ok(())
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
    let cli_st = crate::pathsetup::status();
    check(
        "docker/kubectl/helm available",
        cli_st.missing_from_path.is_empty(),
        "run `nebula setup path` to use the copies bundled with Nebula",
    );
    check(
        "daemon running",
        client::daemon_running(),
        "run `nebula up`",
    );
    if client::daemon_running() {
        let agent_ok = matches!(
            client::request(&DaemonRequest::Status),
            Ok(DaemonResponse::Status(s)) if s.agent.is_some()
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
    // The live engine holds disks/rootfs.img open read-write; overwriting it
    // under a running VM corrupts the guest (observed: dockerd panics).
    anyhow::ensure!(
        !client::daemon_running(),
        "the engine is running — stop it first: nebula down"
    );
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

    // Accept gzip artifacts directly (app bundles / release downloads).
    let staging = home.join("cache/image-install");
    let kernel_src = maybe_gunzip(&kernel_src, &staging.join("Image"))?;
    let rootfs_src = maybe_gunzip(&rootfs_src, &staging.join("rootfs.img"))?;

    std::fs::copy(&kernel_src, home.join("kernel/Image"))?;
    // Pristine store: the live engine disk mutates from here; resets
    // (`nebula vessels reset`) re-clone from this untouched copy.
    std::fs::create_dir_all(home.join("images"))?;
    std::fs::copy(&rootfs_src, home.join("images/rootfs-pristine.img"))?;
    std::fs::copy(&rootfs_src, home.join("disks/rootfs.img"))?;
    println!("installed kernel:  {}", kernel_src.display());
    println!("installed rootfs:  {}", rootfs_src.display());
    Ok(())
}

/// Decompress `src` to `dst` when it's gzip; otherwise return `src` as-is.
/// Pure Rust (flate2): no `gzip` binary on stock Windows.
pub fn maybe_gunzip(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<PathBuf> {
    let mut magic = [0u8; 2];
    use std::io::Read;
    std::fs::File::open(src)?.read_exact(&mut magic).ok();
    if magic != [0x1f, 0x8b] {
        return Ok(src.to_path_buf());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    gunzip(src, dst)?;
    Ok(dst.to_path_buf())
}

fn gunzip(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    let mut decoder =
        flate2::read::GzDecoder::new(std::io::BufReader::new(std::fs::File::open(src)?));
    let mut out = std::io::BufWriter::new(std::fs::File::create(dst)?);
    std::io::copy(&mut decoder, &mut out)
        .map_err(|e| anyhow::anyhow!("decompress {} failed: {e}", src.display()))?;
    Ok(())
}

/// Verify `files` against a `shasum -a 256`-format checksum list.
fn verify_sha256sums(dir: &std::path::Path, sums_file: &str) -> anyhow::Result<()> {
    use sha2::Digest;
    let sums = std::fs::read_to_string(dir.join(sums_file))?;
    let mut checked = 0;
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let (Some(expected), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim_start_matches('*');
        let mut hasher = sha2::Sha256::new();
        let mut f = std::fs::File::open(dir.join(name))?;
        std::io::copy(&mut f, &mut hasher)?;
        let actual = format!("{:x}", hasher.finalize());
        anyhow::ensure!(
            actual == expected,
            "checksum mismatch for {name}: expected {expected}, got {actual}"
        );
        checked += 1;
    }
    anyhow::ensure!(checked > 0, "no entries in {sums_file}");
    Ok(())
}

/// Guest-image artifact arch tag for this host (guest arch == host arch).
fn images_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x86_64",
    }
}

/// Default release channel for guest images (override: NEBULA_IMAGES_URL).
const IMAGES_BASE_URL: &str = "https://github.com/Flux159/nebula/releases/latest/download";

/// Download kernel + rootfs artifacts, verify checksums, install them.
/// This is the end-user bootstrap: no Docker required on the host — the
/// images are built in CI (see .github/workflows/guest-images.yml).
pub fn download_images() -> anyhow::Result<()> {
    let base = std::env::var("NEBULA_IMAGES_URL").unwrap_or_else(|_| IMAGES_BASE_URL.into());
    let arch = images_arch();
    let home = client::nebula_home()?;
    let tmp = home.join("cache/image-download");
    std::fs::create_dir_all(&tmp)?;

    let sums = format!("SHA256SUMS-{arch}");
    let kernel_gz = format!("kernel-Image-{arch}.gz");
    let rootfs_gz = format!("rootfs-{arch}.img.gz");

    println!("downloading {arch} guest images from {base} …");
    for name in [sums.as_str(), kernel_gz.as_str(), rootfs_gz.as_str()] {
        // curl ships on macOS, Linux distros we care about, and Windows 10+.
        let status = std::process::Command::new("curl")
            .args(["-fL", "--retry", "3", "--progress-bar", "-o"])
            .arg(tmp.join(name))
            .arg(format!("{base}/{name}"))
            .status()?;
        anyhow::ensure!(status.success(), "download failed: {base}/{name}");
    }

    // Verify before touching anything (pure Rust: no shasum on Windows).
    verify_sha256sums(&tmp, &sums)?;

    let kernel = tmp.join("Image");
    let rootfs = tmp.join("rootfs.img");
    gunzip(&tmp.join(&kernel_gz), &kernel)?;
    gunzip(&tmp.join(&rootfs_gz), &rootfs)?;
    install_image(Some(kernel), Some(rootfs))?;
    let _ = std::fs::remove_dir_all(&tmp);
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
    // End users: fetch the CI-built artifacts (no Docker needed).
    println!("guest images not found locally — fetching the released images");
    download_images()
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

#[cfg(unix)]
pub fn term_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
}

/// TODO(windows): GetConsoleScreenBufferInfo; callers default to 80x24.
#[cfg(windows)]
pub fn term_size() -> Option<(u16, u16)> {
    None
}

/// RAII raw-mode terminal guard.
#[cfg(unix)]
struct RawTerm {
    original: libc::termios,
}

#[cfg(unix)]
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

#[cfg(unix)]
impl Drop for RawTerm {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.original) };
    }
}

/// TODO(windows): ConPTY/SetConsoleMode raw input. Line-buffered shells
/// still work; interactive TUIs inside `nebula shell` won't until then.
#[cfg(windows)]
struct RawTerm;

#[cfg(windows)]
impl RawTerm {
    fn enable() -> anyhow::Result<Self> {
        Ok(Self)
    }
}
