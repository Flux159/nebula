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

/// Names that refer to the engine vessel (docker/k8s). Read-style commands
/// route there transparently; destructive ones refuse and point at
/// `nebula up` / `nebula down`.
const RESERVED: &[&str] = &["vessel", "default", "engine", "nebula"];

fn is_engine(name: &str) -> bool {
    RESERVED.contains(&name)
}

pub struct NewOpts {
    pub name: String,
    pub cpus: u32,
    pub mem: u64,
    pub gpu: bool,
    pub data_gib: u64,
    /// Build the vessel's rootfs from a docker image reference.
    pub from_image: Option<String>,
    /// Clone the rootfs from a prebuilt image file (.img or .img.gz, made by
    /// `vessels convert-image`) — offline, no engine needed.
    pub rootfs_img: Option<PathBuf>,
    /// Rootfs size when building from an image (MiB).
    pub rootfs_mb: u64,
    /// VMM: `krun` (fastest boot, GPU) or `vz` (live memory snapshots).
    pub backend: String,
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
    anyhow::ensure!(
        opts.backend == "krun" || opts.backend == "vz",
        "--backend must be `krun` or `vz`, got `{}`",
        opts.backend
    );
    anyhow::ensure!(
        !(opts.gpu && opts.backend == "vz"),
        "GPU passthrough (Venus) is libkrun-only — drop --backend vz or --gpu"
    );
    anyhow::ensure!(
        !(opts.from_image.is_some() && opts.rootfs_img.is_some()),
        "--from-image and --rootfs-img are mutually exclusive"
    );
    let dir = dir_of(&opts.name)?;
    anyhow::ensure!(
        !dir.exists(),
        "vessel `{}` already exists (nebula vessels rm {} to recreate)",
        opts.name,
        opts.name
    );

    let home = client::nebula_home()?;
    // Clone from the pristine store, not the engine's live (mutated) disk.
    let base_rootfs = if home.join("images/rootfs-pristine.img").is_file() {
        home.join("images/rootfs-pristine.img")
    } else {
        home.join("disks/rootfs.img")
    };
    let kernel = home.join("kernel/Image");
    anyhow::ensure!(
        kernel.is_file(),
        "guest kernel missing — run `nebula up` once first"
    );
    anyhow::ensure!(
        opts.from_image.is_some() || opts.rootfs_img.is_some() || base_rootfs.is_file(),
        "guest images missing — run `nebula up` once first"
    );

    std::fs::create_dir_all(&dir)?;
    if let Some(image) = &opts.from_image {
        // Docker image -> bootable microVM rootfs, built inside the engine
        // (it has docker + e2fsprogs; our static init/agent are injected so
        // ANY arm64 linux image becomes a manageable vessel).
        build_rootfs_from_image(image, &opts.name, &dir, opts.rootfs_mb, opts.data_gib)?;
    } else if let Some(src) = &opts.rootfs_img {
        // Prebuilt rootfs file (vessels convert-image): offline, engine-free.
        anyhow::ensure!(src.is_file(), "no rootfs image at {}", src.display());
        let raw = crate::commands::maybe_gunzip(src, &dir.join("rootfs.img"))?;
        if raw != dir.join("rootfs.img") {
            clone_file(&raw, &dir.join("rootfs.img"))?;
        }
        create_data_disk(&dir, opts.data_gib)?;
    } else {
        // APFS copy-on-write clone: instant and space-shared with the base.
        clone_file(&base_rootfs, &dir.join("rootfs.img"))?;
        let data = std::fs::File::create(dir.join("data.img"))?;
        data.set_len(opts.data_gib * 1024 * 1024 * 1024)?;
    }

    let vz = opts.backend == "vz";
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
        // krun: TSI handles outbound transparently with no NIC.
        // vz: a NAT NIC (DHCP via vessel-init; DNS through the engine's relay).
        net: if vz { NetSpec::Nat } else { NetSpec::None },
        vsock: vz, // VZ needs the device for the worker's socket proxies
        console: ConsoleSpec::File(dir.join("console.log")),
        balloon: false,
        rng: true,
        rosetta: false,
        gpu: opts.gpu,
        vsock_ports: vec![
            VsockPortMap { port: VSOCK_PORT_CONTROL, host_path: dir.join("agent.sock") },
            VsockPortMap { port: VSOCK_PORT_SHELL, host_path: dir.join("shell.sock") },
        ],
        backend: Some(opts.backend.clone()),
        // Stable MAC + machine id: keep the DHCP lease across restarts and
        // keep the config identical to any saved machine state.
        mac: if vz { Some(random_mac()?) } else { None },
        machine_id: if vz {
            Some(nebula_core::backend::vz::new_machine_id())
        } else {
            None
        },
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
    start_with(name, None)
}

