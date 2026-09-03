//! Named-vessel lifecycle — the one implementation shared by the CLI and
//! nebulad's REST API (the embedding surface). Logic only: functions return
//! outcome data and never print; callers own presentation.
//!
//! Layout: ~/.nebula/vessels/<name>/{spec.json,pid,rootfs.img,data.img,
//! console.log,agent.sock,shell.sock,vmm.sock,worker.log,snapshots/}

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};

use crate::detach;
use crate::ipc::{self, IpcStream};
use crate::proto::*;
use crate::spec::{BootSpec, ConsoleSpec, DiskSpec, NetSpec, ShareSpec, VmSpec, VsockPortMap};

/// Names that refer to the engine vessel (docker/k8s). The engine is owned
/// by `nebula up`/`down`; vessel ops refuse it.
pub const RESERVED: &[&str] = &["vessel", "default", "engine", "nebula"];

pub fn is_engine(name: &str) -> bool {
    RESERVED.contains(&name)
}

// --- paths & validation -----------------------------------------------------

pub fn vessels_root() -> anyhow::Result<PathBuf> {
    Ok(crate::home::nebula_home()?.join("vessels"))
}

pub fn dir_of(name: &str) -> anyhow::Result<PathBuf> {
    validate_name(name)?;
    Ok(vessels_root()?.join(name))
}

pub fn validate_name(name: &str) -> anyhow::Result<()> {
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

pub fn validate_label(label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
        "snapshot labels must be [a-zA-Z0-9._-], got `{label}`"
    );
    Ok(())
}

pub fn read_spec(dir: &Path) -> anyhow::Result<VmSpec> {
    let raw = std::fs::read_to_string(dir.join("spec.json"))
        .with_context(|| format!("no vessel at {}", dir.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

fn snap_dir(dir: &Path, label: &str) -> PathBuf {
    dir.join("snapshots").join(label)
}

/// Every disk image a vessel owns, in device order (rootfs=vda, data=vdb,
/// volumes from vdc) — the set snapshots/branches must clone.
pub fn disk_images(dir: &Path) -> Vec<String> {
    let mut v = vec!["rootfs.img".to_string(), "data.img".to_string()];
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut vols: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.starts_with("vol-") && n.ends_with(".img"))
            .collect();
        vols.sort();
        v.extend(vols);
    }
    v
}

/// Parse `--volume name:GiB` specs. Names become mount points (/mnt/<name>)
/// and disk files (vol-<name>.img), so they're strictly validated.
/// Parse `--mount /host/path[:ro]` into (canonical path, read_only).
///
/// The guest mounts each share at the *same absolute path* it has on the host,
/// which is the contract the engine vessel's `$HOME` share already uses — a
/// path that works on one side works verbatim on the other.
pub fn parse_mounts(specs: &[String]) -> anyhow::Result<Vec<(PathBuf, bool)>> {
    let mut out: Vec<(PathBuf, bool)> = Vec::new();
    for s in specs {
        let (raw, read_only) = match s.rsplit_once(':') {
            Some((head, "ro")) => (head, true),
            Some((head, "rw")) => (head, false),
            _ => (s.as_str(), false),
        };
        let path = Path::new(raw)
            .canonicalize()
            .with_context(|| format!("--mount path does not exist: `{raw}`"))?;
        anyhow::ensure!(
            path.is_dir(),
            "--mount wants a directory, got `{}`",
            path.display()
        );
        if !out.iter().any(|(p, _)| p == &path) {
            out.push((path, read_only));
        }
    }
    Ok(out)
}

pub fn parse_volumes(specs: &[String]) -> anyhow::Result<Vec<(String, u64)>> {
    let mut out: Vec<(String, u64)> = Vec::new();
    for s in specs {
        let (name, size) = s
            .split_once(':')
            .with_context(|| format!("--volume wants name:GiB, got `{s}`"))?;
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 32
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "volume names must be [a-z0-9_-] (max 32 chars), got `{name}`"
        );
        anyhow::ensure!(
            name != "data",
            "`data` is the built-in data disk (size it with --disk)"
        );
        anyhow::ensure!(
            !out.iter().any(|(n, _)| n == name),
            "duplicate volume `{name}`"
        );
        let gib: u64 = size
            .parse()
            .with_context(|| format!("volume `{name}`: bad size `{size}` (GiB)"))?;
        anyhow::ensure!(
            (1..=2048).contains(&gib),
            "volume `{name}`: size must be 1..=2048 GiB"
        );
        out.push((name.to_string(), gib));
    }
    anyhow::ensure!(out.len() <= 8, "at most 8 extra volumes per vessel");
    Ok(out)
}

