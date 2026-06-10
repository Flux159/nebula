//! `nebula setup <tool>` / `nebula revert <tool>` — point docker/nerdctl/
//! kubectl at Nebula, and put them back *exactly* as they were.
//!
//! Safety rules (these tools can target other VMs or production):
//! - never delete or rewrite a user's pre-existing contexts/config beyond the
//!   single field that selects the active target
//! - revert is a stack (repeated `use` calls don't lose history) and idempotent
//! - switching away from something that looks remote warns loudly

use std::path::PathBuf;

use anyhow::{bail, Context};
use serde_json::{json, Value};
use sha2::Digest;

use crate::client;

const DOCKER_CONTEXT_NAME: &str = "nebula";

pub fn setup_tool(tool: &str) -> anyhow::Result<()> {
    match tool {
        "docker" => use_docker(),
        "nerdctl" => use_nerdctl(),
        "kubectl" => crate::kube::setup_kubectl(),
        other => bail!("unknown tool `{other}` (expected docker, nerdctl, or kubectl)"),
    }
}

pub fn revert_tool(tool: &str) -> anyhow::Result<()> {
    match tool {
        "docker" => revert_docker(),
        "nerdctl" => revert_nerdctl(),
        "kubectl" => crate::kube::revert_kubectl(),
        "--all" | "all" => {
            revert_docker()?;
            revert_nerdctl()?;
            crate::kube::revert_kubectl()
        }
        other => bail!("unknown tool `{other}`"),
    }
}

// --- docker -----------------------------------------------------------------

fn docker_dir() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(".docker"))
}

fn docker_config_path() -> anyhow::Result<PathBuf> {
    Ok(docker_dir()?.join("config.json"))
}

fn read_json(path: &std::path::Path) -> anyhow::Result<Value> {
    if path.is_file() {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("parsing {}", path.display()))?)
    } else {
        Ok(json!({}))
    }
}

/// Docker context metadata lives at contexts/meta/<sha256(name)>/meta.json.
fn docker_context_meta_dir(name: &str) -> anyhow::Result<PathBuf> {
    let digest = sha2::Sha256::digest(name.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(docker_dir()?.join("contexts/meta").join(hex))
}

fn nebula_docker_host() -> anyhow::Result<String> {
    Ok(format!(
        "unix://{}",
        client::nebula_home()?.join("run/docker.sock").display()
    ))
}

fn use_docker() -> anyhow::Result<()> {
    // 1. Create/refresh our context (never touches other contexts).
    let meta_dir = docker_context_meta_dir(DOCKER_CONTEXT_NAME)?;
    std::fs::create_dir_all(&meta_dir)?;
    let meta = json!({
        "Name": DOCKER_CONTEXT_NAME,
        "Metadata": { "Description": "Nebula Vessel" },
        "Endpoints": { "docker": { "Host": nebula_docker_host()?, "SkipTLSVerify": false } }
    });
    std::fs::write(
        meta_dir.join("meta.json"),
        serde_json::to_vec_pretty(&meta)?,
    )?;

    // 2. Flip currentContext, recording the previous value on the revert stack.
    let cfg_path = docker_config_path()?;
    let mut cfg = read_json(&cfg_path)?;
    let previous = cfg
        .get("currentContext")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    if previous.as_deref() == Some(DOCKER_CONTEXT_NAME) {
        println!("docker already points at nebula");
        return Ok(());
    }
    warn_if_remote_docker(previous.as_deref());
    push_prev("docker", &previous)?;

    cfg["currentContext"] = json!(DOCKER_CONTEXT_NAME);
    write_json_preserving(&cfg_path, &cfg)?;
    println!(
        "docker → nebula (was: {})",
        previous.as_deref().unwrap_or("default (unset)")
    );
    println!("revert anytime with: nebula revert docker");
    Ok(())
}

fn revert_docker() -> anyhow::Result<()> {
    let Some(previous) = pop_prev("docker")? else {
        println!("docker: nothing to revert");
        return Ok(());
    };
    let cfg_path = docker_config_path()?;
    let mut cfg = read_json(&cfg_path)?;
    match &previous {
        Some(name) => cfg["currentContext"] = json!(name),
        None => {
            // Previous state was "no current context" (docker default).
            cfg.as_object_mut().map(|o| o.remove("currentContext"));
        }
    }
    write_json_preserving(&cfg_path, &cfg)?;
    println!(
        "docker → {}",
        previous.as_deref().unwrap_or("default (unset)")
    );
    Ok(())
}

fn warn_if_remote_docker(context: Option<&str>) {
    let Some(name) = context else { return };
    let Ok(meta_dir) = docker_context_meta_dir(name) else {
        return;
    };
    let Ok(meta) = read_json(&meta_dir.join("meta.json")) else {
        return;
    };
    if let Some(host) = meta
        .pointer("/Endpoints/docker/Host")
        .and_then(|v| v.as_str())
    {
        if host.starts_with("ssh://") || host.starts_with("tcp://") {
            eprintln!("\n  ⚠ switching away from REMOTE docker context `{name}` ({host})");
            eprintln!("    `nebula revert docker` restores it.\n");
        }
    }
}

// --- nerdctl ----------------------------------------------------------------
//
// nerdctl has no native macOS binary; the config below prepares the host side
// (address of our containerd socket) for guest-exec'd or future use. See
// tasks/issues.md.

fn nerdctl_toml() -> anyhow::Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").context("HOME not set")?)
            .join(".config/nerdctl/nerdctl.toml"),
    )
}