/// Boot a vessel; with `restore` (vz only) the worker resumes from a saved
/// machine state instead of cold-booting the kernel.
fn start_with(name: &str, restore: Option<&std::path::Path>) -> anyhow::Result<()> {
    if is_engine(name) {
        return crate::commands::up();
    }
    let dir = dir_of(name)?;
    let spec = read_spec(&dir)?;
    if live_pid(&dir).is_some() {
        println!("vessel `{name}` is already running");
        return Ok(());
    }
    let backend = spec.backend.clone().unwrap_or_else(|| "krun".into());
    anyhow::ensure!(
        restore.is_none() || backend == "vz",
        "memory-state restore requires a vz-backed vessel"
    );

    let spec_json = serde_json::to_string(&spec)?;
    let exe = std::env::current_exe()?;
    let child = if backend == "vz" {
        // VZ writes the guest console itself (spec); stderr catches worker errors.
        let log = std::fs::File::create(dir.join("worker.log"))?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("vz-worker")
            .arg("--spec")
            .arg(spec_json)
            .arg("--control")
            .arg(dir.join("vmm.sock"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log);
        if let Some(state) = restore {
            cmd.arg("--restore").arg(state);
        }
        cmd.spawn()?
    } else {
        let console = std::fs::File::create(dir.join("console.log"))?;
        std::process::Command::new(&exe)
            .arg("krun-worker")
            .arg("--spec")
            .arg(spec_json)
            .stdin(std::process::Stdio::null())
            .stdout(console)
            .stderr(std::process::Stdio::null())
            .spawn()?
    };
    std::fs::write(dir.join("pid"), child.id().to_string())?;
    std::mem::forget(child); // vessel outlives this CLI invocation

    // Wait for the agent socket to answer.
    let t0 = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(AgentResponse::Health(h)) = agent_request(&dir, &AgentRequest::Health) {
            println!(
                "vessel `{name}` {} in {:?} (kernel {}, agent v{})",
                if restore.is_some() { "resumed" } else { "up" },
                t0.elapsed(),
                h.kernel,
                h.agent_version
            );
            println!("  shell: nebula vessels shell {name}");
            return Ok(());
        }
        if live_pid(&dir).is_none() && t0.elapsed() > Duration::from_millis(500) {
            bail!(
                "vessel `{name}` worker exited — see {}",
                dir.join(if backend == "vz" {
                    "worker.log"
                } else {
                    "console.log"
                })
                .display()
            );
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

/// One connection to a vz-worker's control socket. Sequential ops share the
/// stream, so pause -> save -> resume cannot be interleaved by another client.
struct VmmCtl {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl VmmCtl {
    fn connect(dir: &std::path::Path) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(dir.join("vmm.sock"))
            .context("vessel has no live vz-worker control socket")?;
        stream.set_read_timeout(Some(Duration::from_secs(120)))?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
        })
    }

    fn op(&mut self, req: &nebula_core::backend::vz::WorkerControl) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut resp = String::new();
        self.reader.read_line(&mut resp)?;
        let reply: nebula_core::backend::vz::WorkerReply =
            serde_json::from_str(resp.trim()).context("bad reply from vz-worker")?;
        anyhow::ensure!(
            reply.ok,
            "{}",
            reply.error.unwrap_or_else(|| "vmm operation failed".into())
        );
        Ok(())
    }
}

fn random_mac() -> anyhow::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 5];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    // 0x02: locally administered, unicast.
    Ok(format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0], b[1], b[2], b[3], b[4]
    ))
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

