//! Render bench/results/* into bench/report/: report.md + SVG charts.

use crate::svg::{line_chart, Series};
use anyhow::Context;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

struct Run {
    dir_name: String,
    meta: Value,
    results: Option<Value>, // scale scenarios
    metrics: Option<Value>, // balloon
    checks: Option<Value>,
}

pub fn run(input: &Path, out: &Path) -> anyhow::Result<i32> {
    let mut runs: Vec<Run> = Vec::new();
    for entry in std::fs::read_dir(input)
        .with_context(|| format!("read {}", input.display()))?
        .flatten()
    {
        let dir = entry.path();
        let meta_path = dir.join("meta.json");
        if !meta_path.is_file() {
            continue;
        }
        let read_json = |name: &str| -> Option<Value> {
            std::fs::read_to_string(dir.join(name))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
        };
        runs.push(Run {
            dir_name: entry.file_name().to_string_lossy().into_owned(),
            meta: read_json("meta.json").unwrap_or(Value::Null),
            results: read_json("results.json"),
            metrics: read_json("metrics.json"),
            checks: read_json("checks.json"),
        });
    }
    runs.sort_by(|a, b| a.dir_name.cmp(&b.dir_name));
    if runs.is_empty() {
        println!("no runs found under {}", input.display());
        return Ok(1);
    }
    std::fs::create_dir_all(out)?;

    let mut md = String::from("# Nebula battle-test report\n\n");
    let _ = writeln!(
        md,
        "Generated from `{}`. Newest run of each scenario wins.\n",
        input.display()
    );

    container_section(&runs, out, &mut md)?;
    vessel_section(&runs, out, &mut md)?;
    balloon_section(&runs, &mut md)?;

    std::fs::write(out.join("report.md"), &md)?;
    println!("report -> {}", out.join("report.md").display());
    Ok(0)
}

fn scenario_of(r: &Run) -> &str {
    r.meta
        .get("scenario")
        .and_then(|s| s.as_str())
        .unwrap_or("")
}

/// Newest run per (scenario, flavor-ish key) — dir names sort by timestamp.
fn latest<'a>(runs: &'a [Run], scenario: &str) -> Vec<&'a Run> {
    let mut by_key: BTreeMap<String, &Run> = BTreeMap::new();
    for r in runs.iter().filter(|r| scenario_of(r) == scenario) {
        let flavor = r
            .meta
            .get("flavor")
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        by_key.insert(flavor, r); // later (newer) dirs overwrite
    }
    by_key.into_values().collect()
}

fn container_section(runs: &[Run], out: &Path, md: &mut String) -> anyhow::Result<()> {
    let latest_runs = latest(runs, "container-scale");
    if latest_runs.is_empty() {
        return Ok(());
    }
    md.push_str("## Containers in vessel 0 vs max RAM\n\n");
    let mut series: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    md.push_str("| flavor | workload | max RAM (MiB) | containers | stop reason | errors |\n");
    md.push_str("|---|---|---|---|---|---|\n");
    for run in &latest_runs {
        let Some(points) = run.results.as_ref().and_then(|r| r.as_array()) else {
            continue;
        };
        for p in points {
            let flavor = p["flavor"].as_str().unwrap_or("?");
            let workload = p["workload"].as_str().unwrap_or("?");
            let mr = p["max_ram_mib"].as_f64().unwrap_or(0.0);
            let n = p["n_running_final"].as_f64().unwrap_or(0.0);
            let _ = writeln!(
                md,
                "| {flavor} | {workload} | {mr:.0} | {n:.0} | {} | {} |",
                p["stop_reason"].as_str().unwrap_or("?"),
                p["container_errors"].as_u64().unwrap_or(0),
            );
            series
                .entry(format!("{flavor}/{workload}"))
                .or_default()
                .push((mr / 1024.0, n));
        }
    }
    let chart = line_chart(
        "Containers in vessel 0 vs configured max RAM",
        "max RAM (GiB)",
        "containers running",
        &series
            .into_iter()
            .map(|(name, points)| Series { name, points })
            .collect::<Vec<_>>(),
    );
    std::fs::write(out.join("containers-vs-maxram.svg"), chart)?;
    md.push_str("\n![containers vs max RAM](containers-vs-maxram.svg)\n\n");
    Ok(())
}

fn vessel_section(runs: &[Run], out: &Path, md: &mut String) -> anyhow::Result<()> {
    let latest_runs = latest(runs, "vessel-scale");
    if latest_runs.is_empty() {
        return Ok(());
    }
    md.push_str("## Concurrent vessels vs per-vessel RAM\n\n");
    let mut count_series: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    md.push_str(
        "| backend | mem (MiB) | vessels | stop reason | boot first→last (ms) | host cost/vessel (MiB) |\n",
    );
    md.push_str("|---|---|---|---|---|---|\n");
    for run in &latest_runs {
        let Some(points) = run.results.as_ref().and_then(|r| r.as_array()) else {
            continue;
        };
        for p in points {
            let backend = p["backend"].as_str().unwrap_or("?");
            let mem = p["mem_mib"].as_f64().unwrap_or(0.0);
            let n = p["n_max"].as_f64().unwrap_or(0.0);
            let _ = writeln!(
                md,
                "| {backend} | {mem:.0} | {n:.0} | {} | {:.0}→{:.0} | {:.0} |",
                p["stop_reason"].as_str().unwrap_or("?"),
                p["boot_ms_first"].as_f64().unwrap_or(0.0),
                p["boot_ms_last"].as_f64().unwrap_or(0.0),
                p["host_cost_per_vessel_mib"].as_f64().unwrap_or(0.0),
            );
            count_series
                .entry(backend.to_string())
                .or_default()
                .push((mem, n));
        }
    }
    let chart = line_chart(
        "Concurrent vessels vs per-vessel max RAM",
        "per-vessel RAM (MiB)",
        "vessels",
        &count_series
            .into_iter()
            .map(|(name, points)| Series { name, points })
            .collect::<Vec<_>>(),
    );
    std::fs::write(out.join("vessels-vs-mem.svg"), chart)?;
    md.push_str("\n![vessels vs per-vessel RAM](vessels-vs-mem.svg)\n\n");
    Ok(())
}

fn balloon_section(runs: &[Run], md: &mut String) -> anyhow::Result<()> {
    let Some(run) = runs.iter().rfind(|r| scenario_of(r) == "balloon") else {
        return Ok(());
    };
    md.push_str("## Balloon contract\n\n");
    if let Some(checks) = &run.checks {
        let passed = checks["passed"].as_bool().unwrap_or(false);
        let _ = writeln!(
            md,
            "Latest run `{}`: **{}**\n",
            run.dir_name,
            if passed { "PASS" } else { "FAIL" }
        );
        if let Some(fails) = checks["failures"].as_array() {
            for f in fails {
                let _ = writeln!(md, "- FAIL: {}", f.as_str().unwrap_or("?"));
            }
        }
    }
    if let Some(metrics) = run.metrics.as_ref().and_then(|m| m.as_object()) {
        md.push_str("\n| metric | value |\n|---|---|\n");
        for (k, v) in metrics {
            let _ = writeln!(md, "| {k} | {:.1} |", v.as_f64().unwrap_or(0.0));
        }
    }
    md.push('\n');
    Ok(())
}
