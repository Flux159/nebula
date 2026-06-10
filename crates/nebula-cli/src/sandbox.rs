//! `nebula sandbox run`: ephemeral, isolated libkrun microVMs (the sidecar
//! engine). Phase 7 scope: boot our shared guest image as a throwaway VM,
//! run a command in it, stream output, tear down. Image store is independent
//! of the Vessel by design (documented limitation — tasks/issues.md).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use nebula_core::backend::{backend_by_name, VmState};
use nebula_core::initramfs::InitramfsBuilder;
use nebula_core::{BootSpec, ConsoleSpec, NetSpec, ShareSpec, VmSpec};

use crate::client;

pub struct SandboxOpts {
    pub cpus: u32,
    pub mem: u64,
    pub share_cwd: bool,
    pub cmd: Vec<String>,
}

pub fn run(opts: SandboxOpts) -> anyhow::Result<()> {
    let home = client::nebula_home()?;
    let kernel = home.join("kernel/Image");
    anyhow::ensure!(
        kernel.is_file(),
        "guest kernel not installed — run `nebula up` once (or nebula install-image)"
    );

    let cache = home.join("cache");
    let run_dir = home.join("run");
    std::fs::create_dir_all(&cache)?;
    std::fs::create_dir_all(&run_dir)?;

    let id = format!("sbx-{}", std::process::id());
    let console_log = run_dir.join(format!("{id}-console.log"));

    // Sandbox initramfs: vessel-init in spike-ish mode runs /sandbox-cmd
    // instead of the marker when present (see vessel-init).
    let script = {
        let mut sh = String::from(
            "#!/bin/sh\nexport PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n",
        );
        if opts.share_cwd {
            sh.push_str(
                "mkdir -p /workdir && mount -t virtiofs cwd /workdir 2>/dev/null && cd /workdir\n",
            );
        }
        sh.push_str(&shell_join(&opts.cmd));
        sh.push('\n');
        sh
    };

    let init_bin = guest_binary("vessel-init")?;
    let busybox = fetch_busybox(&cache)?;
    let mut b = InitramfsBuilder::new()
        .dir("/dev", 0o755)
        .char_dev("/dev/console", 5, 1, 0o600)
        .dir("/proc", 0o755)
        .dir("/sys", 0o755)
        .dir("/bin", 0o755)
        .dir("/usr", 0o755)
        .dir("/usr/bin", 0o755)
        .dir("/tmp", 0o1777)
        .file("/init", std::fs::read(&init_bin)?, 0o755)
        .file("/bin/busybox", busybox.clone(), 0o755)
        .file("/sandbox-cmd", script.into_bytes(), 0o755);
    // busybox applet symlinks for a usable shell environment
    for applet in [
        "sh", "mount", "ls", "cat", "echo", "uname", "env", "mkdir", "wget", "ip",
    ] {
        b = b.symlink(&format!("/bin/{applet}"), "/bin/busybox");
    }
    let initramfs_path = cache.join(format!("{id}-initramfs.cpio"));
    std::fs::write(&initramfs_path, b.build())?;

    let mut shares = vec![];
    if opts.share_cwd {
        shares.push(ShareSpec {
            tag: "cwd".into(),
            host_path: std::env::current_dir()?,
            read_only: false,
        });
    }

    let spec = VmSpec {
        name: id.clone(),
        cpus: opts.cpus,
        mem_mib: opts.mem.max(1024), // libkrun layout floor
        boot: BootSpec::Kernel {
            kernel,
            initramfs: Some(initramfs_path.clone()),
            cmdline: "console=hvc0 reboot=k NEBULA_SANDBOX=1".into(),
        },
        disks: vec![],
        shares,
        net: NetSpec::None,
        vsock: false,
        console: ConsoleSpec::File(console_log.clone()),
        balloon: false,
        rng: true,
        rosetta: false,
        gpu: false,
    };

    let backend = backend_by_name("krun")?;
    backend.is_available().context(
        "libkrun unavailable — build it with scripts/build-libkrun.sh or set NEBULA_LIBKRUN_PATH",
    )?;
    let mut vm = backend.create(&spec)?;
    let t0 = Instant::now();
    vm.start()?;
    vm.wait_for(VmState::Stopped, Duration::from_secs(600))?;
    let elapsed = t0.elapsed();

    let console = std::fs::read_to_string(&console_log).unwrap_or_default();
    // Print everything between our begin/end markers (the workload's output).
    let mut printing = false;
    let mut exit_code = 0;
    for line in console.lines() {
        if line.contains("NEBULA_SANDBOX_BEGIN") {
            printing = true;
            continue;
        }
        if let Some(rest) = line.split("NEBULA_SANDBOX_END=").nth(1) {
            exit_code = rest.trim().parse().unwrap_or(1);
            break;
        }
        if printing {
            println!("{line}");
        }
    }
    eprintln!("[sandbox] {id} done in {elapsed:.2?} (exit {exit_code})");
    let _ = std::fs::remove_file(&initramfs_path);
    let _ = std::fs::remove_file(&console_log);
    std::process::exit(exit_code);
}

fn shell_join(cmd: &[String]) -> String {
    cmd.iter()
        .map(|c| {
            if c.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_./=:".contains(ch))
            {
                c.clone()
            } else {
                format!("'{}'", c.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn guest_binary(name: &str) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    for dir in exe.ancestors() {
        if dir.file_name().is_some_and(|n| n == "target") {
            for profile in ["release", "debug"] {
                let p = dir
                    .join("aarch64-unknown-linux-musl")
                    .join(profile)
                    .join(name);
                if p.is_file() {
                    return Ok(p);
                }
            }
        }
    }
    bail!("{name} (musl) not built — cargo build -p {name} --release --target aarch64-unknown-linux-musl")
}

/// Static arm64 busybox for the sandbox initramfs (cached).
fn fetch_busybox(cache: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    let path = cache.join("busybox-arm64");
    if path.is_file() {
        return Ok(std::fs::read(&path)?);
    }
    // Resolve the current busybox-static apk (the -rN suffix changes).
    let list = std::process::Command::new("sh")
        .arg("-c")
        .arg(r"curl -fsSL https://dl-cdn.alpinelinux.org/alpine/v3.22/main/aarch64/ | grep -oE 'busybox-static-[0-9r.-]+\.apk' | head -1")
        .output()?;
    let apk = String::from_utf8_lossy(&list.stdout).trim().to_string();
    anyhow::ensure!(!apk.is_empty(), "could not resolve busybox-static apk name");
    let url = format!("https://dl-cdn.alpinelinux.org/alpine/v3.22/main/aarch64/{apk}");
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("curl -fsSL {url} | tar -xzO bin/busybox.static"))
        .output()?;
    anyhow::ensure!(
        out.status.success() && !out.stdout.is_empty(),
        "busybox download failed (offline?) — sandbox needs ~/.nebula/cache/busybox-arm64"
    );
    std::fs::write(&path, &out.stdout)?;
    Ok(out.stdout)
}