/// Restore a vessel's rootfs from the pristine image (data disk kept unless
/// wipe_data). The fix for "I shelled in and broke something".
pub fn reset(name: &str, wipe_data: bool) -> anyhow::Result<()> {
    let home = client::nebula_home()?;
    let pristine = home.join("images/rootfs-pristine.img");
    anyhow::ensure!(
        pristine.is_file(),
        "no pristine image at {} — run `nebula install-image` once",
        pristine.display()
    );

    if is_engine(name) {
        let was_running = client::daemon_running();
        if was_running {
            println!("stopping the engine…");
            crate::commands::down(false)?;
        }
        let live = home.join("disks/rootfs.img");
        let _ = std::fs::remove_file(&live);
        clone_file(&pristine, &live)?;
        if wipe_data {
            let _ = std::fs::remove_file(home.join("disks/data.img"));
            println!("data disk wiped (containers/images/k8s state gone)");
        }
        println!("engine rootfs restored to pristine");
        if was_running {
            return crate::commands::up();
        }
        return Ok(());
    }

    let dir = dir_of(name)?;
    anyhow::ensure!(dir.exists(), "no vessel named `{name}`");
    let was_running = live_pid(&dir).is_some();
    if was_running {
        stop(name)?;
    }
    let live = dir.join("rootfs.img");
    let _ = std::fs::remove_file(&live);
    clone_file(&pristine, &live)?;
    if wipe_data {
        let spec = read_spec(&dir)?;
        let size = std::fs::metadata(dir.join("data.img"))
            .map(|m| m.len())
            .unwrap_or(0);
        let _ = std::fs::remove_file(dir.join("data.img"));
        let f = std::fs::File::create(dir.join("data.img"))?;
        f.set_len(size)?;
        let _ = spec; // sizes live on disk, spec unchanged
        println!("data disk wiped");
    }
    println!("vessel `{name}` rootfs restored to pristine");
    if was_running {
        return start(name);
    }
    Ok(())
}

/// Build a vessel rootfs from a docker image, inside the engine vessel.
/// Output lands in `dir` via the $HOME virtiofs share.
fn build_rootfs_from_image(
    image: &str,
    name: &str,
    dir: &std::path::Path,
    rootfs_mb: u64,
    data_gib: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        client::daemon_running(),
        "building from an image needs the engine: nebula up"
    );
    let host_home = std::env::var("HOME").context("HOME not set")?;
    let stage_host = std::path::PathBuf::from(&host_home)
        .join(".nebula-image-build")
        .join(name);
    std::fs::create_dir_all(&stage_host)?;

    // Pull + export on the HOST (resolved docker CLI against the engine
    // socket — the slim guest image carries no docker CLI), then do the
    // filesystem work inside the engine where ext4 tooling lives.
    println!("building rootfs from docker image `{image}`…");
    let docker = crate::wrap::resolve_tool("docker");
    let sock = client::nebula_home()?.join("run/docker.sock");
    let docker_env = format!("unix://{}", sock.display());
    let run_docker = |args: &[&str]| -> anyhow::Result<std::process::Output> {
        Ok(std::process::Command::new(&docker)
            .env("DOCKER_HOST", &docker_env)
            .args(args)
            .output()?)
    };
    // Local images (docker build artifacts, loaded tarballs) work without a
    // registry: only pull when the engine doesn't already have the ref.
    let local = run_docker(&["image", "inspect", image])?.status.success();
    if !local {
        let pull = run_docker(&["pull", "-q", image])?;
        anyhow::ensure!(
            pull.status.success(),
            "image `{image}` is not in the engine and could not be pulled: {}",
            String::from_utf8_lossy(&pull.stderr)
        );
    }
    let create = run_docker(&["create", image, "/bin/true"])?;
    anyhow::ensure!(
        create.status.success(),
        "docker create failed: {}",
        String::from_utf8_lossy(&create.stderr)
    );
    let cid = String::from_utf8_lossy(&create.stdout).trim().to_string();
    let export_tar = stage_host.join("export.tar");
    let export = std::process::Command::new(&docker)
        .env("DOCKER_HOST", &docker_env)
        .args(["export", "-o"])
        .arg(&export_tar)
        .arg(&cid)
        .status()?;
    let _ = run_docker(&["rm", &cid]);
    anyhow::ensure!(export.success(), "docker export failed");

    let script = format!(
        r#"set -e
STAGE='{stage}'; SIZE_MB={rootfs_mb}; DATA_MB={data_mb}
BUILD=/var/lib/nebula/img-build-{name}
rm -rf "$BUILD"; mkdir -p "$BUILD/root"
tar -xf "$STAGE/export.tar" -C "$BUILD/root"
# Inject Nebula's static guest binaries so any image boots managed.
cp /sbin/nebula-init "$BUILD/root/sbin/nebula-init"
cp /usr/bin/vessel-agent "$BUILD/root/usr/bin/vessel-agent" 2>/dev/null || {{ mkdir -p "$BUILD/root/usr/bin"; cp /usr/bin/vessel-agent "$BUILD/root/usr/bin/vessel-agent"; }}
mkdir -p "$BUILD/root/var/lib/nebula" "$BUILD/root/run" "$BUILD/root/tmp" "$BUILD/root/proc" "$BUILD/root/sys" "$BUILD/root/dev"
truncate -s ${{SIZE_MB}}M "$BUILD/rootfs.img"
mkfs.ext4 -q -L nebula-root -d "$BUILD/root" "$BUILD/rootfs.img"
# Pre-format the data disk here too: foreign images may lack e2fsprogs.
truncate -s ${{DATA_MB}}M "$BUILD/data.img"
mkfs.ext4 -q -L nebula-data "$BUILD/data.img"
mv "$BUILD/rootfs.img" "$STAGE/rootfs.img"
mv "$BUILD/data.img" "$STAGE/data.img"
rm -rf "$BUILD"
"#,
        stage = stage_host.display(),
        data_mb = data_gib * 1024,
    );
    // (export.tar consumed in-guest; removed with the stage dir below)
    let r = engine_exec_long(&script)?;
    if r.exit_code != 0 {
        let _ = std::fs::remove_dir_all(&stage_host);
        bail!(
            "image build failed:
{}{}",
            r.stdout,
            r.stderr
        );
    }
    // Move (or copy across volumes) into the vessel dir.
    for f in ["rootfs.img", "data.img"] {
        let src = stage_host.join(f);
        let dst = dir.join(f);
        if std::fs::rename(&src, &dst).is_err() {
            std::fs::copy(&src, &dst)?;
            let _ = std::fs::remove_file(&src);
        }
    }
    let _ = std::fs::remove_dir_all(&stage_host);
    Ok(())
}

