//! nebula-battletest — scale limits + balloon regression harness.
//! Plan and rationale: tasks/nebulabattletest.md. NOT run on hosted CI
//! (RAM-starved); compiled there so it can't rot, executed locally and on
//! future self-hosted runners.

mod api;
mod config;
mod hostmem;
mod nebula;
mod report;
mod sampler;
mod scenarios;
mod svg;
mod util;

use anyhow::bail;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "nebula-battletest",
    about = "Battle-test Nebula: container/vessel scale limits and balloon regression.\n\
             Destructive to the local engine (rewrites ~/.nebula/config.toml, restarts it);\n\
             requires NEBULA_BATTLETEST=1 or --yes."
)]
struct Cli {
    /// Path to the nebula CLI (default: `nebula` beside this binary, then PATH)
    #[arg(long, global = true)]
    nebula: Option<PathBuf>,
    /// REST API port of the engine under test
    #[arg(long, global = true, default_value_t = 7440)]
    api_port: u16,
    /// Results directory
    #[arg(long, global = true, default_value = "bench/results")]
    out: PathBuf,
    /// Confirm a destructive run (alternative to NEBULA_BATTLETEST=1)
    #[arg(long, global = true)]
    yes: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scenario 1: container density in vessel 0 per configured max RAM
    ContainerScale {
        /// Engine flavor label recorded in results: full or slim
        #[arg(long, default_value = "full")]
        flavor: String,
        /// max_ram_mib sweep points (MiB)
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "4096,8192,16384,32768,65536"
        )]
        max_ram: Vec<u64>,
        /// Workloads: idle | nginx | hog:<MiB>
        #[arg(long, value_delimiter = ',', default_value = "idle,nginx,hog:256")]
        workload: Vec<String>,
        /// Containers per batch between health checks
        #[arg(long, default_value_t = 10)]
        batch: usize,
        /// Hard cap per point (runaway guard, recorded as stop_reason=max_n)
        #[arg(long, default_value_t = 1500)]
        max_n: usize,
        /// rootfs image override written to config.toml for the run (slim)
        #[arg(long)]
        rootfs: Option<PathBuf>,
    },
    /// Scenario 2: concurrent vessels (no containers) per backend and RAM
    VesselScale {
        #[arg(long, value_delimiter = ',', default_value = "vz,krun")]
        backend: Vec<String>,
        /// Per-vessel RAM sweep points (MiB)
        #[arg(long, value_delimiter = ',', default_value = "1024,2048,4096")]
        mem: Vec<u64>,
        /// Hard cap per point
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Per-vessel data disk (GiB, sparse)
        #[arg(long, default_value_t = 4)]
        disk: u64,
    },
    /// Scenario 3: balloon contract suite (+ regression baseline compare)
    Balloon {
        /// Repeat-cycle count for drift detection
        #[arg(long, default_value_t = 10)]
        cycles: u32,
        /// Baseline JSON to compare against (default bench/baselines/<host>.json)
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Write this run's metrics as the new baseline instead of comparing
        #[arg(long)]
        write_baseline: bool,
        /// ~10 min variant: 3 drift cycles, 3 min sawtooth
        #[arg(long)]
        quick: bool,
    },
    /// Render bench/results into report.md + SVG charts
    Report {
        #[arg(long = "in", default_value = "bench/results")]
        input: PathBuf,
        #[arg(long, default_value = "bench/report")]
        report_out: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if !matches!(cli.cmd, Cmd::Report { .. }) {
        let armed = cli.yes || std::env::var("NEBULA_BATTLETEST").ok().as_deref() == Some("1");
        if !armed {
            bail!(
                "refusing to run: this rewrites ~/.nebula/config.toml and restarts the \
                 engine repeatedly. Set NEBULA_BATTLETEST=1 or pass --yes."
            );
        }
    }

    let exit = match cli.cmd {
        Cmd::ContainerScale {
            flavor,
            max_ram,
            workload,
            batch,
            max_n,
            rootfs,
        } => {
            let neb = nebula::Nebula::locate(cli.nebula, cli.api_port)?;
            scenarios::container_scale::run(
                &neb,
                &cli.out,
                scenarios::container_scale::Args {
                    flavor,
                    max_ram,
                    workloads: workload,
                    batch,
                    max_n,
                    rootfs,
                },
            )?
        }
        Cmd::VesselScale {
            backend,
            mem,
            limit,
            disk,
        } => {
            let neb = nebula::Nebula::locate(cli.nebula, cli.api_port)?;
            scenarios::vessel_scale::run(
                &neb,
                &cli.out,
                scenarios::vessel_scale::Args {
                    backends: backend,
                    mems: mem,
                    limit,
                    disk_gib: disk,
                },
            )?
        }
        Cmd::Balloon {
            cycles,
            baseline,
            write_baseline,
            quick,
        } => {
            let neb = nebula::Nebula::locate(cli.nebula, cli.api_port)?;
            scenarios::balloon::run(
                &neb,
                &cli.out,
                scenarios::balloon::Args {
                    cycles,
                    baseline,
                    write_baseline,
                    quick,
                },
            )?
        }
        Cmd::Report { input, report_out } => report::run(&input, &report_out)?,
    };
    std::process::exit(exit);
}
