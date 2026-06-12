//! Scenario 3 — balloon contract + regression suite.
//!
//! Absorbs the phase-4 single-hog check and extends it: repeat-cycle drift,
//! concurrent hogs, pressure at the ceiling, sawtooth thrash-count. The VZ
//! balloon has high-water-mark semantics (tasks/issues.md), so the contract
//! is bounded growth + re-inflate, never post-workload shrink.
//!
//! Every check lands numbers in metrics.json; `--baseline` compares against a
//! stored run and exits non-zero on drift — that file IS the regression gate.

use crate::config::ConfigGuard;
use crate::nebula::Nebula;
use crate::sampler::Sampler;
use crate::util;
use anyhow::Context;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct Args {
    pub cycles: u32,
    pub baseline: Option<PathBuf>,
    pub write_baseline: bool,
    pub quick: bool,
}

const HOG_NAME: &str = "bt-balloon-hog";

struct Rec {
    metrics: BTreeMap<String, f64>,
    failures: Vec<String>,
}

impl Rec {
    fn m(&mut self, k: &str, v: f64) {
        println!("    {k} = {v:.1}");
        self.metrics.insert(k.to_string(), v);
    }
    fn assert(&mut self, ok: bool, what: &str) {
        if ok {
            println!("  PASS: {what}");
        } else {
            println!("  FAIL: {what}");
            self.failures.push(what.to_string());
        }
    }
}

struct HogCycle {
    peak_fp: u64,
    min_held: u64,
    reinflate_s: Option<f64>,
    settled_fp: u64,
    settled_held: u64,
    hog_ok: bool,
}

pub fn run(neb: &Nebula, out_root: &Path, a: Args) -> anyhow::Result<i32> {
    let dir = crate::nebula::run_dir(out_root, "balloon")?;
    println!("balloon suite -> {}", dir.display());
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "scenario": "balloon",
            "host": crate::hostmem::host_meta(),
            "params": { "cycles": a.cycles, "quick": a.quick },
            "started_unix": util::unix_now(),
        }))?,
    )?;

    let mut guard = ConfigGuard::take()?;
    let sampler = Sampler::start(neb.api_port, &dir.join("trace.csv"), Duration::from_secs(2))?;
    let mut rec = Rec {
        metrics: BTreeMap::new(),
        failures: Vec::new(),
    };

    let result = run_checks(neb, &guard, &sampler, &mut rec, &a);

    // Whatever happened, put the user's engine back: original config, fresh
    // boot. Leftover hog containers are --rm and per-name, but sweep anyway.
    neb.cleanup_containers(HOG_NAME);
    guard.restore()?;
    println!("-- restoring engine with original config");
    neb.fresh_up().ok();
    sampler.stop();

    // Persist whatever we measured even if a check errored mid-way.
    std::fs::write(
        dir.join("metrics.json"),
        serde_json::to_string_pretty(&rec.metrics)?,
    )?;
    std::fs::write(
        dir.join("checks.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "failures": rec.failures,
            "passed": rec.failures.is_empty(),
        }))?,
    )?;
    result?;

    let mut exit = if rec.failures.is_empty() { 0 } else { 1 };

    if let Some(base_path) = baseline_path(&a, out_root) {
        if a.write_baseline {
            std::fs::create_dir_all(base_path.parent().unwrap_or(Path::new(".")))?;
            std::fs::write(&base_path, serde_json::to_string_pretty(&rec.metrics)?)?;
            println!("baseline written: {}", base_path.display());
        } else if base_path.exists() {
            let regressions = compare_baseline(&base_path, &rec.metrics)?;
            for r in &regressions {
                println!("REGRESSION: {r}");
            }
            if !regressions.is_empty() {
                exit = 1;
            } else {
                println!("baseline check: all metrics within tolerance");
            }
        } else {
            println!(
                "note: baseline {} not found (run with --write-baseline to create it)",
                base_path.display()
            );
        }
    }

    println!(
        "\nballoon suite: {} checks failed{}",
        rec.failures.len(),
        if rec.failures.is_empty() {
            " — PASS"
        } else {
            ""
        }
    );
    Ok(exit)
}