/// Exec in the ENGINE vessel with a long timeout (image pulls can be slow).
fn engine_exec_long(script: &str) -> anyhow::Result<ExecResult> {
    let req = DaemonRequest::Agent {
        request: AgentRequest::Exec {
            cmd: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            timeout_ms: 900_000,
        },
    };
    match client::request(&req)? {
        DaemonResponse::Agent {
            response: AgentResponse::Exec(r),
        } => Ok(r),
        DaemonResponse::Agent {
            response: AgentResponse::Error { message },
        } => bail!("{message}"),
        DaemonResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

fn clone_file(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("cp")
        .arg("-c")
        .arg(from)
        .arg(to)
        .status()?;
    if !status.success() {
        // clonefile only works within one APFS volume; fall back to a copy
        // (e.g. --rootfs-img sources on another disk).
        std::fs::copy(from, to)
            .with_context(|| format!("clone/copy {} -> {} failed", from.display(), to.display()))?;
    }
    Ok(())
}

/// Create a vessel data disk. Foreign rootfs images may lack e2fsprogs (so
/// the guest can't format it itself); when the engine is up, pre-format
/// host-side through the $HOME share like --from-image does.
fn create_data_disk(dir: &std::path::Path, data_gib: u64) -> anyhow::Result<()> {
    if client::daemon_running() {
        let host_home = std::env::var("HOME").context("HOME not set")?;
        let stage = std::path::PathBuf::from(&host_home)
            .join(".nebula-image-build")
            .join(format!("data-{}", std::process::id()));
        std::fs::create_dir_all(&stage)?;
        let script = format!(
            "set -e\ntruncate -s {}M '{stage}/data.img'\nmkfs.ext4 -q -L nebula-data '{stage}/data.img'\n",
            data_gib * 1024,
            stage = stage.display()
        );
        let formatted = engine_exec_long(&script).map(|r| r.exit_code == 0);
        if let Ok(true) = formatted {
            let dst = dir.join("data.img");
            if std::fs::rename(stage.join("data.img"), &dst).is_err() {
                std::fs::copy(stage.join("data.img"), &dst)?;
            }
            let _ = std::fs::remove_dir_all(&stage);
            return Ok(());
        }
        let _ = std::fs::remove_dir_all(&stage);
    }
    eprintln!(
        "note: data disk left unformatted (engine not running) — the guest \
         formats it on first boot if it has e2fsprogs"
    );
    let data = std::fs::File::create(dir.join("data.img"))?;
    data.set_len(data_gib * 1024 * 1024 * 1024)?;
    Ok(())
}

/// `nebula vessels convert-image <ref> --out <file>`: produce a bootable
/// vessel rootfs from a docker image (local or remote) WITHOUT creating a
/// vessel. The output is what `vessels new --rootfs-img` and embed kits
/// consume — apps ship it and create vessels offline, engine-free.
pub fn convert_image(image: &str, out: &std::path::Path, rootfs_mb: u64) -> anyhow::Result<()> {
    let tmp_name = format!("convert-{}", std::process::id());
    let tmp = std::env::temp_dir().join(format!("nebula-{tmp_name}"));
    std::fs::create_dir_all(&tmp)?;
    // 1 GiB throwaway data disk: the builder formats one; we discard it.
    let built = build_rootfs_from_image(image, &tmp_name, &tmp, rootfs_mb, 1);
    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let _ = std::fs::remove_file(out);
    if std::fs::rename(tmp.join("rootfs.img"), out).is_err() {
        std::fs::copy(tmp.join("rootfs.img"), out)?;
    }
    let _ = std::fs::remove_dir_all(&tmp);
    let mb = std::fs::metadata(out)?.len() / (1024 * 1024);
    println!(
        "converted `{image}` -> {} ({mb} MiB sparse; gzip for distribution)",
        out.display()
    );
    Ok(())
}

fn snap_dir(dir: &std::path::Path, label: &str) -> PathBuf {
    dir.join("snapshots").join(label)
}

fn validate_label(label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
        "snapshot labels must be [a-zA-Z0-9._-], got `{label}`"
    );
    Ok(())
}

/// How much of a vessel a snapshot captures.
#[derive(PartialEq, Clone, Copy)]
pub enum SnapMode {
    /// Memory + disks when possible (running vz vessel), else disks only.
    Auto,
    /// Memory + disks, or fail.
    Memory,
    /// Disks only.
    DiskOnly,
}

/// Snapshot a vessel.
///
/// Memory mode (default on running vz vessels): pause -> save machine state
/// -> clone disks -> resume. The guest never stops; restore resumes
/// mid-execution with running processes, open sockets, and RAM (tmpfs etc.)
/// intact.
///
/// Disk mode: stop (if running), APFS-clone both disks, restart — crash-
/// consistent, ~0.2s stop + ~10ms clone + ~0.1s restart.
pub fn snapshot(name: &str, label: &str, mode: SnapMode) -> anyhow::Result<()> {
    validate_label(label)?;
    let dir = dir_of(name)?;
    anyhow::ensure!(dir.exists(), "no vessel named `{name}`");
    let sdir = snap_dir(&dir, label);
    anyhow::ensure!(!sdir.exists(), "snapshot `{label}` already exists");

    let spec = read_spec(&dir)?;
    let is_vz = spec.backend.as_deref() == Some("vz");
    let running = live_pid(&dir).is_some();
    let memory = match mode {
        SnapMode::DiskOnly => false,
        SnapMode::Memory => {
            anyhow::ensure!(
                is_vz,
                "memory snapshots need a vz vessel (nebula vessels new {name} --backend vz); \
                 this one runs on {}",
                spec.backend.as_deref().unwrap_or("krun")
            );
            anyhow::ensure!(
                running,
                "vessel `{name}` is not running — a memory snapshot captures a LIVE vm \
                 (for a stopped vessel take a disk snapshot: --no-memory)"
            );
            true
        }
        SnapMode::Auto => {
            if !is_vz {
                println!(
                    "note: disk-only snapshot — live memory capture needs a vz vessel \
                     (nebula vessels new <name> --backend vz)"
                );
            } else if !running {
                println!(
                    "note: vessel is stopped — disk-only snapshot (start it to capture memory)"
                );
            }
            is_vz && running
        }
    };

    if memory {
        std::fs::create_dir_all(&sdir)?;
        let t0 = Instant::now();
        let mut ctl = VmmCtl::connect(&dir)?;
        use nebula_core::backend::vz::WorkerControl;
        ctl.op(&WorkerControl::Pause)?;
        let saved = ctl
            .op(&WorkerControl::Save {
                path: sdir.join("memory.vzstate"),
            })
            // Disks cloned while still paused = consistent with the state file.
            .and_then(|()| clone_file(&dir.join("rootfs.img"), &sdir.join("rootfs.img")))
            .and_then(|()| {
                if dir.join("data.img").is_file() {
                    clone_file(&dir.join("data.img"), &sdir.join("data.img"))?;
                }
                Ok(())
            });
        let resumed = ctl.op(&WorkerControl::Resume);
        if let Err(e) = saved {
            let _ = std::fs::remove_dir_all(&sdir);
            return Err(e.context("memory snapshot failed (vessel resumed unharmed)"));
        }
        resumed?;
        let state_mb = std::fs::metadata(sdir.join("memory.vzstate"))
            .map(|m| m.len() / (1024 * 1024))
            .unwrap_or(0);
        println!(
            "memory snapshot `{name}@{label}` taken in {:.2?} ({state_mb} MiB state, vessel never stopped)",
            t0.elapsed()
        );
        return Ok(());
    }

    if running {
        stop(name)?;
    }
    std::fs::create_dir_all(&sdir)?;
    let t0 = Instant::now();
    clone_file(&dir.join("rootfs.img"), &sdir.join("rootfs.img"))?;
    if dir.join("data.img").is_file() {
        clone_file(&dir.join("data.img"), &sdir.join("data.img"))?;
    }
    println!("snapshot `{name}@{label}` taken in {:.0?}", t0.elapsed());
    if running {
        start(name)?;
    }
    Ok(())
}

pub fn snapshots(name: &str) -> anyhow::Result<()> {
    let dir = dir_of(name)?;
    let root = dir.join("snapshots");
    let mut labels: Vec<String> = match std::fs::read_dir(&root) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => vec![],
    };
    labels.sort();
    if labels.is_empty() {
        println!("no snapshots for `{name}` (create one: nebula vessels snapshot {name} <label>)");
        return Ok(());
    }
    for l in labels {
        if root.join(&l).join("memory.vzstate").is_file() {
            println!("{name}@{l}  (disks + memory state)");
        } else {
            println!("{name}@{l}");
        }
    }
    Ok(())
}

