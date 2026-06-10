use clap::{Parser, Subcommand};

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
    /// Start the Nebula engine (Phase 0: --dev boots a throwaway spike VM).
    Up {
        /// Boot a throwaway spike VM, verify the boot marker, and exit.
        #[arg(long)]
        dev: bool,
        /// VMM backend: vz (Virtualization.framework) or krun (libkrun).
        #[arg(long, default_value = "vz")]
        backend: String,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        #[arg(long, default_value_t = 512)]
        mem: u64,
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
                anyhow::bail!(
                    "`nebula up` (the managed Vessel) lands in Phase 1; use --dev for the spike"
                );
            }
        }
        Commands::KrunWorker { spec } => match nebula_core::backend::krun::run_worker(&spec) {
            Ok(never) => match never {},
            Err(e) => {
                eprintln!("krun-worker: {e}");
                std::process::exit(1);
            }
        },
    }
}
