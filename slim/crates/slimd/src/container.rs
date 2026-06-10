//! Persisted container model + the per-container runtime state slimd keeps
//! in memory (io handles, waiter, live attach subscribers).

use serde::{Deserialize, Serialize};
use slim_api::container::*;
use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The on-disk record (state/<id>/config.json). Everything needed to restart
/// a container after slimd or the vessel reboots.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image_ref: String, // as requested ("alpine:3.19")
    pub image_id: String,  // sha256:...
    pub created: String,
    pub config: ContainerConfig,
    pub host_config: HostConfig,
    /// Resolved at create time from image + config (entrypoint+cmd).
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: String,
    pub user: String,
    pub hostname: String,
    /// chosen primary network (name) + assigned ip.
    pub network: String,
    pub ip: String,
    pub mac: String,
    /// Extra DNS names for this container on its network (from
    /// NetworkingConfig endpoint aliases / --network-alias / the kube facade).
    #[serde(default)]
    pub aliases: Vec<String>,
    pub state: State,
    pub log_path: String,
    /// overlay base dir (state/<id>/rootfs).
    pub rootfs_base: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    pub status: String, // created|running|exited|dead|removing
    pub pid: i32,
    pub exit_code: i32,
    pub error: String,
    pub oom_killed: bool,
    pub started_at: String,
    pub finished_at: String,
    pub restart_count: i64,
}

impl Container {
    pub fn running(&self) -> bool {
        self.state.status == "running"
    }

    pub fn slash_name(&self) -> String {
        format!("/{}", self.name)
    }

    pub fn short_id(&self) -> String {
        self.id.chars().take(12).collect()
    }
}

/// Live runtime handles (NOT persisted).
pub struct Runtime {
    pub tty: bool,
    /// tty: pty master (read+write clone source). pipe: None.
    pub pty: Option<Mutex<File>>,
    /// pipe mode stdin writer.
    pub stdin: Option<Mutex<File>>,
    /// pipe mode stdout/stderr read ends (consumed by the pump thread).
    pub stdout: Option<Mutex<File>>,
    pub stderr: Option<Mutex<File>>,
    /// Live output fan-out: chunk = (stream, bytes). Used by attach.
    pub subscribers: Mutex<Vec<std::sync::mpsc::Sender<(u8, Vec<u8>)>>>,
    /// set when the process exits (notifies waiters).
    pub exited: Arc<(Mutex<Option<i32>>, std::sync::Condvar)>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            tty: false,
            pty: None,
            stdin: None,
            stdout: None,
            stderr: None,
            subscribers: Mutex::new(Vec::new()),
            exited: Arc::new((Mutex::new(None), std::sync::Condvar::new())),
        }
    }
}

#[allow(dead_code)]
pub const STREAM_STDIN: u8 = 0;
pub const STREAM_STDOUT: u8 = 1;
pub const STREAM_STDERR: u8 = 2;

/// In-memory entry: the record plus its live runtime.
pub struct Entry {
    pub c: Mutex<Container>,
    pub rt: Mutex<Arc<Runtime>>,
    #[allow(dead_code)]
    pub base_dir: PathBuf,
}

impl Entry {
    pub fn snapshot(&self) -> Container {
        self.c.lock().unwrap().clone()
    }
}

/// An exec session.
pub struct ExecSession {
    pub id: String,
    pub container_id: String,
    pub config: slim_api::exec::ExecConfig,
    pub pid: Mutex<i32>,
    pub exit_code: Mutex<Option<i32>>,
    pub running: Mutex<bool>,
}

/// Resolve the runnable argv from image config + container overrides, the way
/// docker composes Entrypoint + Cmd.
pub fn resolve_argv(
    image_cfg: &slim_api::image::ImageConfig,
    req: &ContainerConfig,
) -> Vec<String> {
    // Entrypoint: request overrides image (empty array clears it).
    let entrypoint = match &req.entrypoint {
        Some(e) => e.clone(),
        None => image_cfg.entrypoint.clone().unwrap_or_default(),
    };
    // Cmd: request overrides image. If request set entrypoint but not cmd,
    // the image cmd is dropped (docker rule).
    let cmd = if !req.cmd.is_empty() {
        req.cmd.clone()
    } else if req.entrypoint.is_some() {
        Vec::new()
    } else {
        image_cfg.cmd.clone().unwrap_or_default()
    };
    let mut argv = entrypoint;
    argv.extend(cmd);
    argv
}

/// Merge image env with request env (request wins per key).
pub fn resolve_env(image_cfg: &slim_api::image::ImageConfig, req: &ContainerConfig) -> Vec<String> {
    let mut map: Vec<(String, String)> = Vec::new();
    let mut push = |e: &str| {
        if let Some((k, v)) = e.split_once('=') {
            if let Some(slot) = map.iter_mut().find(|(mk, _)| mk == k) {
                slot.1 = v.to_string();
            } else {
                map.push((k.to_string(), v.to_string()));
            }
        }
    };
    for e in &image_cfg.env {
        push(e);
    }
    for e in &req.env {
        push(e);
    }
    map.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// docker-style "Up 3 minutes" / "Exited (0) 5 seconds ago".
pub fn status_string(state: &State) -> String {
    match state.status.as_str() {
        "running" => format!("Up {}", since(&state.started_at)),
        "created" => "Created".to_string(),
        "exited" => format!("Exited ({}) {} ago", state.exit_code, since(&state.finished_at)),
        other => other.to_string(),
    }
}

fn since(rfc3339: &str) -> String {
    let Some(then) = slim_runtime::jsonlog::parse_rfc3339(rfc3339) else {
        return "moments".into();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(then);
    let secs = (now - then).max(0);
    if secs < 60 {
        format!("{secs} seconds")
    } else if secs < 3600 {
        format!("{} minutes", secs / 60)
    } else if secs < 86400 {
        format!("{} hours", secs / 3600)
    } else {
        format!("{} days", secs / 86400)
    }
}

/// Build the container's /etc/hosts content.
pub fn hosts_file(hostname: &str, ip: &str, extra: &[(String, Vec<String>)], extra_hosts: &[String]) -> String {
    let mut s = String::from("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
    if !ip.is_empty() && !hostname.is_empty() {
        s.push_str(&format!("{ip}\t{hostname}\n"));
    }
    for (eip, names) in extra {
        s.push_str(&format!("{eip}\t{}\n", names.join(" ")));
    }
    for h in extra_hosts {
        if let Some((host, addr)) = h.split_once(':') {
            s.push_str(&format!("{addr}\t{host}\n"));
        }
    }
    s
}

pub fn resolv_conf(nameserver: &str, dns_override: &[String]) -> String {
    if !dns_override.is_empty() {
        return dns_override.iter().map(|d| format!("nameserver {d}\n")).collect();
    }
    format!("nameserver {nameserver}\noptions ndots:0\n")
}

/// Parse a port spec map from HostConfig.PortBindings + image ExposedPorts.
pub fn collect_ports(
    bindings: &BTreeMap<String, Vec<PortBinding>>,
) -> Vec<(String, u16, u16)> {
    // returns (proto, host_port, container_port)
    let mut out = Vec::new();
    for (key, binds) in bindings {
        let (port, proto) = match key.split_once('/') {
            Some((p, pr)) => (p, pr),
            None => (key.as_str(), "tcp"),
        };
        let Ok(cport) = port.parse::<u16>() else { continue };
        for b in binds {
            let hport = b.host_port.parse::<u16>().unwrap_or(cport);
            out.push((proto.to_string(), hport, cport));
        }
        if binds.is_empty() {
            out.push((proto.to_string(), cport, cport));
        }
    }
    out
}
