//! `nebula use kubectl` / `nebula revert kubectl`: a local k3s cluster with
//! the same lossless-revert discipline as docker — extra guardrails because
//! the previous context may be production.

use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use nebula_core::proto::*;
use serde_yaml::Value;

use crate::client;

const CONTEXT_NAME: &str = "nebula";

/// Bring k3s up (idempotent) and return the path of a standalone kubeconfig
/// containing only the nebula cluster — used by `nebula kubectl|helm` and as
/// the source for the `setup` merge.
fn instance_net() -> InstanceNet {
    match client::request(&DaemonRequest::Status) {
        Ok(DaemonResponse::Status(s)) => s.net,
        _ => InstanceNet::default(),
    }
}

pub fn ensure_ready() -> anyhow::Result<std::path::PathBuf> {
    let net = instance_net();
    let k8s_port = net.k8s_port;
    let resp = client::request(&DaemonRequest::Agent {
        request: AgentRequest::ServiceCtl {
            name: "k3s".into(),
            action: ServiceAction::Start,
        },
    })?;
    if let DaemonResponse::Error { message } = resp {
        bail!("starting k3s: {message}");
    }

    // Guest kubeconfig (first boot takes ~20-30s), node Ready, host forward.
    let guest_yaml = wait_for_guest_kubeconfig(Duration::from_secs(120))?;
    wait_node_ready(Duration::from_secs(120))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if std::net::TcpStream::connect_timeout(
            &format!("127.0.0.1:{k8s_port}").parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok()
        {
            break;
        }
        if Instant::now() > deadline {
            bail!("host forward 127.0.0.1:{k8s_port} did not come up within 30s");
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // Standalone kubeconfig with the server pointed at the stable forward.
    let mut doc: Value = serde_yaml::from_str(&guest_yaml)?;
    doc["clusters"][0]["cluster"]["server"] =
        Value::String(format!("https://127.0.0.1:{k8s_port}"));
    doc["clusters"][0]["name"] = Value::String(CONTEXT_NAME.into());
    doc["contexts"][0]["context"]["cluster"] = Value::String(CONTEXT_NAME.into());
    doc["contexts"][0]["context"]["user"] = Value::String(CONTEXT_NAME.into());
    doc["contexts"][0]["name"] = Value::String(CONTEXT_NAME.into());
    doc["users"][0]["name"] = Value::String(CONTEXT_NAME.into());
    doc["current-context"] = Value::String(CONTEXT_NAME.into());
    let path = client::nebula_home()?.join("kubeconfig");
    std::fs::write(&path, serde_yaml::to_string(&doc)?)?;
    Ok(path)
}

pub fn setup_kubectl() -> anyhow::Result<()> {
    println!("k3s starting in the Vessel…");
    let standalone = ensure_ready()?;
    let guest_yaml = std::fs::read_to_string(&standalone)?;

    // Merge into ~/.kube/config as cluster/user/context `nebula`, never
    // touching other entries; record the previous context for revert.
    let merged = merge_kubeconfig(&guest_yaml, "127.0.0.1")?;
    if let Some(prev) = merged {
        println!("kubectl → nebula (was: {prev})");
        if prev != CONTEXT_NAME && looks_remote(&prev) {
            eprintln!("\n  ⚠ previous kubectl context `{prev}` looks like a real cluster.");
            eprintln!("    `nebula revert kubectl` restores it exactly.\n");
        }
    }
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
    let k8s_port = instance_net().k8s_port;
    cluster["server"] = Value::String(format!("https://{guest_ip}:{k8s_port}"));

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