// --- process liveness --------------------------------------------------------

/// Portable "is this pid alive" / "kill it" (vessel workers).
fn pid_alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

fn pid_kill(pid: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
}

pub fn live_pid(dir: &Path) -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(dir.join("pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    pid_alive(pid).then_some(pid)
}

// --- file plumbing ------------------------------------------------------------

pub fn clone_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    // APFS clonefile on macOS; reflink (btrfs/XFS) on Linux. Both are
    // near-free and share the source's extents, holes included.
    let cloned = if cfg!(windows) {
        false
    } else {
        let flag = if cfg!(target_os = "macos") {
            "-c"
        } else {
            "--reflink=auto"
        };
        std::process::Command::new("cp")
            .arg(flag)
            .arg(from)
            .arg(to)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !cloned {
        // No reflink here — NTFS has none, and `cp` is absent on Windows
        // entirely. A dense copy would write every zero of a mostly-empty
        // disk image, which is what made `install-image` and
        // `vessels reset` slow enough to notice (issue #24).
        crate::sparse::copy_sparse(from, to)
            .with_context(|| format!("clone/copy {} -> {} failed", from.display(), to.display()))?;
    }
    Ok(())
}

/// On-disk size of a snapshot state file in MiB (sparse-aware where the
/// platform can say — krun memory images are mostly holes). Falls back to the
/// logical size on Windows, which reports no allocation figure.
fn physical_size_mb(path: &Path) -> u64 {
    crate::sparse::physical_bytes(path)
        .or_else(|| std::fs::metadata(path).ok().map(|m| m.len()))
        .unwrap_or(0)
        / (1024 * 1024)
}

// --- vz identity helpers -------------------------------------------------------

#[cfg(target_os = "macos")]
fn vz_machine_id() -> Option<String> {
    Some(crate::backend::vz::new_machine_id())
}
#[cfg(not(target_os = "macos"))]
fn vz_machine_id() -> Option<String> {
    None
}

pub fn random_mac() -> anyhow::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 5];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    // 0x02: locally administered, unicast.
    Ok(format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0], b[1], b[2], b[3], b[4]
    ))
}

// --- agent & worker control ----------------------------------------------------

pub fn agent_request(dir: &Path, req: &AgentRequest) -> anyhow::Result<AgentResponse> {
    let stream = ipc::connect(&dir.join("agent.sock"))?;
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

/// One connection to a VM worker's control socket (vz-worker or krun-worker;
/// same JSON protocol). Sequential ops share the stream, so pause -> save ->
/// resume cannot be interleaved by another client.
struct VmmCtl {
    reader: BufReader<IpcStream>,
    writer: IpcStream,
}

impl VmmCtl {
    fn connect(dir: &Path) -> anyhow::Result<Self> {
        let stream = ipc::connect(&dir.join("vmm.sock"))
            .context("vessel has no live worker control socket")?;
        stream.set_read_timeout(Some(Duration::from_secs(120)))?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
        })
    }

    fn op(&mut self, req: &crate::backend::WorkerControl) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut resp = String::new();
        self.reader.read_line(&mut resp)?;
        let reply: crate::backend::WorkerReply =
            serde_json::from_str(resp.trim()).context("bad reply from worker")?;
        anyhow::ensure!(
            reply.ok,
            "{}",
            reply.error.unwrap_or_else(|| "worker op failed".into())
        );
        Ok(())
    }
}

// --- create ----------------------------------------------------------------------