pub fn snapshot_rm(name: &str, label: &str) -> anyhow::Result<()> {
    validate_label(label)?;
    let dir = dir_of(name)?;
    let sdir = snap_dir(&dir, label);
    anyhow::ensure!(sdir.exists(), "no snapshot `{name}@{label}`");
    std::fs::remove_dir_all(&sdir)?;
    println!("removed snapshot `{name}@{label}`");
    Ok(())
}

/// Roll a vessel back to a snapshot (its current disks are replaced). When
/// the snapshot carries machine state, the vessel RESUMES mid-execution.
pub fn restore(name: &str, label: &str) -> anyhow::Result<()> {
    validate_label(label)?;
    let dir = dir_of(name)?;
    let sdir = snap_dir(&dir, label);
    anyhow::ensure!(sdir.exists(), "no snapshot `{name}@{label}`");
    let memory_state = sdir.join("memory.vzstate");
    let has_memory = memory_state.is_file();
    let was_running = live_pid(&dir).is_some();
    if was_running {
        stop(name)?;
    }
    for img in ["rootfs.img", "data.img"] {
        if sdir.join(img).is_file() {
            let _ = std::fs::remove_file(dir.join(img));
            clone_file(&sdir.join(img), &dir.join(img))?;
        }
    }
    if has_memory {
        match start_with(name, Some(&memory_state)) {
            Ok(()) => {
                println!("`{name}` restored to @{label} (live resume — processes/RAM intact)");
                return Ok(());
            }
            Err(e) => {
                // Disks were cloned while paused, so a cold boot of them is
                // crash-consistent — degrade instead of leaving it dead.
                eprintln!(
                    "memory-state resume failed ({e}); cold-booting the restored disks instead"
                );
                return start(name);
            }
        }
    }
    println!("`{name}` restored to @{label}");
    if was_running {
        start(name)?;
    }
    Ok(())
}