fn baseline_path(a: &Args, out_root: &Path) -> Option<PathBuf> {
    if let Some(p) = &a.baseline {
        return Some(p.clone());
    }
    // Default per-host baseline beside the results tree: bench/baselines/.
    let host = crate::hostmem::host_meta()
        .get("hostname")
        .and_then(|h| h.as_str())
        .map(String::from)?;
    Some(
        out_root
            .parent()
            .unwrap_or(Path::new("."))
            .join("baselines")
            .join(format!("{host}.json")),
    )
}

fn run_checks(
    neb: &Nebula,
    guard: &ConfigGuard,
    sampler: &Sampler,
    rec: &mut Rec,
    a: &Args,
) -> anyhow::Result<()> {
    // ---- Part A: the 32 GiB engine (same ceiling as the published numbers).
    println!("-- engine @ 32768 MiB max");
    set_max_and_boot(neb, guard, 32768)?;

    println!("-- check 1: idle reclaim");
    sampler.set_phase("idle");
    let (settle_s, idle) = settle(neb, 180)?;
    let (idle_held, idle_fp) = (idle.held_mib(), idle.host_footprint_mib);
    rec.m("idle.settle_s", settle_s);
    rec.m("idle.held_mib", idle_held as f64);
    rec.m("idle.fp_mib", idle_fp as f64);
    rec.assert(idle_held > 32768 * 3 / 4, "idle: balloon holds >75% of max");
    rec.assert(idle_fp < 4096, "idle: host footprint < 4 GiB");

    println!("-- check 2: single 6 GiB hog cycle");
    sampler.set_phase("hog-single");
    let oom0 = neb.oom_count();
    let c = hog_cycle(neb, 6144, 12, idle_held)?;
    rec.m("hog.peak_fp_mib", c.peak_fp as f64);
    rec.m("hog.min_held_mib", c.min_held as f64);
    rec.m("hog.reinflate_s", c.reinflate_s.unwrap_or(999.0));
    rec.m("hog.settled_fp_mib", c.settled_fp as f64);
    rec.assert(c.hog_ok, "hog: container completed (no OOM)");
    rec.assert(
        c.peak_fp > idle_fp + 3072,
        "hog: footprint grew >3 GiB under load",
    );
    rec.assert(
        c.min_held < idle_held,
        "hog: balloon deflated for the workload",
    );
    rec.assert(
        c.reinflate_s.is_some(),
        "hog: balloon re-inflated <=120s after release",
    );
    rec.assert(
        c.settled_fp <= c.peak_fp + 512,
        "hog: settled footprint bounded by workload peak",
    );
    rec.assert(neb.oom_count() == oom0, "hog: no guest OOM kills");
    rec.assert(neb.exec_ok("true"), "hog: guest survived");

    let cycles = if a.quick { 3 } else { a.cycles.max(2) };
    println!("-- check 3: repeat-cycle drift ({cycles} cycles)");
    let mut held_after = Vec::new();
    let mut reinflate = Vec::new();
    for i in 0..cycles {
        sampler.set_phase(&format!("drift-cycle-{i}"));
        let c = hog_cycle(neb, 6144, 12, idle_held)?;
        if !c.hog_ok || c.reinflate_s.is_none() {
            rec.assert(false, &format!("drift: cycle {i} completed + re-inflated"));
            break;
        }
        held_after.push(c.settled_held as f64);
        reinflate.push(c.reinflate_s.unwrap_or(999.0));
        println!(
            "    cycle {i}: settled_held={} MiB, reinflate={:.0}s",
            c.settled_held,
            c.reinflate_s.unwrap_or(999.0)
        );
    }
    if held_after.len() >= 2 {
        let mean = held_after.iter().sum::<f64>() / held_after.len() as f64;
        // Negative slope = the balloon reclaims less each cycle = a leak.
        let degrade_pct =
            -util::slope(&held_after) * (held_after.len() as f64 - 1.0) / mean * 100.0;
        rec.m("drift.held_degrade_pct", degrade_pct);
        rec.m("drift.reinflate_median_s", util::median(&reinflate));
        rec.assert(
            degrade_pct < 5.0,
            "drift: idle balloon level degrades <5% across cycles",
        );
    }

    // ---- Part B: a 16 GiB engine for the pressure-shaped checks.
    println!("-- engine @ 16384 MiB max");
    set_max_and_boot(neb, guard, 16384)?;
    sampler.set_phase("idle-16g");
    let (_, idle16) = settle(neb, 120)?;
    let idle16_held = idle16.held_mib();

    println!("-- check 4: 4 concurrent 2 GiB hogs (staggered 3s)");
    sampler.set_phase("hog-concurrent");
    let oom0 = neb.oom_count();
    let mut handles = Vec::new();
    for i in 0..4 {
        let bin = neb.bin.clone();
        handles.push(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3 * i));
            let n = Nebula {
                bin,
                api_port: 0, // unused on this path
            };
            n.docker(
                &hog_args(&format!("{HOG_NAME}-{i}"), 2048, 15),
                Duration::from_secs(180),
            )
            .map(|o| o.ok())
            .unwrap_or(false)
        }));
    }
    let mut min_held = idle16_held;
    let t0 = Instant::now();
    while handles.iter().any(|h| !h.is_finished()) && t0.elapsed() < Duration::from_secs(240) {
        std::thread::sleep(Duration::from_secs(2));
        if let Ok(s) = neb.stats() {
            min_held = min_held.min(s.held_mib());
        }
    }
    let all_ok = handles.into_iter().all(|h| h.join().unwrap_or(false));
    rec.m("concurrent.min_held_mib", min_held as f64);
    rec.assert(all_ok, "concurrent: all 4 hogs completed");
    rec.assert(
        neb.oom_count() == oom0,
        "concurrent: no guest OOM kills (deflate kept pace)",
    );
    rec.assert(neb.exec_ok("true"), "concurrent: guest survived");

    println!("-- check 5: pressure at the ceiling (~95% of available)");
    sampler.set_phase("hog-ceiling");
    let avail = neb.stats()?.guest_avail_mib();
    let hog_mib = avail * 95 / 100;
    println!("    guest avail {avail} MiB -> hog {hog_mib} MiB");
    let oom0 = neb.oom_count();
    let o = neb.docker(&hog_args(HOG_NAME, hog_mib, 5), Duration::from_secs(300))?;
    let hog_survived = o.ok();
    rec.m("ceiling.hog_mib", hog_mib as f64);
    rec.m("ceiling.hog_survived", if hog_survived { 1.0 } else { 0.0 });
    rec.m(
        "ceiling.oom_kills",
        (neb.oom_count().saturating_sub(oom0)) as f64,
    );
    // The hog is allowed to die; the engine is not.
    rec.assert(neb.exec_ok("true"), "ceiling: guest agent survived");
    rec.assert(
        neb.docker(&["ps", "-q"], Duration::from_secs(30))
            .map(|o| o.ok())
            .unwrap_or(false),
        "ceiling: container engine still answers",
    );

    println!("-- check 6: sawtooth (alternating 4 GiB hog / idle)");
    sampler.set_phase("sawtooth");
    let saw_secs = if a.quick { 180 } else { 600 };
    let oom0 = neb.oom_count();
    let bin = neb.bin.clone();
    let saw = std::thread::spawn(move || {
        let n = Nebula { bin, api_port: 0 };
        let t0 = Instant::now();
        let mut i = 0u32;
        while t0.elapsed() < Duration::from_secs(saw_secs) {
            n.docker(
                &hog_args(&format!("{HOG_NAME}-saw{i}"), 4096, 20),
                Duration::from_secs(120),
            )
            .ok();
            std::thread::sleep(Duration::from_secs(30));
            i += 1;
        }
        i
    });
    let mut targets = Vec::new();
    while !saw.is_finished() {
        std::thread::sleep(Duration::from_secs(2));
        if let Ok(s) = neb.stats() {
            targets.push(s.balloon_target_mib);
        }
    }
    let saw_cycles = saw.join().unwrap_or(0).max(1);
    let resizes = targets.windows(2).filter(|w| w[0] != w[1]).count();
    rec.m("sawtooth.cycles", saw_cycles as f64);
    rec.m("sawtooth.resizes", resizes as f64);
    rec.m(
        "sawtooth.resizes_per_cycle",
        resizes as f64 / saw_cycles as f64,
    );
    rec.assert(
        neb.oom_count() == oom0,
        "sawtooth: no guest OOM kills across the window",
    );
    rec.assert(
        (resizes as f64 / saw_cycles as f64) <= 8.0,
        "sawtooth: controller does not thrash (<=8 resizes/cycle)",
    );
    rec.assert(neb.exec_ok("true"), "sawtooth: guest survived");

    Ok(())
}

