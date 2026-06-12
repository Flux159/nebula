//! Scenario 1 — container density in vessel 0 per configured max RAM.
//!
//! For each (max_ram, workload) point: fresh engine, add containers in
//! batches, watch for the first break condition. Container-level failures are
//! counted but only stop the point when half a batch fails (slimd is expected
//! to shed load before the engine dies — that tail is data, not an abort).

use crate::config::ConfigGuard;
use crate::nebula::Nebula;
use crate::sampler::Sampler;
use crate::scenarios::common::{engine_checks, Brk};
use crate::util;
use anyhow::Context;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Args {
    pub flavor: String,
    pub max_ram: Vec<u64>,
    pub workloads: Vec<String>,
    pub batch: usize,
    pub max_n: usize,
    pub rootfs: Option<std::path::PathBuf>,
}

#[derive(serde::Serialize)]
struct PointResult {
    max_ram_mib: u64,
    workload: String,
    flavor: String,
    n_started: usize,
    n_running_final: usize,
    container_errors: usize,
    stop_reason: String,
    duration_s: f64,
}

pub fn run(neb: &Nebula, out_root: &Path, a: Args) -> anyhow::Result<i32> {
    let dir = crate::nebula::run_dir(out_root, &format!("container-scale-{}", a.flavor))?;
    println!("container-scale ({}) -> {}", a.flavor, dir.display());
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "scenario": "container-scale",
            "flavor": a.flavor,
            "host": crate::hostmem::host_meta(),
            "params": {
                "max_ram": a.max_ram, "workloads": a.workloads,
                "batch": a.batch, "max_n": a.max_n,
            },
            "started_unix": util::unix_now(),
        }))?,
    )?;

    let mut points_csv = std::fs::File::create(dir.join("points.csv"))?;
    writeln!(
        points_csv,
        "max_ram_mib,workload,n,running,batch_ms_median,fp_mib,held_mib,guest_avail_mib,psi_some,errors_cum"
    )?;
    let mut results: Vec<PointResult> = Vec::new();

    let mut guard = ConfigGuard::take()?;
    if a.rootfs.is_some() {
        guard.set_rootfs(a.rootfs.as_deref())?;
    }
    let sampler = Sampler::start(neb.api_port, &dir.join("trace.csv"), Duration::from_secs(2))?;

    let outcome = (|| -> anyhow::Result<()> {
        for &mr in &a.max_ram {
            guard.set_max_ram(mr)?;
            for w in &a.workloads {
                println!("== point: max_ram={mr} MiB, workload={w}");
                sampler.set_phase(&format!("maxram={mr}/{w}"));
                neb.fresh_up()
                    .with_context(|| format!("fresh up @ {mr} MiB"))?;
                neb.wait_docker(Duration::from_secs(60))?;
                neb.pre_pull(&images_for(w))?;
                let r = run_point(neb, &a, mr, w, &mut points_csv, &sampler)?;
                println!(
                    "   -> n={} ({} still running), stop: {}",
                    r.n_started, r.n_running_final, r.stop_reason
                );
                results.push(r);
                std::fs::write(
                    dir.join("results.json"),
                    serde_json::to_string_pretty(&results)?,
                )?;
                neb.cleanup_containers("bt-c");
            }
        }
        Ok(())
    })();

    neb.cleanup_containers("bt-c");
    guard.restore()?;
    println!("-- restoring engine with original config");
    neb.fresh_up().ok();
    sampler.stop();
    outcome?;
    println!(
        "container-scale ({}): {} points done",
        a.flavor,
        results.len()
    );
    Ok(0)
}

fn images_for(w: &str) -> Vec<&'static str> {
    if w == "nginx" {
        vec!["nginx:alpine"]
    } else {
        vec!["alpine:3.20"]
    }
}

