//! `nebula vessels` — persistent named microVMs alongside the engine vessel.
//!
//! Each named vessel is a libkrun VM with its own copy-on-write rootfs clone
//! and sparse data disk, booting the same guest image as the engine but in
//! agent-only mode (no docker/k8s — those live in the engine vessel, which
//! this command intentionally cannot stop). The agent's vsock ports are
//! mapped to per-vessel unix sockets, so exec/shell work daemon-free.
//!
//! Layout: ~/.nebula/vessels/<name>/{spec.json,pid,rootfs.img,data.img,
//! console.log,agent.sock,shell.sock}

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use nebula_core::proto::*;
use nebula_core::{BootSpec, ConsoleSpec, DiskSpec, NetSpec, VmSpec, VsockPortMap};

use crate::client;

/// Names that would collide with the engine vessel or confuse routing.
const RESERVED: &[&str] = &["vessel", "default", "engine", "nebula"];

pub struct NewOpts {
    pub name: String,
    pub cpus: u32,
    pub mem: u64,
    pub gpu: bool,
    pub data_gib: u64,
}

fn vessels_root() -> anyhow::Result<PathBuf> {
    Ok(client::nebula_home()?.join("vessels"))
}

fn dir_of(name: &str) -> anyhow::Result<PathBuf> {
    validate_name(name)?;
    Ok(vessels_root()?.join(name))
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if RESERVED.contains(&name) {
        bail!(
            "`{name}` is the engine vessel — it runs docker/kubernetes and is managed with `nebula up` / `nebula down`"
        );
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("vessel names must be non-empty [a-zA-Z0-9_-], got `{name}`");
    }
    Ok(())
}