/// Branch new vessel(s) from a snapshot (or from the current state when no
/// label is given). With --count N this is the tree-search fan-out: N clones,
/// each booted, each fully independent.
pub fn branch(name: &str, new_name: &str, label: Option<&str>, count: u32) -> anyhow::Result<()> {
    let dir = dir_of(name)?;
    anyhow::ensure!(dir.exists(), "no vessel named `{name}`");
    let spec = read_spec(&dir)?;

    // Branch source: a snapshot, or a transient clone of the current state.
    let (src_root, src_data, _tmp_guard);
    let mut src_state: Option<PathBuf> = None;
    match label {
        Some(l) => {
            validate_label(l)?;
            let sdir = snap_dir(&dir, l);
            anyhow::ensure!(sdir.exists(), "no snapshot `{name}@{l}`");
            src_root = sdir.join("rootfs.img");
            src_data = sdir.join("data.img");
            // Memory snapshots fan out as LIVE resumes: every branch wakes
            // mid-execution at the exact saved instant.
            let state = sdir.join("memory.vzstate");
            if state.is_file() {
                src_state = Some(state);
            }
            _tmp_guard = None::<tempdir::Guard>;
        }
        None => {
            let was_running = live_pid(&dir).is_some();
            if was_running {
                stop(name)?;
            }
            let tmp = dir.join(".branch-src");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            clone_file(&dir.join("rootfs.img"), &tmp.join("rootfs.img"))?;
            if dir.join("data.img").is_file() {
                clone_file(&dir.join("data.img"), &tmp.join("data.img"))?;
            }
            if was_running {
                start(name)?;
            }
            src_root = tmp.join("rootfs.img");
            src_data = tmp.join("data.img");
            _tmp_guard = Some(tempdir::Guard(tmp));
        }
    }

    let t0 = Instant::now();
    let names: Vec<String> = if count <= 1 {
        vec![new_name.to_string()]
    } else {
        (1..=count).map(|i| format!("{new_name}-{i}")).collect()
    };
    for n in &names {
        validate_name(n)?;
        let ndir = vessels_root()?.join(n);
        anyhow::ensure!(!ndir.exists(), "vessel `{n}` already exists");
        std::fs::create_dir_all(&ndir)?;
        clone_file(&src_root, &ndir.join("rootfs.img"))?;
        if src_data.is_file() {
            clone_file(&src_data, &ndir.join("data.img"))?;
        }
        let mut nspec = spec.clone();
        nspec.name = format!("vessel-{n}");
        retarget_spec(&mut nspec, &ndir);
        if src_state.is_none() && nspec.backend.as_deref() == Some("vz") {
            // Cold-booted branches get their own identity. Memory resumes
            // must keep the saved config (MAC + machine id) — those branches
            // share the source's network identity (vsock control unaffected).
            nspec.mac = Some(random_mac()?);
            nspec.machine_id = Some(nebula_core::backend::vz::new_machine_id());
        }
        std::fs::write(ndir.join("spec.json"), serde_json::to_vec_pretty(&nspec)?)?;
        match &src_state {
            Some(state) => {
                if let Err(e) = start_with(n, Some(state)) {
                    eprintln!(
                        "branch `{n}`: live resume failed ({e}); cold-booting its disks instead"
                    );
                    start(n)?;
                }
            }
            None => start(n)?,
        }
    }
    println!(
        "branched {} vessel(s) from `{name}{}` in {:.2?}{}",
        names.len(),
        label.map(|l| format!("@{l}")).unwrap_or_default(),
        t0.elapsed(),
        if src_state.is_some() {
            " (live resume — each woke mid-execution)"
        } else {
            ""
        }
    );
    Ok(())
}

/// Point a cloned spec's paths (disks, console, vsock sockets) at its own dir.
fn retarget_spec(spec: &mut VmSpec, dir: &std::path::Path) {
    for d in &mut spec.disks {
        if let Some(fname) = d.path.file_name() {
            d.path = dir.join(fname);
        }
    }
    if let nebula_core::ConsoleSpec::File(p) = &mut spec.console {
        if let Some(fname) = p.file_name() {
            *p = dir.join(fname);
        }
    }
    for m in &mut spec.vsock_ports {
        if let Some(fname) = m.host_path.file_name() {
            m.host_path = dir.join(fname);
        }
    }
}

mod tempdir {
    pub struct Guard(pub std::path::PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
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
    if is_engine(name) {
        println!("vessel:   vessel (engine — docker/kubernetes)");
        println!("managed:  nebula up / nebula down / nebula status");
        return crate::commands::status();
    }
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
    println!("backend:  {}", spec.backend.as_deref().unwrap_or("krun"));
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
    if is_engine(name) {
        return crate::commands::exec(cmd);
    }
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
    if is_engine(name) {
        return crate::commands::shell();
    }
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
