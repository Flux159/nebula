use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod autostart;
mod client;
mod commands;
mod contexts;
mod kube;
mod pathsetup;
mod sandbox;
mod spike;
mod vessels;
mod wrap;

#[derive(Parser)]
#[command(
    name = "nebula",
    version,
    about = "Containers, Kubernetes, and microVMs on macOS",
    after_help = "Don't know where to start? Run: nebula quickstart"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the Nebula engine (boots the Vessel and waits for it to be healthy).
    Up {
        /// Dev spike: boot a throwaway VM, verify the boot marker, and exit.
        #[arg(long)]
        dev: bool,
        /// (--dev) VMM backend: vz or krun.
        #[arg(long, default_value = "vz")]
        backend: String,
        /// (--dev) vCPUs.
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        /// (--dev) guest RAM in MiB.
        #[arg(long, default_value_t = 512)]
        mem: u64,
    },
    /// Stop the Nebula engine.
    Down {
        /// Skip graceful guest shutdown.
        #[arg(long)]
        force: bool,
    },
    /// Show engine status.
    Status,
    /// Run a command in the Vessel and print its output.
    Exec {
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
    /// Open an interactive shell in the Vessel.
    Shell,
    /// Show the Vessel console log.
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    /// Point a tool (docker, nerdctl, kubectl) at Nebula. Undo: nebula revert.
    #[command(alias = "use")]
    Setup {
        /// docker | nerdctl | kubectl | path (link bundled CLIs for missing tools)
        tool: String,
        /// Skip confirmation prompts.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Run one docker command against Nebula without changing any context.
    Docker {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Run one kubectl command against Nebula without changing any context.
    Kubectl {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Run one helm command against Nebula without changing any context.
    Helm {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Restore a tool's previous configuration (`--all` for every tool).
    Revert {
        /// docker | nerdctl | kubectl | all
        tool: String,
    },
    /// Live memory stats: guest usage, balloon, and host-visible footprint.
    Stats {
        /// Refresh continuously.
        #[arg(short, long)]
        watch: bool,
    },
    /// Manage starting the engine automatically at login (launchd).
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
    /// Open the Nebula desktop app.
    Ui,
    /// Show short guides for getting started (engine, docker, k8s, helm).
    Quickstart,
    /// Diagnose common setup problems.
    Doctor,
    /// Install guest images (kernel + rootfs) into ~/.nebula.
    InstallImage {
        #[arg(long)]
        kernel: Option<PathBuf>,
        #[arg(long)]
        rootfs: Option<PathBuf>,
        /// Download the released images instead of using local files.
        #[arg(long)]
        download: bool,
    },
    /// Manage persistent named microVMs (separate from the engine vessel).
    Vessels {
        #[command(subcommand)]
        action: VesselsAction,
    },
    /// Run a command in an ephemeral, isolated microVM (libkrun sidecar).
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },
    /// Internal: libkrun worker process (takes over and becomes the microVM).
    #[command(name = "krun-worker", hide = true)]
    KrunWorker {
        #[arg(long)]
        spec: String,
    },
    /// Internal: Virtualization.framework worker process (hosts a VZ vessel).
    #[command(name = "vz-worker", hide = true)]
    VzWorker {
        #[arg(long)]
        spec: String,
        /// Boot from a saved machine state instead of a cold kernel boot.
        #[arg(long)]
        restore: Option<PathBuf>,
        /// JSON-lines control socket (pause/save/resume) for live snapshots.
        #[arg(long)]
        control: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum AutostartAction {
    /// Start nebulad at login and keep it alive.
    Enable,
    /// Remove the login agent.
    Disable,
    /// Show autostart + engine state.
    Status,
}

#[derive(Subcommand)]
enum VesselsAction {
    /// Create and start a new named vessel.
    New {
        name: String,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        /// Guest RAM in MiB.
        #[arg(long, default_value_t = 2048)]
        mem: u64,
        /// Attach the host GPU (virtio-gpu Venus).
        #[arg(long)]
        gpu: bool,
        /// Data disk size in GiB (sparse).
        #[arg(long, default_value_t = 16)]
        disk: u64,
        /// Build the rootfs from a docker image (full VM isolation +
        /// snapshots for a standard container image).
        #[arg(long)]
        from_image: Option<String>,
        /// Rootfs size in MiB when building from an image.
        #[arg(long, default_value_t = 4096)]
        rootfs_size: u64,
        /// VMM backend: `krun` (default — fastest boot, GPU) or `vz`
        /// (Virtualization.framework — enables live memory-state snapshots).
        #[arg(long, default_value = "krun")]
        backend: String,
    },
    /// List vessels (including the engine vessel).
    Ls,
    /// Start a stopped vessel.
    Start { name: String },
    /// Stop a running vessel (the engine vessel is managed by nebula down).
    Stop { name: String },
    /// Remove a vessel and its disks.
    Rm {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Take a crash-consistent disk snapshot (stop->clone->restart, <1s).
    Snapshot {
        name: String,
        label: String,
        /// Also save live machine state (RAM + devices) WITHOUT stopping the
        /// vessel: pause -> save -> clone disks -> resume. Restoring resumes
        /// exactly where execution left off. Requires a `--backend vz` vessel.
        #[arg(long)]
        memory: bool,
    },
    /// List a vessel's snapshots.
    Snapshots { name: String },
    /// Delete a snapshot.
    SnapshotRm { name: String, label: String },
    /// Roll a vessel back to a snapshot.
    Restore { name: String, label: String },
    /// Clone new vessel(s) from a snapshot (or current state) and boot them.
    Branch {
        name: String,
        new_name: String,
        /// Branch from this snapshot instead of the current state.
        #[arg(long)]
        snapshot: Option<String>,
        /// Fan out N clones (named <new_name>-1..N) — the tree-search primitive.
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Restore a vessel's rootfs to pristine (works on the engine vessel too).
    Reset {
        name: String,
        /// Also wipe the data disk (containers/images/k8s state).
        #[arg(long)]
        wipe_data: bool,
    },
    /// Show a vessel's configuration and live state.
    Info { name: String },
    /// Run a command inside a vessel.
    Exec {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Open an interactive shell inside a vessel.
    Shell { name: String },
}

#[derive(Subcommand)]
enum SandboxAction {
    /// Boot a microVM, run CMD, print its output, exit with its code.
    Run {
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        /// Guest RAM in MiB.
        #[arg(long, default_value_t = 1024)]
        mem: u64,
        /// Share the current directory into the sandbox at /workdir.
        #[arg(long)]
        share_cwd: bool,
        /// Attach the host GPU (virtio-gpu Venus, Vulkan->Metal).
        #[arg(long)]
        gpu: bool,
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    // Default SIGPIPE handling: `nebula status | grep -q …` should not panic
    // when the reader closes early.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Up {
            dev,
            backend,
            cpus,
            mem,
        } => {
            if dev {
                spike::run(&backend, cpus, mem)
            } else {
                commands::up()
            }
        }
        Commands::Down { force } => commands::down(force),
        Commands::Status => commands::status(),
        Commands::Exec { cmd } => commands::exec(cmd),
        Commands::Shell => commands::shell(),
        Commands::Logs { follow } => commands::logs(follow),
        Commands::Setup { tool, yes } => {
            if tool == "path" {
                pathsetup::install(yes)
            } else {
                contexts::setup_tool(&tool)
            }
        }
        Commands::Docker { args } => wrap::docker(args),
        Commands::Kubectl { args } => wrap::kubectl(args),
        Commands::Helm { args } => wrap::helm(args),
        Commands::Revert { tool } => contexts::revert_tool(&tool),
        Commands::Stats { watch } => commands::stats(watch),
        Commands::Autostart { action } => match action {
            AutostartAction::Enable => autostart::enable(),
            AutostartAction::Disable => autostart::disable(),
            AutostartAction::Status => autostart::status(),
        },
        Commands::Ui => autostart::open_ui(),
        Commands::Quickstart => commands::quickstart(),
        Commands::Doctor => commands::doctor(),
        Commands::InstallImage {
            kernel,
            rootfs,
            download,
        } => {
            if download {
                commands::download_images()
            } else {
                commands::install_image(kernel, rootfs)
            }
        }
        Commands::Vessels { action } => match action {
            VesselsAction::New {
                name,
                cpus,
                mem,
                gpu,
                disk,
                from_image,
                rootfs_size,
                backend,
            } => vessels::new(vessels::NewOpts {
                name,
                cpus,
                mem,
                gpu,
                data_gib: disk,
                from_image,
                rootfs_mb: rootfs_size,
                backend,
            }),
            VesselsAction::Ls => vessels::ls(),
            VesselsAction::Start { name } => vessels::start(&name),
            VesselsAction::Stop { name } => vessels::stop(&name),
            VesselsAction::Rm { name, force } => vessels::rm(&name, force),
            VesselsAction::Snapshot {
                name,
                label,
                memory,
            } => vessels::snapshot(&name, &label, memory),
            VesselsAction::Snapshots { name } => vessels::snapshots(&name),
            VesselsAction::SnapshotRm { name, label } => vessels::snapshot_rm(&name, &label),
            VesselsAction::Restore { name, label } => vessels::restore(&name, &label),
            VesselsAction::Branch {
                name,
                new_name,
                snapshot,
                count,
            } => vessels::branch(&name, &new_name, snapshot.as_deref(), count),
            VesselsAction::Reset { name, wipe_data } => vessels::reset(&name, wipe_data),
            VesselsAction::Info { name } => vessels::info(&name),
            VesselsAction::Exec { name, cmd } => vessels::exec(&name, cmd),
            VesselsAction::Shell { name } => vessels::shell(&name),
        },
        Commands::Sandbox { action } => match action {
            SandboxAction::Run {
                cpus,
                mem,
                share_cwd,
                gpu,
                cmd,
            } => sandbox::run(sandbox::SandboxOpts {
                cpus,
                mem,
                share_cwd,
                gpu,
                cmd,
            }),
        },
        Commands::KrunWorker { spec } => match nebula_core::backend::krun::run_worker(&spec) {
            Ok(never) => match never {},
            Err(e) => {
                eprintln!("krun-worker: {e}");
                std::process::exit(1);
            }
        },
        Commands::VzWorker {
            spec,
            restore,
            control,
        } => {
            if let Err(e) =
                nebula_core::backend::vz::run_worker(&spec, restore.as_deref(), control.as_deref())
            {
                eprintln!("vz-worker: {e}");
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
