//! `nebula vessels` — persistent named microVMs alongside the engine vessel.
//!
//! The lifecycle logic lives in `nebula_core::vessels` (shared with
//! nebulad's REST API); this module is the CLI presentation: argument
//! shaping, engine routing, the engine-dependent image-build flows, and
//! printing.

use std::io::{BufReader, Write};
use std::path::PathBuf;

use anyhow::{bail, Context};
use nebula_core::ipc;
use nebula_core::proto::*;
use nebula_core::vessels as core;
pub use nebula_core::vessels::SnapMode;
use nebula_core::vessels::{
    agent_request, clone_file, dir_of, is_engine, live_pid, read_spec, RestoreOutcome, Rootfs,
    SnapshotOutcome, StartOutcome, StopOutcome,
};

use crate::client;

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
    /// Extra persistent volumes (`name:GiB`), mounted at /mnt/<name>.
    pub volumes: Vec<String>,
}

pub fn new(opts: NewOpts) -> anyhow::Result<()> {
    anyhow::ensure!(
        !(opts.from_image.is_some() && opts.rootfs_img.is_some()),
        "--from-image and --rootfs-img are mutually exclusive"
    );
    let volumes = core::parse_volumes(&opts.volumes)?;
    let dir = dir_of(&opts.name)?;
    anyhow::ensure!(
        !dir.exists(),
        "vessel `{}` already exists (nebula vessels rm {} to recreate)",
        opts.name,
        opts.name
    );

    // Engine-dependent rootfs preparation stays in the CLI; the core create
    // takes over once rootfs.img is in place (or clones the base image).
    let rootfs = if let Some(image) = &opts.from_image {
        std::fs::create_dir_all(&dir)?;
        // Docker image -> bootable microVM rootfs, built inside the engine
        // (it has docker + e2fsprogs; our static init/agent are injected so
        // ANY arm64 linux image becomes a manageable vessel).
        build_rootfs_from_image(image, &opts.name, &dir, opts.rootfs_mb, opts.data_gib)?;
        Rootfs::Prepared
    } else if let Some(src) = &opts.rootfs_img {
        // Prebuilt rootfs file (vessels convert-image): offline, engine-free.
        anyhow::ensure!(src.is_file(), "no rootfs image at {}", src.display());
        std::fs::create_dir_all(&dir)?;
        let raw = crate::commands::maybe_gunzip(src, &dir.join("rootfs.img"))?;
        if raw != dir.join("rootfs.img") {
            clone_file(&raw, &dir.join("rootfs.img"))?;
        }
        create_data_disk(&dir, opts.data_gib)?;
        Rootfs::Prepared
    } else {
        Rootfs::BaseImage
    };

    let create = core::CreateOpts {
        name: opts.name.clone(),
        cpus: opts.cpus,
        mem: opts.mem,
        gpu: opts.gpu,
        data_gib: opts.data_gib,
        backend: opts.backend.clone(),
        volumes: volumes.clone(),
    };
    let created = core::create(&create, rootfs);
    if created.is_err() {
        // Don't leave a half-made dir behind a validation failure.
        let _ = std::fs::remove_dir_all(&dir);
    }
    created?;

    println!(
        "created vessel `{}` ({} cpus, {} MiB{}{})",
        opts.name,
        opts.cpus,
        opts.mem.max(1024),
        if opts.gpu { ", gpu" } else { "" },
        if volumes.is_empty() {
            String::new()
        } else {
            format!(
                ", volumes: {}",
                volumes
                    .iter()
                    .map(|(n, g)| format!("/mnt/{n} ({g}G)"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
    );
    start(&opts.name)
}

fn print_started(name: &str, outcome: &StartOutcome) {
    match outcome {
        StartOutcome::AlreadyRunning => println!("vessel `{name}` is already running"),
        StartOutcome::Started(s) => {
            println!(
                "vessel `{name}` {} in {}ms (kernel {}, agent v{})",
                if s.resumed { "resumed" } else { "up" },
                s.boot_ms,
                s.kernel,
                s.agent_version
            );
            println!("  shell: nebula vessels shell {name}");
        }
    }
}

pub fn start(name: &str) -> anyhow::Result<()> {
    if is_engine(name) {
        return crate::commands::up();
    }
    let outcome = core::start(name)?;
    print_started(name, &outcome);
    Ok(())
}

pub fn stop(name: &str) -> anyhow::Result<()> {
    match core::stop(name)? {
        StopOutcome::NotRunning => println!("vessel `{name}` is not running"),
        StopOutcome::Stopped => println!("vessel `{name}` stopped"),
        StopOutcome::Forced => println!("vessel `{name}` stopped (forced)"),
    }
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
        let size = std::fs::metadata(dir.join("data.img"))
            .map(|m| m.len())
            .unwrap_or(0);
        let _ = std::fs::remove_file(dir.join("data.img"));
        let f = std::fs::File::create(dir.join("data.img"))?;
        f.set_len(size)?;
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

pub fn snapshot(name: &str, label: &str, mode: SnapMode) -> anyhow::Result<()> {
    match core::snapshot(name, label, mode)? {
        SnapshotOutcome::Memory { ms, state_mb } => println!(
            "memory snapshot `{name}@{label}` taken in {:.2}s ({state_mb} MiB state, vessel never stopped)",
            ms as f64 / 1000.0
        ),
        SnapshotOutcome::DiskOnly { ms, reason } => {
            match reason {
                core::DiskOnlyReason::BackendUnsupported => println!(
                    "note: disk-only snapshot — live memory capture isn't supported for this \
                     vessel's backend on this platform yet"
                ),
                core::DiskOnlyReason::NotRunning => println!(
                    "note: vessel is stopped — disk-only snapshot (start it to capture memory)"
                ),
                core::DiskOnlyReason::Requested => {}
            }
            println!(
                "snapshot `{name}@{label}` taken in {:.0}s",
                ms as f64 / 1000.0
            );
        }
    }
    Ok(())
}

pub fn snapshots(name: &str) -> anyhow::Result<()> {
    let list = core::snapshots(name)?;
    if list.is_empty() {
        println!("no snapshots for `{name}` (create one: nebula vessels snapshot {name} <label>)");
        return Ok(());
    }
    for s in list {
        if s.memory {
            println!("{name}@{}  (disks + memory state)", s.label);
        } else {
            println!("{name}@{}", s.label);
        }
    }
    Ok(())
}

pub fn snapshot_rm(name: &str, label: &str) -> anyhow::Result<()> {
    core::snapshot_rm(name, label)?;
    println!("removed snapshot `{name}@{label}`");
    Ok(())
}

/// Roll a vessel back to a snapshot (its current disks are replaced). When
/// the snapshot carries machine state, the vessel RESUMES mid-execution.
pub fn restore(name: &str, label: &str) -> anyhow::Result<()> {
    match core::restore(name, label)? {
        RestoreOutcome::LiveResume(s) => {
            print_started(name, &StartOutcome::Started(s));
            println!("`{name}` restored to @{label} (live resume — processes/RAM intact)");
        }
        RestoreOutcome::ColdBootFallback { resume_error } => {
            eprintln!(
                "memory-state resume failed ({resume_error}); cold-booted the restored disks instead"
            );
            println!("`{name}` restored to @{label}");
        }
        RestoreOutcome::DiskRestore { .. } => println!("`{name}` restored to @{label}"),
    }
    Ok(())
}

/// Branch new vessel(s) from a snapshot (or from the current state when no
/// label is given). With --count N this is the tree-search fan-out: N clones,
/// each booted, each fully independent.
pub fn branch(name: &str, new_name: &str, label: Option<&str>, count: u32) -> anyhow::Result<()> {
    let out = core::branch(name, new_name, label, count)?;
    for v in &out.vessels {
        if let Some(err) = &v.fallback_error {
            eprintln!(
                "branch `{}`: live resume failed ({err}); cold-booted its disks instead",
                v.name
            );
        }
    }
    println!(
        "branched {} vessel(s) from `{name}{}` in {:.2}s{}",
        out.vessels.len(),
        label.map(|l| format!("@{l}")).unwrap_or_default(),
        out.ms as f64 / 1000.0,
        if out.from_memory {
            " (live resume — each woke mid-execution)"
        } else {
            ""
        }
    );
    Ok(())
}

pub fn rm(name: &str, force: bool) -> anyhow::Result<()> {
    core::rm(name, force)?;
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

    for v in core::list()? {
        println!(
            "{:<14} {:<9} {:>5} {:>8}M {:>5}  ",
            v.name,
            if v.running { "running" } else { "stopped" },
            v.cpus,
            v.mem_mib,
            if v.gpu { "yes" } else { "no" }
        );
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
    for d in spec.disks.iter().skip(2) {
        let mount = d
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("vol-"))
            .map(|n| format!("  -> /mnt/{n}"))
            .unwrap_or_default();
        println!("          {}{mount}", d.path.display());
    }
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
    let stream = ipc::connect(&dir.join("shell.sock"))?;
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
