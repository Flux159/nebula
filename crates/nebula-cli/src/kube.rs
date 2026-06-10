//! `nebula use kubectl` / `nebula revert kubectl`: a local k3s cluster with
//! the same lossless-revert discipline as docker — extra guardrails because
//! the previous context may be production.

use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use nebula_core::proto::*;
use serde_yaml::Value;

use crate::client;

const CONTEXT_NAME: &str = "nebula";

pub fn use_kubectl() -> anyhow::Result<()> {
    // 1. Start k3s in the Vessel (persisted across boots).
    let resp = client::request(&DaemonRequest::Agent {
        request: AgentRequest::ServiceCtl {
            name: "k3s".into(),
            action: ServiceAction::Start,
        },
    })?;
    if let DaemonResponse::Error { message } = resp {
        bail!("starting k3s: {message}");
    }
    println!("k3s starting in the Vessel…");

    // 2. Wait for its kubeconfig (first boot takes ~20-30s).
    let kubeconfig = wait_for_guest_kubeconfig(Duration::from_secs(120))?;

    // 3. Rewrite the server address to the guest IP (in the cert SANs via
    //    --tls-san) and merge into ~/.kube/config as cluster/user/context
    //    `nebula`, never touching other entries.
    let guest_ip = guest_ip()?;
    let merged = merge_kubeconfig(&kubeconfig, &guest_ip)?;
    if let Some(prev) = merged {
        println!("kubectl → nebula (was: {prev})");
        if prev != CONTEXT_NAME && looks_remote(&prev) {
            eprintln!("\n  ⚠ previous kubectl context `{prev}` looks like a real cluster.");
            eprintln!("    `nebula revert kubectl` restores it exactly.\n");
        }
    }
    // 4. Smoke check.
    println!("waiting for node to be Ready…");
    wait_node_ready(Duration::from_secs(120))?;
    println!("kubectl ready — try: kubectl get nodes");
    Ok(())
}

pub fn revert_kubectl() -> anyhow::Result<()> {
    let Some(previous) = crate::contexts::pop_prev_pub("kubectl")? else {
        println!("kubectl: nothing to revert");
        return Ok(());
    };
    let path = kubeconfig_path()?;
    let mut root = read_yaml(&path)?;
    match &previous {
        Some(name) => root["current-context"] = Value::String(name.clone()),
        None => {
            if let Value::Mapping(m) = &mut root {
                m.remove(Value::String("current-context".into()));
            }
        }
    }
    write_yaml(&path, &root)?;
    println!(
        "kubectl → {}",
        previous.as_deref().unwrap_or("default (unset)")
    );
    Ok(())
}

fn kubeconfig_path() -> anyhow::Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("KUBECONFIG") {
        let first = std::env::split_paths(&p).next();
        if let Some(first) = first {
            return Ok(first);
        }
    }
    Ok(
        std::path::PathBuf::from(std::env::var_os("HOME").context("HOME not set")?)
            .join(".kube/config"),
    )
}

fn read_yaml(path: &std::path::Path) -> anyhow::Result<Value> {
    if path.is_file() {
        Ok(serde_yaml::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("parsing {}", path.display()))?)
    } else {
        Ok(serde_yaml::from_str("apiVersion: v1\nkind: Config\n")?)
    }
}

fn write_yaml(path: &std::path::Path, value: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_yaml::to_string(value)?)?;
    Ok(())
}

fn guest_ip() -> anyhow::Result<String> {
    match client::request(&DaemonRequest::Status)? {
        DaemonResponse::Status(s) => s
            .agent
            .and_then(|a| a.ip)
            .context("guest IP unknown (agent unhealthy?)"),
        _ => bail!("status unavailable"),
    }
}

