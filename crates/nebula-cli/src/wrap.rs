//! `nebula docker|kubectl|helm <args…>` — run a tool against Nebula for just
//! this one invocation, without touching the user's contexts at all.
//!
//! Mechanism: environment, not config mutation. `DOCKER_HOST` overrides the
//! docker context; `KUBECONFIG` points kubectl/helm at a standalone file that
//! only contains the nebula cluster. Nothing to restore afterwards — the
//! user's own contexts never change. (`nebula setup <tool>` remains the way
//! to switch the plain commands over persistently.)

use std::os::unix::process::CommandExt;

use anyhow::{bail, Context};

use crate::client;

pub fn docker(args: Vec<String>) -> anyhow::Result<()> {
    ensure_engine()?;
    let sock = client::nebula_home()?.join("run/docker.sock");
    exec(
        "docker",
        &args,
        &[("DOCKER_HOST", format!("unix://{}", sock.display()))],
    )
}

pub fn kubectl(args: Vec<String>) -> anyhow::Result<()> {
    ensure_engine()?;
    let kubeconfig = crate::kube::ensure_ready()?;
    exec(
        "kubectl",
        &args,
        &[("KUBECONFIG", kubeconfig.display().to_string())],
    )
}

pub fn helm(args: Vec<String>) -> anyhow::Result<()> {
    ensure_engine()?;
    let kubeconfig = crate::kube::ensure_ready()?;
    exec(
        "helm",
        &args,
        &[("KUBECONFIG", kubeconfig.display().to_string())],
    )
}

fn ensure_engine() -> anyhow::Result<()> {
    if !client::daemon_running() {
        bail!("the Nebula engine is not running — start it with: nebula up");
    }
    Ok(())
}

/// Find `tool`: the user's own install (PATH) wins; otherwise fall back to
/// the copy bundled with Nebula.app (Contents/Resources/resources/bin next
/// to this sidecar) or ~/.nebula/bin.
fn resolve_tool(tool: &str) -> String {
    let on_path = std::process::Command::new("/usr/bin/which")
        .arg(tool)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if on_path {
        return tool.to_string();
    }
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Sidecar layout: Contents/MacOS/nebula -> Contents/Resources/resources/bin
            candidates.push(dir.join("../Resources/resources/bin").join(tool));
        }
    }
    if let Ok(home) = client::nebula_home() {
        candidates.push(home.join("bin").join(tool));
    }
    for c in candidates {
        if c.is_file() {
            return c.display().to_string();
        }
    }
    tool.to_string() // let exec fail with a clear error
}

/// Replace this process with the tool (signals, TTY, and exit code all
/// behave exactly as if the user ran it directly).
fn exec(tool: &str, args: &[String], env: &[(&str, String)]) -> anyhow::Result<()> {
    let resolved = resolve_tool(tool);
    let mut cmd = std::process::Command::new(&resolved);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let err = cmd.exec(); // only returns on failure
    Err(err).with_context(|| {
        format!(
            "could not run `{tool}` — not on your PATH and no bundled copy found (looked for {resolved})"
        )
    })
}
