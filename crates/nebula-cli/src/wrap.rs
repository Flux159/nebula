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

/// Replace this process with the tool (signals, TTY, and exit code all
/// behave exactly as if the user ran it directly).
fn exec(tool: &str, args: &[String], env: &[(&str, String)]) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(tool);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let err = cmd.exec(); // only returns on failure
    Err(err).with_context(|| format!("could not run `{tool}` — is it installed and on your PATH?"))
}