fn agent_exec(cmd: &str, args: &[&str]) -> anyhow::Result<ExecResult> {
    let req = DaemonRequest::Agent {
        request: AgentRequest::Exec {
            cmd: cmd.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            timeout_ms: 20_000,
        },
    };
    match client::request(&req)? {
        DaemonResponse::Agent {
            response: AgentResponse::Exec(r),
        } => Ok(r),
        DaemonResponse::Agent {
            response: AgentResponse::Error { message },
        } => bail!("{message}"),
        DaemonResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected response: {other:?}"),
    }
}

fn wait_for_guest_kubeconfig(timeout: Duration) -> anyhow::Result<String> {
    let start = Instant::now();
    loop {
        if let Ok(r) = agent_exec("cat", &["/etc/rancher/k3s/k3s.yaml"]) {
            if r.exit_code == 0 && r.stdout.contains("clusters:") {
                return Ok(r.stdout);
            }
        }
        if start.elapsed() > timeout {
            bail!("k3s did not produce a kubeconfig within {timeout:?} (see nebula exec cat /var/log/k3s.log)");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn wait_node_ready(timeout: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if let Ok(r) = agent_exec(
            "/usr/local/bin/k3s",
            &["kubectl", "get", "nodes", "--no-headers"],
        ) {
            if r.exit_code == 0 && r.stdout.contains(" Ready") {
                return Ok(());
            }
        }
        if start.elapsed() > timeout {
            bail!("node did not become Ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Merge the guest kubeconfig into the user's, as `nebula` entries. Returns
/// the previous current-context (recorded for revert) on change.
fn merge_kubeconfig(guest_yaml: &str, guest_ip: &str) -> anyhow::Result<Option<String>> {
    let guest: Value = serde_yaml::from_str(guest_yaml)?;
    let cluster_data = guest["clusters"][0]["cluster"].clone();
    let user_data = guest["users"][0]["user"].clone();
    anyhow::ensure!(!cluster_data.is_null(), "guest kubeconfig missing cluster");

    let mut cluster = cluster_data;
    cluster["server"] = Value::String(format!("https://{guest_ip}:6443"));

    let path = kubeconfig_path()?;
    let mut root = read_yaml(&path)?;

    upsert_named(&mut root, "clusters", "cluster", cluster)?;
    upsert_named(&mut root, "users", "user", user_data)?;
    let ctx: Value =
        serde_yaml::from_str(&format!("cluster: {CONTEXT_NAME}\nuser: {CONTEXT_NAME}\n"))?;
    upsert_named(&mut root, "contexts", "context", ctx)?;

    let previous = root["current-context"].as_str().map(str::to_string);
    if previous.as_deref() == Some(CONTEXT_NAME) {
        write_yaml(&path, &root)?;
        return Ok(None);
    }
    crate::contexts::push_prev_pub("kubectl", &previous)?;
    root["current-context"] = Value::String(CONTEXT_NAME.into());
    write_yaml(&path, &root)?;
    Ok(Some(previous.unwrap_or_else(|| "default (unset)".into())))
}

/// Insert or replace the entry named `nebula` in a kubeconfig list section,
/// leaving every other entry untouched.
fn upsert_named(
    root: &mut Value,
    section: &str,
    inner_key: &str,
    data: Value,
) -> anyhow::Result<()> {
    let mut entry = serde_yaml::Mapping::new();
    entry.insert(
        Value::String("name".into()),
        Value::String(CONTEXT_NAME.into()),
    );
    entry.insert(Value::String(inner_key.into()), data);
    let entry = Value::Mapping(entry);

    if root.get(section).and_then(|v| v.as_sequence()).is_none() {
        root[section] = Value::Sequence(vec![]);
    }
    let seq = root[section].as_sequence_mut().unwrap();
    if let Some(existing) = seq
        .iter_mut()
        .find(|e| e["name"].as_str() == Some(CONTEXT_NAME))
    {
        *existing = entry;
    } else {
        seq.push(entry);
    }
    Ok(())
}

fn looks_remote(context: &str) -> bool {
    // Heuristic only: locals are well-known names.
    !matches!(
        context,
        "docker-desktop" | "minikube" | "kind-kind" | "rancher-desktop" | "default (unset)"
    )
}