fn read_spec(dir: &std::path::Path) -> anyhow::Result<VmSpec> {
    let raw = std::fs::read_to_string(dir.join("spec.json"))
        .with_context(|| format!("no vessel at {}", dir.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

fn live_pid(dir: &std::path::Path) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(dir.join("pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (unsafe { libc::kill(pid, 0) } == 0).then_some(pid)
}

pub fn new(opts: NewOpts) -> anyhow::Result<()> {
    let dir = dir_of(&opts.name)?;
    anyhow::ensure!(
        !dir.exists(),
        "vessel `{}` already exists (nebula vessels rm {} to recreate)",
        opts.name,
        opts.name
    );

    let home = client::nebula_home()?;
    let base_rootfs = home.join("disks/rootfs.img");
    let kernel = home.join("kernel/Image");
    anyhow::ensure!(
        base_rootfs.is_file() && kernel.is_file(),
        "guest images missing — run `nebula up` once first"
    );

    std::fs::create_dir_all(&dir)?;
    // APFS copy-on-write clone: instant and space-shared with the base.
    let status = std::process::Command::new("cp")
        .arg("-c")
        .arg(&base_rootfs)
        .arg(dir.join("rootfs.img"))
        .status()?;
    anyhow::ensure!(status.success(), "rootfs clone failed");
    let data = std::fs::File::create(dir.join("data.img"))?;
    data.set_len(opts.data_gib * 1024 * 1024 * 1024)?;

    let spec = VmSpec {
        name: format!("vessel-{}", opts.name),
        cpus: opts.cpus,
        mem_mib: opts.mem.max(1024),
        boot: BootSpec::Kernel {
            kernel,
            initramfs: None,
            cmdline:
                "console=hvc0 root=/dev/vda rw rootfstype=ext4 init=/sbin/nebula-init reboot=k panic=10 NEBULA_AGENT_ONLY=1"
                    .into(),
        },
        disks: vec![
            DiskSpec { path: dir.join("rootfs.img"), read_only: false },
            DiskSpec { path: dir.join("data.img"), read_only: false },
        ],
        shares: vec![],
        net: NetSpec::None, // libkrun TSI handles outbound transparently
        vsock: false,
        console: ConsoleSpec::File(dir.join("console.log")),
        balloon: false,
        rng: true,
        rosetta: false,
        gpu: opts.gpu,
        vsock_ports: vec![
            VsockPortMap { port: VSOCK_PORT_CONTROL, host_path: dir.join("agent.sock") },
            VsockPortMap { port: VSOCK_PORT_SHELL, host_path: dir.join("shell.sock") },
        ],
    };
    std::fs::write(dir.join("spec.json"), serde_json::to_vec_pretty(&spec)?)?;
    println!(
        "created vessel `{}` ({} cpus, {} MiB{})",
        opts.name,
        opts.cpus,
        spec.mem_mib,
        if opts.gpu { ", gpu" } else { "" }
    );
    start(&opts.name)
}

pub fn start(name: &str) -> anyhow::Result<()> {
    let dir = dir_of(name)?;
    let spec = read_spec(&dir)?;
    if live_pid(&dir).is_some() {
        println!("vessel `{name}` is already running");
        return Ok(());
    }

    let spec_json = serde_json::to_string(&spec)?;
    let exe = std::env::current_exe()?;
    let console = std::fs::File::create(dir.join("console.log"))?;
    let child = std::process::Command::new(&exe)
        .arg("krun-worker")
        .arg("--spec")
        .arg(spec_json)
        .stdin(std::process::Stdio::null())
        .stdout(console)
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::fs::write(dir.join("pid"), child.id().to_string())?;
    std::mem::forget(child); // vessel outlives this CLI invocation

    // Wait for the agent socket to answer.
    let t0 = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(AgentResponse::Health(h)) = agent_request(&dir, &AgentRequest::Health) {
            println!(
                "vessel `{name}` up in {:?} (kernel {}, agent v{})",
                t0.elapsed(),
                h.kernel,
                h.agent_version
            );
            println!("  shell: nebula vessels shell {name}");
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!(
                "vessel `{name}` did not become healthy within 20s — see {}",
                dir.join("console.log").display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn stop(name: &str) -> anyhow::Result<()> {
    let dir = dir_of(name)?;
    let Some(pid) = live_pid(&dir) else {
        println!("vessel `{name}` is not running");
        return Ok(());
    };
    // Graceful first: agent powers the guest off and the worker exits.
    let _ = agent_request(&dir, &AgentRequest::Shutdown);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid, 0) } != 0 {
            println!("vessel `{name}` stopped");
            let _ = std::fs::remove_file(dir.join("pid"));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let _ = std::fs::remove_file(dir.join("pid"));
    println!("vessel `{name}` stopped (forced)");
    Ok(())
}

pub fn rm(name: &str, force: bool) -> anyhow::Result<()> {
    let dir = dir_of(name)?;
    anyhow::ensure!(dir.exists(), "no vessel named `{name}`");
    if live_pid(&dir).is_some() {
        anyhow::ensure!(
            force,
            "vessel `{name}` is running — stop it first or use --force"
        );
        stop(name)?;
    }
    std::fs::remove_dir_all(&dir)?;
    println!("removed vessel `{name}`");
    Ok(())
}

pub fn ls() -> anyhow::Result<()> {
    println!(
        "{:<14} {:<9} {:>5} {:>9} {:>5}  NOTES",
        "NAME", "STATE", "CPUS", "MEM", "GPU"
    );
    // The engine vessel first — visible, but owned by nebula up/down.
    let engine = if client::daemon_running() {
        "running"
    } else {
        "stopped"
    };
    println!(
        "{:<14} {:<9} {:>5} {:>9} {:>5}  engine (docker/k8s) — use nebula up/down",
        "vessel", engine, "-", "-", "-"
    );

    let root = vessels_root()?;
    if root.is_dir() {
        let mut names: Vec<_> = std::fs::read_dir(&root)?
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            let dir = root.join(&name);
            let Ok(spec) = read_spec(&dir) else { continue };
            let state = if live_pid(&dir).is_some() {
                "running"
            } else {
                "stopped"
            };
            println!(
                "{:<14} {:<9} {:>5} {:>8}M {:>5}  ",
                name,
                state,
                spec.cpus,
                spec.mem_mib,
                if spec.gpu { "yes" } else { "no" }
            );
        }
    }
    Ok(())
}

pub fn info(name: &str) -> anyhow::Result<()> {
    let dir = dir_of(name)?;
    let spec = read_spec(&dir)?;
    let running = live_pid(&dir);
    println!("vessel:   {name}");
    println!(
        "state:    {}",
        if running.is_some() {
            "running"
        } else {
            "stopped"
        }
    );
    if let Some(pid) = running {
        println!("pid:      {pid}");
        if let Ok(AgentResponse::Health(h)) = agent_request(&dir, &AgentRequest::Health) {
            println!(
                "kernel:   {} (agent v{}, up {}s)",
                h.kernel, h.agent_version, h.uptime_secs
            );
        }
        if let Ok(AgentResponse::MemStats(m)) = agent_request(&dir, &AgentRequest::MemStats) {
            println!(
                "memory:   {} / {} MiB used",
                (m.total_kib - m.available_kib) / 1024,
                m.total_kib / 1024
            );
        }
    }
    println!("cpus:     {}", spec.cpus);
    println!("mem:      {} MiB", spec.mem_mib);
    println!(
        "gpu:      {}",
        if spec.gpu {
            "yes (virtio-gpu Venus)"
        } else {
            "no"
        }
    );
    println!("disks:    {}", dir.join("rootfs.img").display());
    println!("          {}", dir.join("data.img").display());
    println!("console:  {}", dir.join("console.log").display());
    Ok(())
}

pub fn exec(name: &str, cmd: Vec<String>) -> anyhow::Result<()> {
    anyhow::ensure!(
        !cmd.is_empty(),
        "usage: nebula vessels exec <name> -- <cmd> [args…]"
    );
    let dir = dir_of(name)?;
    anyhow::ensure!(live_pid(&dir).is_some(), "vessel `{name}` is not running");
    let resp = agent_request(
        &dir,
        &AgentRequest::Exec {
            cmd: cmd[0].clone(),
            args: cmd[1..].to_vec(),
            env: vec![],
            timeout_ms: 60_000,
        },
    )?;
    match resp {
        AgentResponse::Exec(r) => {
            print!("{}", r.stdout);
            eprint!("{}", r.stderr);
            std::process::exit(r.exit_code);
        }
        AgentResponse::Error { message } => bail!("exec failed: {message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

pub fn shell(name: &str) -> anyhow::Result<()> {
    let dir = dir_of(name)?;
    anyhow::ensure!(live_pid(&dir).is_some(), "vessel `{name}` is not running");
    let stream = UnixStream::connect(dir.join("shell.sock"))?;
    let (cols, rows) = crate::commands::term_size().unwrap_or((80, 24));
    let open = ShellOpen {
        cmd: "/bin/sh".into(),
        args: vec![],
        cols,
        rows,
    };
    let mut writer = stream.try_clone()?;
    let mut line = serde_json::to_string(&open)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    crate::commands::interactive_pump(BufReader::new(stream), writer)
}

fn agent_request(dir: &std::path::Path, req: &AgentRequest) -> anyhow::Result<AgentResponse> {
    let stream = UnixStream::connect(dir.join("agent.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(65)))?;
    let mut writer = stream.try_clone()?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    anyhow::ensure!(!resp.trim().is_empty(), "agent closed the connection");
    Ok(serde_json::from_str(resp.trim())?)
}
