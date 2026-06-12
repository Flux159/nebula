//! Scenario 2 — concurrent vessel count per backend and per-vessel RAM.
//!
//! Vessels are created one at a time (boot latency vs N is half the point),
//! verified live with an exec round-trip, and torn down at the end. The
//! engine vessel stays up — that's the realistic baseline.

use crate::nebula::Nebula;
use crate::sampler::Sampler;
use crate::scenarios::common::{engine_checks, Brk};
use crate::util;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Args {
    pub backends: Vec<String>,
    pub mems: Vec<u64>,
    pub limit: usize,
    pub disk_gib: u64,
}

#[derive(serde::Serialize)]
struct PointResult {
    backend: String,
    mem_mib: u64,
    n_max: usize,
    stop_reason: String,
    boot_ms_first: f64,
    boot_ms_last: f64,
    boot_ms_median: f64,
    /// Host free-RAM delta divided by N — noisy but honest; the engine's
    /// stats footprint only covers the engine VM, not sibling vessels.
    host_cost_per_vessel_mib: f64,
    duration_s: f64,
}

pub fn run(neb: &Nebula, out_root: &Path, a: Args) -> anyhow::Result<i32> {
    let dir = crate::nebula::run_dir(out_root, "vessel-scale")?;
    println!("vessel-scale -> {}", dir.display());
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "scenario": "vessel-scale",
            "host": crate::hostmem::host_meta(),
            "params": {
                "backends": a.backends, "mems": a.mems,
                "limit": a.limit, "disk_gib": a.disk_gib,
            },
            "started_unix": util::unix_now(),
        }))?,
    )?;

    let mut points_csv = std::fs::File::create(dir.join("points.csv"))?;
    writeln!(
        points_csv,
        "backend,mem_mib,n,boot_ms,host_free_mib,exec_ok"
    )?;
    let mut results: Vec<PointResult> = Vec::new();

    // No config juggling here — the engine stays as-is; vessels carry their
    // own --mem. But make sure the engine is healthy before we lean on it.
    if !neb.healthy() {
        println!("engine not healthy; booting it");
        neb.fresh_up()?;
    }

    let sampler = Sampler::start(neb.api_port, &dir.join("trace.csv"), Duration::from_secs(2))?;

    let outcome = (|| -> anyhow::Result<()> {
        for backend in &a.backends {
            for &mem in &a.mems {
                println!("== point: backend={backend}, mem={mem} MiB");
                sampler.set_phase(&format!("{backend}/mem={mem}"));
                let r = run_point(neb, &a, backend, mem, &mut points_csv)?;
                println!(
                    "   -> n_max={} (boot {:.0}ms -> {:.0}ms), stop: {}",
                    r.n_max, r.boot_ms_first, r.boot_ms_last, r.stop_reason
                );
                results.push(r);
                std::fs::write(
                    dir.join("results.json"),
                    serde_json::to_string_pretty(&results)?,
                )?;
                println!("   tearing down bt-v vessels");
                neb.cleanup_vessels();
            }
        }
        Ok(())
    })();

    neb.cleanup_vessels();
    sampler.stop();
    outcome?;
    println!("vessel-scale: {} points done", results.len());
    Ok(0)
}

fn run_point(
    neb: &Nebula,
    a: &Args,
    backend: &str,
    mem: u64,
    points_csv: &mut std::fs::File,
) -> anyhow::Result<PointResult> {
    let t0 = Instant::now();
    let oom0 = neb.oom_count();
    let free0 = crate::hostmem::host_free_mib().unwrap_or(0);
    let mut boots: Vec<f64> = Vec::new();
    let mut n = 0usize;
    let stop: Brk = loop {
        if n >= a.limit {
            break Brk::MaxN { n };
        }
        let name = format!("bt-v{n}");
        let disk = a.disk_gib.to_string();
        let mem_s = mem.to_string();
        let t = Instant::now();
        let create = neb.vessels(
            &[
                "new",
                &name,
                "--backend",
                backend,
                "--mem",
                &mem_s,
                "--disk",
                &disk,
            ],
            Duration::from_secs(120),
        );
        let boot_ms = t.elapsed().as_secs_f64() * 1000.0;
        let created_ok = match create {
            Ok(o) if o.ok() => true,
            Ok(o) => {
                break if o.timed_out {
                    Brk::Timeout {
                        detail: format!("vessels new {name}"),
                    }
                } else {
                    Brk::CmdError {
                        detail: format!("vessels new {name}: {}", o.brief_err()),
                    }
                };
            }
            Err(e) => {
                break Brk::CmdError {
                    detail: format!("vessels new {name}: {e:#}"),
                }
            }
        };
        // Verify the vessel is actually alive, not just created.
        let exec_ok = created_ok
            && neb
                .vessels(&["exec", &name, "true"], Duration::from_secs(30))
                .map(|o| o.ok())
                .unwrap_or(false);
        let host_free = crate::hostmem::host_free_mib().unwrap_or(0);
        writeln!(
            points_csv,
            "{backend},{mem},{n},{boot_ms:.0},{host_free},{}",
            exec_ok as u8
        )?;
        points_csv.flush().ok();
        if !exec_ok {
            break Brk::AgentUnhealthy {
                detail: format!("{name} created but exec failed"),
            };
        }
        boots.push(boot_ms);
        n += 1;
        if n.is_multiple_of(5) {
            println!("   n={n} boot={boot_ms:.0}ms host_free={host_free}MiB");
            if let Some(b) = engine_checks(neb, oom0) {
                break b;
            }
        }
    };

    let free_now = crate::hostmem::host_free_mib().unwrap_or(free0);
    Ok(PointResult {
        backend: backend.to_string(),
        mem_mib: mem,
        n_max: n,
        stop_reason: stop.label(),
        boot_ms_first: boots.first().copied().unwrap_or(0.0),
        boot_ms_last: boots.last().copied().unwrap_or(0.0),
        boot_ms_median: util::median(&boots),
        host_cost_per_vessel_mib: if n > 0 {
            (free0.saturating_sub(free_now)) as f64 / n as f64
        } else {
            0.0
        },
        duration_s: t0.elapsed().as_secs_f64(),
    })
}