/// `--shm-size` hog: dd into /dev/shm touches and *holds* the pages, the
/// same mechanism test-phase4.sh characterized the balloon with.
fn hog_args(name: &str, mib: u64, hold_s: u64) -> Vec<String> {
    vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        name.into(),
        format!("--shm-size={}m", mib + 512),
        "alpine:3.20".into(),
        "sh".into(),
        "-c".into(),
        format!("dd if=/dev/zero of=/dev/shm/h bs=1M count={mib} status=none && sleep {hold_s}"),
    ]
}

fn hog_cycle(neb: &Nebula, mib: u64, hold_s: u64, idle_held: u64) -> anyhow::Result<HogCycle> {
    let pre = neb.stats()?;
    let bin = neb.bin.clone();
    let args = hog_args(HOG_NAME, mib, hold_s);
    let hog = std::thread::spawn(move || {
        let n = Nebula { bin, api_port: 0 };
        n.docker(&args, Duration::from_secs(300))
            .map(|o| o.ok())
            .unwrap_or(false)
    });
    let mut peak_fp = pre.host_footprint_mib;
    let mut min_held = pre.held_mib();
    while !hog.is_finished() {
        std::thread::sleep(Duration::from_secs(2));
        if let Ok(s) = neb.stats() {
            peak_fp = peak_fp.max(s.host_footprint_mib);
            min_held = min_held.min(s.held_mib());
        }
    }
    let hog_ok = hog.join().unwrap_or(false);

    // Re-inflate: back to near idle level within 120s (45s surplus window +
    // slack), the same contract as phase 4.
    let t0 = Instant::now();
    let mut reinflate_s = None;
    while t0.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(Duration::from_secs(2));
        if let Ok(s) = neb.stats() {
            if s.held_mib() >= idle_held.saturating_sub(2048) {
                reinflate_s = Some(t0.elapsed().as_secs_f64());
                break;
            }
        }
    }
    let settled = neb.stats()?;
    Ok(HogCycle {
        peak_fp,
        min_held,
        reinflate_s,
        settled_fp: settled.host_footprint_mib,
        settled_held: settled.held_mib(),
        hog_ok,
    })
}

