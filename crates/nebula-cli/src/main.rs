use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod autostart;
mod client;
mod commands;
mod contexts;
mod kube;
mod sandbox;
mod spike;
mod wrap;

const QUICKSTART: &str = "\
\x1b[1mQUICKSTART\x1b[0m
  Engine:
    nebula up                          start the engine (~0.6s)
    nebula autostart enable            start it at login instead

  Docker:
    nebula setup docker                point `docker` at Nebula (revert: nebula revert docker)
    docker run -d -p 8080:80 nginx     then open http://localhost:8080
    nebula docker ps                   …or one-off, without changing contexts

  Kubernetes (k3s, started on demand):
    nebula setup kubectl               point `kubectl` at Nebula (revert: nebula revert kubectl)
    kubectl create deployment web --image=nginx
    kubectl expose deployment web --port 80 --type NodePort
    nebula kubectl get nodes           …or one-off, without changing contexts

  Helm:
    nebula helm install my-redis oci://registry-1.docker.io/bitnamicharts/redis

  Isolated microVMs:
    nebula sandbox run -- uname -a     boots, runs, and exits in ~250ms

  See where you stand anytime: nebula status";

#[derive(Parser)]
#[command(
    name = "nebula",
    version,
    about = "Containers, Kubernetes, and microVMs on macOS",
    after_help = QUICKSTART
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
        /// docker | nerdctl | kubectl
        tool: String,
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
    /// Diagnose common setup problems.
    Doctor,
    /// Install guest images (kernel + rootfs) into ~/.nebula.
    InstallImage {
        #[arg(long)]
        kernel: Option<PathBuf>,
        #[arg(long)]
        rootfs: Option<PathBuf>,
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
        Commands::Setup { tool } => contexts::setup_tool(&tool),
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
        Commands::Doctor => commands::doctor(),
        Commands::InstallImage { kernel, rootfs } => commands::install_image(kernel, rootfs),
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
    }
}