/// Vessel creation parameters (validated here, not at the CLI edge).
pub struct CreateOpts {
    pub name: String,
    pub cpus: u32,
    pub mem: u64,
    pub gpu: bool,
    pub data_gib: u64,
    /// `krun` | `vz`.
    pub backend: String,
    /// Extra persistent volumes: (name, GiB), mounted at /mnt/<name>.
    pub volumes: Vec<(String, u64)>,
    /// Host directories shared into the guest at their identical absolute
    /// paths: (path, read_only).
    pub mounts: Vec<(PathBuf, bool)>,
}

/// Where the rootfs comes from.
pub enum Rootfs {
    /// CoW-clone the installed base image; sparse data disk (guest formats).
    BaseImage,
    /// rootfs.img (and possibly data.img) were already placed in the vessel
    /// dir by the caller (--from-image / --rootfs-img flows).
    Prepared,
}

/// Create the vessel directory, disks and spec.json. Does NOT start it.
pub fn create(opts: &CreateOpts, rootfs: Rootfs) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        opts.backend == "krun" || opts.backend == "vz",
        "--backend must be `krun` or `vz`, got `{}`",
        opts.backend
    );
    anyhow::ensure!(
        opts.backend != "vz" || cfg!(target_os = "macos"),
        "--backend vz is Virtualization.framework (macOS-only) — use krun on Linux"
    );
    anyhow::ensure!(
        !(opts.gpu && opts.backend == "vz"),
        "GPU passthrough (Venus) is libkrun-only — drop --backend vz or --gpu"
    );
    let dir = dir_of(&opts.name)?;

    let home = crate::home::nebula_home()?;
    let kernel = home.join("kernel/Image");
    anyhow::ensure!(
        kernel.is_file(),
        "guest kernel missing — run `nebula up` once first"
    );

    std::fs::create_dir_all(&dir)?;
    match rootfs {
        Rootfs::BaseImage => {
            // Clone from the pristine store, not the engine's live disk.
            let base_rootfs = if home.join("images/rootfs-pristine.img").is_file() {
                home.join("images/rootfs-pristine.img")
            } else {
                home.join("disks/rootfs.img")
            };
            anyhow::ensure!(
                base_rootfs.is_file(),
                "guest images missing — run `nebula up` once first"
            );
            // APFS/reflink copy-on-write: instant and space-shared.
            clone_file(&base_rootfs, &dir.join("rootfs.img"))?;
            let data = std::fs::File::create(dir.join("data.img"))?;
            data.set_len(opts.data_gib * 1024 * 1024 * 1024)?;
        }
        Rootfs::Prepared => {
            anyhow::ensure!(
                dir.join("rootfs.img").is_file(),
                "prepared rootfs.img missing in {}",
                dir.display()
            );
            if !dir.join("data.img").is_file() {
                let data = std::fs::File::create(dir.join("data.img"))?;
                data.set_len(opts.data_gib * 1024 * 1024 * 1024)?;
            }
        }
    }

    // Extra volumes: sparse files; the guest formats + mounts them by name.
    let mut disks = vec![
        DiskSpec {
            path: dir.join("rootfs.img"),
            read_only: false,
        },
        DiskSpec {
            path: dir.join("data.img"),
            read_only: false,
        },
    ];
    for (vname, gib) in &opts.volumes {
        let path = dir.join(format!("vol-{vname}.img"));
        let f = std::fs::File::create(&path)?;
        f.set_len(gib * 1024 * 1024 * 1024)?;
        disks.push(DiskSpec {
            path,
            read_only: false,
        });
    }
    let mut cmdline = String::from(
        "console=hvc0 root=/dev/vda rw rootfstype=ext4 init=/sbin/nebula-init reboot=k panic=10 NEBULA_AGENT_ONLY=1",
    );
    if !opts.volumes.is_empty() {
        cmdline.push_str(" NEBULA_VOLUMES=");
        cmdline.push_str(
            &opts
                .volumes
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(","),
        );
    }

    // Same contract as the engine vessel: hand vessel-init a tag→path map and
    // let it mount each share at the identical absolute path.
    let shares: Vec<ShareSpec> = opts
        .mounts
        .iter()
        .enumerate()
        .map(|(n, (path, read_only))| ShareSpec {
            tag: format!("mount{n}"),
            host_path: path.clone(),
            read_only: *read_only,
        })
        .collect();
    if !shares.is_empty() {
        cmdline.push_str(" NEBULA_SHARES=");
        cmdline.push_str(
            &shares
                .iter()
                .map(|s| {
                    format!(
                        "{}={}{}",
                        s.tag,
                        s.host_path
                            .to_string_lossy()
                            .as_bytes()
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>(),
                        if s.read_only { ":ro" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
        );
    }

    let vz = opts.backend == "vz";
    let spec = VmSpec {
        name: format!("vessel-{}", opts.name),
        cpus: opts.cpus,
        mem_mib: opts.mem.max(1024),
        boot: BootSpec::Kernel {
            kernel,
            initramfs: None,
            cmdline,
        },
        disks,
        shares,
        // NICs everywhere: VZ NAT on vz; the fork's in-process usernet NAT
        // on krun (TSI never applied to our own-init disk boots).
        net: NetSpec::Nat,
        vsock: vz, // VZ needs the device for the worker's socket proxies
        console: ConsoleSpec::File(dir.join("console.log")),
        balloon: false,
        rng: true,
        rosetta: false,
        gpu: opts.gpu,
        // krun workers serve pause/save/resume here (vz uses its own
        // vmm.sock wired by the vz-worker itself).
        control_path: Some(dir.join("vmm.sock")),
        restore_path: None,
        vsock_ports: vec![
            VsockPortMap {
                port: VSOCK_PORT_CONTROL,
                host_path: dir.join("agent.sock"),
            },
            VsockPortMap {
                port: VSOCK_PORT_SHELL,
                host_path: dir.join("shell.sock"),
            },
            // Framebuffer + input for `nebula vessels display`. Mapped
            // unconditionally: the guest side only binds it when the rootfs
            // ships vessel-display, and an unbound port costs nothing.
            VsockPortMap {
                port: crate::display::VSOCK_PORT_DISPLAY,
                host_path: dir.join("display.sock"),
            },
        ],
        backend: Some(opts.backend.clone()),
        // Stable MAC + machine id: keep the DHCP lease across restarts and
        // keep the config identical to any saved machine state.
        mac: if vz { Some(random_mac()?) } else { None },
        machine_id: if vz { vz_machine_id() } else { None },
    };
    std::fs::write(dir.join("spec.json"), serde_json::to_vec_pretty(&spec)?)?;
    Ok(dir)
}

// --- start / stop / rm -------------------------------------------------------------

/// A vessel that just came up (or resumed).
#[derive(Debug, serde::Serialize)]
pub struct Started {
    pub resumed: bool,
    pub boot_ms: u64,
    pub kernel: String,
    pub agent_version: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum StartOutcome {
    AlreadyRunning,
    Started(Started),
}

pub fn start(name: &str) -> anyhow::Result<StartOutcome> {
    start_with(name, None)
}

/// Boot a vessel; with `restore` the worker resumes from a saved machine
/// state (vz: a memory.vzstate file; krun on x86_64 Linux/Windows: a
/// memory.krun snapshot directory) instead of cold-booting the kernel.
pub fn start_with(name: &str, restore: Option<&Path>) -> anyhow::Result<StartOutcome> {
    let dir = dir_of(name)?;
    let mut spec = read_spec(&dir)?;
    if live_pid(&dir).is_some() {
        return Ok(StartOutcome::AlreadyRunning);
    }
    let backend = spec.backend.clone().unwrap_or_else(|| "krun".into());
    let krun_restore_ok = cfg!(all(
        any(target_os = "linux", target_os = "windows"),
        target_arch = "x86_64"
    ));
    anyhow::ensure!(
        restore.is_none() || backend == "vz" || krun_restore_ok,
        "memory-state restore for krun vessels needs an x86_64 Linux or Windows host \
         (use vz on macOS)"
    );
    if backend != "vz" {
        // Transient: the worker reads it from its spec argument; the vessel's
        // spec.json on disk is untouched, so later starts cold-boot normally.
        spec.restore_path = restore.map(|p| p.to_path_buf());
    }

    // The worker outlives this call, so it must inherit nothing of ours: on
    // Windows a leaked pipe handle it never writes to still denies the reader
    // EOF, and `nebula vessels new x | ...` hangs until the vessel dies.
    // `detach` hands the child only the log files named here.
    let spec_json = serde_json::to_string(&spec)?;
    let exe = std::env::current_exe()?;
    let child = if backend == "vz" {
        // VZ writes the guest console itself (spec); stderr catches worker errors.
        let log = std::fs::File::create(dir.join("worker.log"))?;
        let mut cmd = detach::Detached::new(&exe)
            .arg("vz-worker")
            .arg("--spec")
            .arg(spec_json)
            .arg("--control")
            .arg(dir.join("vmm.sock"))
            .stderr(detach::Stdio::File(log));
        if let Some(state) = restore {
            cmd = cmd.arg("--restore").arg(state);
        }
        cmd.spawn()?
    } else {
        let console = std::fs::File::create(dir.join("console.log"))?;
        // stderr catches worker panics + fork log/trace output.
        let log = std::fs::File::create(dir.join("worker.log"))?;
        detach::Detached::new(&exe)
            .arg("krun-worker")
            .arg("--spec")
            .arg(spec_json)
            .stdout(detach::Stdio::File(console))
            .stderr(detach::Stdio::File(log))
            .spawn()?
    };
    std::fs::write(dir.join("pid"), child.id().to_string())?;
    drop(child); // vessel outlives this invocation; dropping neither kills nor reaps

    // Wait for the agent socket to answer.
    let t0 = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(AgentResponse::Health(h)) = agent_request(&dir, &AgentRequest::Health) {
            return Ok(StartOutcome::Started(Started {
                resumed: restore.is_some(),
                boot_ms: t0.elapsed().as_millis() as u64,
                kernel: h.kernel,
                agent_version: h.agent_version,
            }));
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

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopOutcome {
    NotRunning,
    Stopped,
    Forced,
}

pub fn stop(name: &str) -> anyhow::Result<StopOutcome> {
    let dir = dir_of(name)?;
    let Some(pid) = live_pid(&dir) else {
        return Ok(StopOutcome::NotRunning);
    };
    // Graceful first: agent powers the guest off and the worker exits.
    let _ = agent_request(&dir, &AgentRequest::Shutdown);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            let _ = std::fs::remove_file(dir.join("pid"));
            return Ok(StopOutcome::Stopped);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    pid_kill(pid);
    let _ = std::fs::remove_file(dir.join("pid"));
    Ok(StopOutcome::Forced)
}

/// Remove a vessel (its directory and all disks/snapshots). Running vessels
/// are refused unless `force`, which stops them first.
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
    Ok(())
}

// --- list / info ----------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct VesselSummary {
    pub name: String,
    pub running: bool,
    pub cpus: u32,
    pub mem_mib: u64,
    pub gpu: bool,
    pub backend: String,
}

/// All named vessels (the engine vessel is not included — it belongs to
/// `nebula up/down` and the daemon).
pub fn list() -> anyhow::Result<Vec<VesselSummary>> {
    let root = vessels_root()?;
    let mut out = Vec::new();
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
            out.push(VesselSummary {
                running: live_pid(&dir).is_some(),
                cpus: spec.cpus,
                mem_mib: spec.mem_mib,
                gpu: spec.gpu,
                backend: spec.backend.unwrap_or_else(|| "krun".into()),
                name,
            });
        }
    }
    Ok(out)
}

// --- snapshots --------------------------------------------------------------------

/// How much of a vessel a snapshot captures.
#[derive(PartialEq, Clone, Copy)]
pub enum SnapMode {
    /// Memory + disks when possible (running vessel, capable backend), else
    /// disks only.
    Auto,
    /// Memory + disks, or fail.
    Memory,
    /// Disks only.
    DiskOnly,
}

/// Why an Auto snapshot fell back to disks only.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskOnlyReason {
    Requested,
    BackendUnsupported,
    NotRunning,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SnapshotOutcome {
    /// Live capture: pause -> save state -> clone disks -> resume. The guest
    /// never stops.
    Memory { ms: u64, state_mb: u64 },
    /// Crash-consistent disk clone (vessel stopped + restarted around it
    /// when it was running).
    DiskOnly { ms: u64, reason: DiskOnlyReason },
}

pub fn snapshot(name: &str, label: &str, mode: SnapMode) -> anyhow::Result<SnapshotOutcome> {
    validate_label(label)?;
    let dir = dir_of(name)?;
    anyhow::ensure!(dir.exists(), "no vessel named `{name}`");
    let sdir = snap_dir(&dir, label);
    anyhow::ensure!(!sdir.exists(), "snapshot `{label}` already exists");

    let spec = read_spec(&dir)?;
    let is_vz = spec.backend.as_deref() == Some("vz");
    // Live memory capture: vz on macOS, krun on x86_64 Linux/Windows.
    let memory_capable = (is_vz && cfg!(target_os = "macos"))
        || (!is_vz
            && cfg!(all(
                any(target_os = "linux", target_os = "windows"),
                target_arch = "x86_64"
            )));
    let running = live_pid(&dir).is_some();
    let (memory, disk_reason) = match mode {
        SnapMode::DiskOnly => (false, DiskOnlyReason::Requested),
        SnapMode::Memory => {
            anyhow::ensure!(
                memory_capable,
                "memory snapshots aren't supported for {}-backed vessels on this platform yet",
                spec.backend.as_deref().unwrap_or("krun")
            );
            anyhow::ensure!(
                running,
                "vessel `{name}` is not running — a memory snapshot captures a LIVE vm \
                 (for a stopped vessel take a disk snapshot: --no-memory)"
            );
            (true, DiskOnlyReason::Requested)
        }
        SnapMode::Auto => {
            if !memory_capable {
                (false, DiskOnlyReason::BackendUnsupported)
            } else if !running {
                (false, DiskOnlyReason::NotRunning)
            } else {
                (true, DiskOnlyReason::Requested)
            }
        }
    };

    if memory {
        std::fs::create_dir_all(&sdir)?;
        let t0 = Instant::now();
        let mut ctl = VmmCtl::connect(&dir)?;
        use crate::backend::WorkerControl;
        // vz saves a single state file; krun saves a snapshot directory.
        let state_path = if is_vz {
            sdir.join("memory.vzstate")
        } else {
            sdir.join("memory.krun")
        };
        ctl.op(&WorkerControl::Pause)?;
        let saved = ctl
            .op(&WorkerControl::Save {
                path: state_path.clone(),
            })
            // Disks cloned while still paused = consistent with the state file.
            .and_then(|()| {
                for img in disk_images(&dir) {
                    if dir.join(&img).is_file() {
                        clone_file(&dir.join(&img), &sdir.join(&img))?;
                    }
                }
                Ok(())
            });
        let resumed = ctl.op(&WorkerControl::Resume);
        if let Err(e) = saved {
            let _ = std::fs::remove_dir_all(&sdir);
            return Err(e.context("memory snapshot failed (vessel resumed unharmed)"));
        }
        resumed?;
        let state_mb = if is_vz {
            physical_size_mb(&state_path)
        } else {
            // The guest RAM image dominates; it's written sparsely.
            physical_size_mb(&state_path.join("memory.bin"))
        };
        return Ok(SnapshotOutcome::Memory {
            ms: t0.elapsed().as_millis() as u64,
            state_mb,
        });
    }

    if running {
        stop(name)?;
    }
    std::fs::create_dir_all(&sdir)?;
    let t0 = Instant::now();
    for img in disk_images(&dir) {
        if dir.join(&img).is_file() {
            clone_file(&dir.join(&img), &sdir.join(&img))?;
        }
    }
    let ms = t0.elapsed().as_millis() as u64;
    if running {
        start(name)?;
    }
    Ok(SnapshotOutcome::DiskOnly {
        ms,
        reason: disk_reason,
    })
}

#[derive(Debug, serde::Serialize)]
pub struct SnapshotInfo {
    pub label: String,
    /// Carries machine state (live-resume capable), not just disks.
    pub memory: bool,
}

pub fn snapshots(name: &str) -> anyhow::Result<Vec<SnapshotInfo>> {
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
    Ok(labels
        .into_iter()
        .map(|label| SnapshotInfo {
            memory: snapshot_memory_state(&root.join(&label)).is_some(),
            label,
        })
        .collect())
}

pub fn snapshot_rm(name: &str, label: &str) -> anyhow::Result<()> {
    validate_label(label)?;
    let dir = dir_of(name)?;
    let sdir = snap_dir(&dir, label);
    anyhow::ensure!(sdir.exists(), "no snapshot `{name}@{label}`");
    std::fs::remove_dir_all(&sdir)?;
    Ok(())
}

/// The memory-state artifact inside a snapshot dir, if the snapshot carries
/// one: a vz state file or a krun snapshot directory.
fn snapshot_memory_state(sdir: &Path) -> Option<PathBuf> {
    let vz = sdir.join("memory.vzstate");
    if vz.is_file() {
        return Some(vz);
    }
    let krun = sdir.join("memory.krun");
    if krun.is_dir() {
        return Some(krun);
    }
    None
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum RestoreOutcome {
    /// Memory snapshot: the vessel RESUMED mid-execution.
    LiveResume(Started),
    /// Memory resume failed; the crash-consistent disks cold-booted instead.
    ColdBootFallback { resume_error: String },
    /// Disk-only snapshot restored (restarted when it had been running).
    DiskRestore { restarted: bool },
}

/// Roll a vessel back to a snapshot (its current disks are replaced).
pub fn restore(name: &str, label: &str) -> anyhow::Result<RestoreOutcome> {
    validate_label(label)?;
    let dir = dir_of(name)?;
    let sdir = snap_dir(&dir, label);
    anyhow::ensure!(sdir.exists(), "no snapshot `{name}@{label}`");
    let memory_state = snapshot_memory_state(&sdir);
    let was_running = live_pid(&dir).is_some();
    if was_running {
        stop(name)?;
    }
    for img in disk_images(&sdir) {
        if sdir.join(&img).is_file() {
            let _ = std::fs::remove_file(dir.join(&img));
            clone_file(&sdir.join(&img), &dir.join(&img))?;
        }
    }
    if let Some(memory_state) = memory_state {
        match start_with(name, Some(&memory_state)) {
            Ok(StartOutcome::Started(s)) => return Ok(RestoreOutcome::LiveResume(s)),
            Ok(StartOutcome::AlreadyRunning) => {
                // Can't happen (stopped above) — treat as resumed-unknown.
                return Ok(RestoreOutcome::ColdBootFallback {
                    resume_error: "worker already running".into(),
                });
            }
            Err(e) => {
                // Disks were cloned while paused, so a cold boot of them is
                // crash-consistent — degrade instead of leaving it dead.
                let resume_error = format!("{e:#}");
                start(name)?;
                return Ok(RestoreOutcome::ColdBootFallback { resume_error });
            }
        }
    }
    if was_running {
        start(name)?;
    }
    Ok(RestoreOutcome::DiskRestore {
        restarted: was_running,
    })
}

// --- branch -------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct BranchedVessel {
    pub name: String,
    /// Woke mid-execution from the memory snapshot (vs cold boot).
    pub live: bool,
    /// Set when a live resume failed and the branch cold-booted instead.
    pub fallback_error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct BranchOutcome {
    pub vessels: Vec<BranchedVessel>,
    pub from_memory: bool,
    pub ms: u64,
}

/// Branch new vessel(s) from a snapshot (or from the current state when no
/// label is given). With count > 1 this is the tree-search fan-out: N
/// clones (<new_name>-1..N), each booted, each fully independent.
pub fn branch(
    name: &str,
    new_name: &str,
    label: Option<&str>,
    count: u32,
) -> anyhow::Result<BranchOutcome> {
    let dir = dir_of(name)?;
    anyhow::ensure!(dir.exists(), "no vessel named `{name}`");
    let spec = read_spec(&dir)?;

    // Branch source: a snapshot, or a transient clone of the current state.
    let (src_dir, _tmp_guard);
    let mut src_state: Option<PathBuf> = None;
    match label {
        Some(l) => {
            validate_label(l)?;
            let sdir = snap_dir(&dir, l);
            anyhow::ensure!(sdir.exists(), "no snapshot `{name}@{l}`");
            // Memory snapshots fan out as LIVE resumes: every branch wakes
            // mid-execution at the exact saved instant.
            src_state = snapshot_memory_state(&sdir);
            src_dir = sdir;
            _tmp_guard = None::<TmpGuard>;
        }
        None => {
            let was_running = live_pid(&dir).is_some();
            if was_running {
                stop(name)?;
            }
            let tmp = dir.join(".branch-src");
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)?;
            for img in disk_images(&dir) {
                if dir.join(&img).is_file() {
                    clone_file(&dir.join(&img), &tmp.join(&img))?;
                }
            }
            if was_running {
                start(name)?;
            }
            src_dir = tmp.clone();
            _tmp_guard = Some(TmpGuard(tmp));
        }
    }

    let t0 = Instant::now();
    let names: Vec<String> = if count <= 1 {
        vec![new_name.to_string()]
    } else {
        (1..=count).map(|i| format!("{new_name}-{i}")).collect()
    };
    let mut vessels = Vec::with_capacity(names.len());
    for n in &names {
        validate_name(n)?;
        let ndir = vessels_root()?.join(n);
        anyhow::ensure!(!ndir.exists(), "vessel `{n}` already exists");
        std::fs::create_dir_all(&ndir)?;
        for img in disk_images(&src_dir) {
            if src_dir.join(&img).is_file() {
                clone_file(&src_dir.join(&img), &ndir.join(&img))?;
            }
        }
        let mut nspec = spec.clone();
        nspec.name = format!("vessel-{n}");
        retarget_spec(&mut nspec, &ndir);
        if src_state.is_none() && nspec.backend.as_deref() == Some("vz") {
            // Cold-booted branches get their own identity. Memory resumes
            // must keep the saved config (MAC + machine id) — those branches
            // share the source's network identity (vsock control unaffected).
            nspec.mac = Some(random_mac()?);
            nspec.machine_id = vz_machine_id();
        }
        std::fs::write(ndir.join("spec.json"), serde_json::to_vec_pretty(&nspec)?)?;
        match &src_state {
            Some(state) => match start_with(n, Some(state)) {
                Ok(_) => vessels.push(BranchedVessel {
                    name: n.clone(),
                    live: true,
                    fallback_error: None,
                }),
                Err(e) => {
                    let err = format!("{e:#}");
                    start(n)?;
                    vessels.push(BranchedVessel {
                        name: n.clone(),
                        live: false,
                        fallback_error: Some(err),
                    });
                }
            },
            None => {
                start(n)?;
                vessels.push(BranchedVessel {
                    name: n.clone(),
                    live: false,
                    fallback_error: None,
                });
            }
        }
    }
    Ok(BranchOutcome {
        from_memory: src_state.is_some(),
        ms: t0.elapsed().as_millis() as u64,
        vessels,
    })
}

/// Removes a transient branch-source dir on drop.
struct TmpGuard(PathBuf);
impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Point a cloned spec's paths (disks, console, vsock sockets, control) at
/// its own dir.
fn retarget_spec(spec: &mut VmSpec, dir: &Path) {
    for d in &mut spec.disks {
        if let Some(fname) = d.path.file_name() {
            d.path = dir.join(fname);
        }
    }
    if let ConsoleSpec::File(p) = &mut spec.console {
        if let Some(fname) = p.file_name() {
            *p = dir.join(fname);
        }
    }
    for m in &mut spec.vsock_ports {
        if let Some(fname) = m.host_path.file_name() {
            m.host_path = dir.join(fname);
        }
    }
    if let Some(p) = &mut spec.control_path {
        if let Some(fname) = p.file_name() {
            *p = dir.join(fname);
        }
    }
}