/// `docker run` args for container number `n` of workload `w`.
fn workload_args(w: &str, n: usize, name: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--restart".into(),
        "no".into(),
        "--name".into(),
        name.into(),
    ];
    if w == "idle" {
        args.extend(["alpine:3.20".into(), "sleep".into(), "infinity".into()]);
    } else if w == "nginx" {
        // Every 10th container publishes a port: exercises the proxy path
        // without making the host port table the bottleneck under test.
        if n.is_multiple_of(10) {
            args.extend(["-p".into(), "127.0.0.1::80".into()]);
        }
        args.push("nginx:alpine".into());
    } else if let Some(mib) = w.strip_prefix("hog:") {
        let mib: u64 = mib.trim_end_matches(['m', 'M']).parse().unwrap_or(256);
        args.extend([
            format!("--shm-size={}m", mib + 32),
            "alpine:3.20".into(),
            "sh".into(),
            "-c".into(),
            format!(
                "dd if=/dev/zero of=/dev/shm/h bs=1M count={mib} status=none && exec sleep infinity"
            ),
        ]);
    } else {
        args.extend(["alpine:3.20".into(), "sleep".into(), "infinity".into()]);
    }
    args
}

fn run_point(
    neb: &Nebula,
    a: &Args,
    mr: u64,
    w: &str,
    points_csv: &mut std::fs::File,
    sampler: &Sampler,
) -> anyhow::Result<PointResult> {
    let t0 = Instant::now();
    let oom0 = neb.oom_count();
    let mut n = 0usize;
    let mut errors = 0usize;
    let mut early_medians: Vec<f64> = Vec::new();
    let stop: Brk = 'outer: loop {
        let mut batch_lats: Vec<f64> = Vec::new();
        let mut batch_errs = 0usize;
        for _ in 0..a.batch {
            n += 1;
            let name = format!("bt-c{n}");
            let args = workload_args(w, n, &name);
            let t = Instant::now();
            let r = neb.docker(&args, Duration::from_secs(60));
            batch_lats.push(t.elapsed().as_secs_f64() * 1000.0);
            match r {
                Ok(o) if o.ok() => {}
                Ok(o) => {
                    errors += 1;
                    batch_errs += 1;
                    if o.timed_out {
                        break 'outer Brk::Timeout {
                            detail: format!("docker run #{n}"),
                        };
                    }
                    if batch_errs * 2 >= a.batch {
                        break 'outer Brk::CmdError {
                            detail: format!(
                                "{batch_errs}/{} of a batch failed; last: {}",
                                a.batch,
                                o.brief_err()
                            ),
                        };
                    }
                }
                Err(e) => {
                    break 'outer Brk::CmdError {
                        detail: format!("docker run #{n}: {e:#}"),
                    }
                }
            }
            if n >= a.max_n {
                break;
            }
        }
        sampler.set_phase(&format!("maxram={mr}/{w}/n={n}"));

        let batch_median = util::median(&batch_lats);
        if early_medians.len() < 3 {
            early_medians.push(batch_median);
        }
        let baseline = util::median(&early_medians);
        let running = neb.bt_containers("bt-c").map(|v| v.len()).unwrap_or(0);
        let s = neb.stats().ok();
        writeln!(
            points_csv,
            "{mr},{w},{n},{running},{batch_median:.0},{},{},{},{:.2},{errors}",
            s.as_ref().map(|s| s.host_footprint_mib).unwrap_or(0),
            s.as_ref().map(|s| s.held_mib()).unwrap_or(0),
            s.as_ref().map(|s| s.guest_avail_mib()).unwrap_or(0),
            s.as_ref().map(|s| s.psi_some()).unwrap_or(0.0),
        )?;
        points_csv.flush().ok();
        println!(
            "   n={n} running={running} batch_median={batch_median:.0}ms fp={}MiB",
            s.as_ref().map(|s| s.host_footprint_mib).unwrap_or(0)
        );

        if let Some(b) = engine_checks(neb, oom0) {
            break b;
        }
        if early_medians.len() >= 3 && batch_median > 10.0 * baseline && batch_median > 2000.0 {
            break Brk::LatencyBlowup {
                median_ms: batch_median,
                baseline_ms: baseline,
            };
        }
        if n >= a.max_n {
            break Brk::MaxN { n };
        }
    };

    let n_running_final = neb.bt_containers("bt-c").map(|v| v.len()).unwrap_or(0);
    Ok(PointResult {
        max_ram_mib: mr,
        workload: w.to_string(),
        flavor: a.flavor.clone(),
        n_started: n,
        n_running_final,
        container_errors: errors,
        stop_reason: stop.label(),
        duration_s: t0.elapsed().as_secs_f64(),
    })
}