fn use_nerdctl() -> anyhow::Result<()> {
    let path = nerdctl_toml()?;
    let previous = path
        .is_file()
        .then(|| std::fs::read_to_string(&path))
        .transpose()?;
    push_prev("nerdctl", &previous)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let addr = client::nebula_home()?.join("run/containerd.sock");
    std::fs::write(
        &path,
        format!("# written by `nebula setup nerdctl` — revert with `nebula revert nerdctl`\naddress = \"unix://{}\"\nnamespace = \"default\"\n", addr.display()),
    )?;
    println!("nerdctl config → nebula containerd ({})", path.display());
    println!(
        "note: use the host nerdctl against this socket (the guest image no longer ships one)"
    );
    Ok(())
}

fn revert_nerdctl() -> anyhow::Result<()> {
    let Some(previous) = pop_prev("nerdctl")? else {
        println!("nerdctl: nothing to revert");
        return Ok(());
    };
    let path = nerdctl_toml()?;
    match previous {
        Some(contents) => std::fs::write(&path, contents)?,
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
    println!("nerdctl config restored");
    Ok(())
}

// --- revert stack -----------------------------------------------------------

pub fn push_prev_pub(tool: &str, value: &Option<String>) -> anyhow::Result<()> {
    push_prev(tool, value)
}

pub fn pop_prev_pub(tool: &str) -> anyhow::Result<Option<Option<String>>> {
    pop_prev(tool)
}

fn stack_path(tool: &str) -> anyhow::Result<PathBuf> {
    let dir = client::nebula_home()?.join("contexts");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{tool}.prev.json")))
}

/// Stack of previous states. `None` entries mean "the tool had no explicit
/// setting" (e.g. docker without a currentContext).
fn load_stack(tool: &str) -> anyhow::Result<Vec<Option<String>>> {
    let path = stack_path(tool)?;
    if !path.is_file() {
        return Ok(vec![]);
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

fn push_prev(tool: &str, value: &Option<String>) -> anyhow::Result<()> {
    let mut stack = load_stack(tool)?;
    stack.push(value.clone());
    std::fs::write(stack_path(tool)?, serde_json::to_vec_pretty(&stack)?)?;
    Ok(())
}

fn pop_prev(tool: &str) -> anyhow::Result<Option<Option<String>>> {
    let mut stack = load_stack(tool)?;
    let top = stack.pop();
    std::fs::write(stack_path(tool)?, serde_json::to_vec_pretty(&stack)?)?;
    Ok(top)
}

/// Write JSON without clobbering unrelated keys (we always re-serialize the
/// full Value we read, so user fields survive; pretty-print like docker does).
fn write_json_preserving(path: &std::path::Path, value: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

/// Which tools currently point at nebula (for `nebula status`).
pub fn pointing_at_nebula() -> Vec<&'static str> {
    let mut out = Vec::new();
    if let Ok(cfg_path) = docker_config_path() {
        if let Ok(cfg) = read_json(&cfg_path) {
            if cfg.get("currentContext").and_then(|v| v.as_str()) == Some(DOCKER_CONTEXT_NAME) {
                out.push("docker");
            }
        }
    }
    if let Ok(path) = nerdctl_toml() {
        if let Ok(s) = std::fs::read_to_string(path) {
            if s.contains(".nebula/run/containerd.sock") {
                out.push("nerdctl");
            }
        }
    }
    out
}