fn set_max_and_boot(neb: &Nebula, guard: &ConfigGuard, mib: u64) -> anyhow::Result<()> {
    guard.set_max_ram(mib)?;
    neb.fresh_up()
        .with_context(|| format!("fresh up @ max_ram {mib}"))?;
    Ok(())
}

/// Poll until the balloon level is stable (3 consecutive samples within
/// 256 MiB) or `timeout_s` elapses. Never returns before 60s: right after
/// boot the balloon sits stably at ~0 through the controller's first surplus
/// window, which would read as "settled" otherwise. Returns (s, last stats).
fn settle(neb: &Nebula, timeout_s: u64) -> anyhow::Result<(f64, crate::api::Stats)> {
    let t0 = Instant::now();
    let mut stable = 0;
    let mut last_held = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let s = neb.stats()?;
        let held = s.held_mib();
        if held.abs_diff(last_held) <= 256 && held > 0 && t0.elapsed() > Duration::from_secs(60) {
            stable += 1;
        } else {
            stable = 0;
        }
        last_held = held;
        if stable >= 3 || t0.elapsed() > Duration::from_secs(timeout_s) {
            return Ok((t0.elapsed().as_secs_f64(), s));
        }
    }
}

fn compare_baseline(path: &Path, metrics: &BTreeMap<String, f64>) -> anyhow::Result<Vec<String>> {
    let base: BTreeMap<String, f64> =
        serde_json::from_str(&std::fs::read_to_string(path).context("read baseline")?)?;
    let mut regressions = Vec::new();
    for (k, bv) in &base {
        let Some(nv) = metrics.get(k) else {
            regressions.push(format!("{k}: missing from this run (baseline {bv:.1})"));
            continue;
        };
        // Small numbers get an absolute band (rel-diff on ~0 is noise);
        // everything else ±15%.
        let ok = if bv.abs() < 5.0 {
            (nv - bv).abs() <= 2.0
        } else {
            ((nv - bv) / bv).abs() <= 0.15
        };
        if !ok {
            regressions.push(format!("{k}: {nv:.1} vs baseline {bv:.1}"));
        }
    }
    Ok(regressions)
}
