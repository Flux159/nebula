//! helm-slim — a standalone, Helm-compatible CLI over the nebula slim engine.
//! Renders charts (Go-template + sprig subset) and applies them through the
//! slim-kube facade. No Tiller, no cluster state. HOST binary.
//!
//! Nebula's `nebula helm` wrapper execs this when the engine is slim.
//! Verbs: install / upgrade / template / uninstall / list / version.

use slim_client::http::Client;
use slim_helm::{build_values, render, Chart, Helm, RenderOptions};
use std::path::Path;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&argv));
}

fn run(argv: &[String]) -> i32 {
    let mut namespace = String::new();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-n" | "--namespace" => {
                namespace = argv.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            s if s.starts_with("--namespace=") => {
                namespace = s["--namespace=".len()..].to_string();
                i += 1;
            }
            "version" if rest.is_empty() => {
                println!("helm-slim version slim-0.1.0 (nebula-slim)");
                return 0;
            }
            "--help" | "-h" if rest.is_empty() => {
                usage();
                return 0;
            }
            _ => {
                rest.push(argv[i].clone());
                i += 1;
            }
        }
    }
    if rest.is_empty() {
        usage();
        return 0;
    }
    let verb = rest[0].as_str();
    let args = &rest[1..];
    let result = match verb {
        "install" => cmd_install(&namespace, args, false),
        "upgrade" => cmd_install(&namespace, args, true),
        "template" => cmd_template(&namespace, args),
        "uninstall" | "delete" => cmd_uninstall(&namespace, args),
        "list" | "ls" => cmd_list(&namespace),
        other => Err(format!("unknown command \"{other}\"")),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

struct InstallArgs {
    release: String,
    chart: String,
    values_files: Vec<String>,
    sets: Vec<String>,
    dry_run: bool,
}

fn parse_install(args: &[String]) -> Result<InstallArgs, String> {
    let mut values_files = Vec::new();
    let mut sets = Vec::new();
    let mut dry_run = false;
    let mut positional = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-f" | "--values" => {
                values_files.push(args.get(i + 1).cloned().ok_or("-f needs a path")?);
                i += 2;
            }
            "--set" => {
                sets.push(args.get(i + 1).cloned().ok_or("--set needs k=v")?);
                i += 2;
            }
            s if s.starts_with("--set=") => {
                sets.push(s["--set=".len()..].to_string());
                i += 1;
            }
            s if s.starts_with("--values=") => {
                values_files.push(s["--values=".len()..].to_string());
                i += 1;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            _ => {
                positional.push(args[i].clone());
                i += 1;
            }
        }
    }
    let (release, chart) = match positional.len() {
        2 => (positional[0].clone(), positional[1].clone()),
        1 => (positional[0].clone(), positional[0].clone()), // generate-name-ish
        _ => return Err("requires RELEASE and CHART".into()),
    };
    Ok(InstallArgs { release, chart, values_files, sets, dry_run })
}

fn cmd_install(ns: &str, args: &[String], upgrade: bool) -> Result<i32, String> {
    let ia = parse_install(args)?;
    let chart = Chart::load(Path::new(&ia.chart)).map_err(|e| e.to_string())?;
    let values = build_values(&chart, &ia.values_files, &ia.sets).map_err(|e| e.to_string())?;
    let namespace = if ns.is_empty() { "default" } else { ns };

    if ia.dry_run {
        let opts = RenderOptions { release: ia.release.clone(), namespace: namespace.to_string(), is_upgrade: upgrade };
        let manifests = render(&chart, &values, &opts).map_err(|e| e.to_string())?;
        print!("{manifests}");
        return Ok(0);
    }

    let client = Client::discover();
    let helm = Helm::new(&client, namespace);
    let mut out = |s: &str| print!("{s}");
    helm.install(&ia.release, &chart, &values, &mut out).map_err(|e| e.to_string())?;
    Ok(0)
}

fn cmd_template(ns: &str, args: &[String]) -> Result<i32, String> {
    let ia = parse_install(args)?;
    let chart = Chart::load(Path::new(&ia.chart)).map_err(|e| e.to_string())?;
    let values = build_values(&chart, &ia.values_files, &ia.sets).map_err(|e| e.to_string())?;
    let namespace = if ns.is_empty() { "default" } else { ns };
    let opts = RenderOptions { release: ia.release, namespace: namespace.to_string(), is_upgrade: false };
    let manifests = render(&chart, &values, &opts).map_err(|e| e.to_string())?;
    print!("{manifests}");
    Ok(0)
}

fn cmd_uninstall(ns: &str, args: &[String]) -> Result<i32, String> {
    let release = args.first().ok_or("uninstall requires a release name")?;
    let client = Client::discover();
    let helm = Helm::new(&client, ns);
    let mut out = |s: &str| print!("{s}");
    helm.uninstall(release, &mut out).map_err(|e| e.to_string())?;
    Ok(0)
}

fn cmd_list(ns: &str) -> Result<i32, String> {
    let client = Client::discover();
    let helm = Helm::new(&client, ns);
    let releases = helm.list().map_err(|e| e.to_string())?;
    println!("{:<20} {:<12} {:<20} {:<12}", "NAME", "NAMESPACE", "CHART", "STATUS");
    for r in releases {
        println!(
            "{:<20} {:<12} {:<20} {:<12}",
            r.name,
            r.namespace,
            format!("{}-{}", r.chart, r.version),
            "deployed"
        );
    }
    Ok(0)
}

fn usage() {
    print!(
        "helm-slim — Helm facade over the nebula slim engine\n\n\
        Usage: helm-slim [-n NS] VERB ...\n\n\
        Verbs:\n\
        \x20 install NAME CHART [-f values.yaml] [--set k=v] [--dry-run]\n\
        \x20 upgrade NAME CHART [...]            Re-render and re-apply\n\
        \x20 template [NAME] CHART [...]         Render manifests to stdout\n\
        \x20 uninstall NAME                      Delete a release's objects\n\
        \x20 list                                List installed releases\n\n\
        CHART is a local directory or a .tgz. Manifests apply via the slim k8s\n\
        facade — kinds outside it (CRDs, etc.) are skipped with a warning.\n"
    );
}
