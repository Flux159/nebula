use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod client;
mod commands;
mod spike;

#[derive(Parser)]
#[command(
    name = "nebula",
    version,
    about = "Containers, Kubernetes, and microVMs on macOS"
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
    /// Diagnose common setup problems.
    Doctor,
    /// Install guest images (kernel + rootfs) into ~/.nebula.
    InstallImage {
        #[arg(long)]
        kernel: Option<PathBuf>,
        #[arg(long)]
        rootfs: Option<PathBuf>,
    },
    /// Internal: libkrun worker process (takes over and becomes the microVM).
    #[command(name = "krun-worker", hide = true)]
    KrunWorker {
        #[arg(long)]
        spec: String,
    },
}

fn main() -> anyhow::Result<()> {
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
        Commands::Doctor => commands::doctor(),
        Commands::InstallImage { kernel, rootfs } => commands::install_image(kernel, rootfs),
        Commands::KrunWorker { spec } => match nebula_core::backend::krun::run_worker(&spec) {
            Ok(never) => match never {},
            Err(e) => {
                eprintln!("krun-worker: {e}");
                std::process::exit(1);
            }
        },
    }
}
