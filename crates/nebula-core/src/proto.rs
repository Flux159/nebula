//! Host <-> guest agent protocol (v0): JSON-lines over vsock.
//!
//! Port map (guest listeners):
//! - 1024: control (one JSON request line -> one JSON response line per connection)
//! - 1025: shell (on connect: one JSON ShellOpen line, then a raw pty byte stream)
//!
//! v0 is deliberately simple; the public, versioned gRPC surface arrives with
//! the embedding API (Phase 10). Bump `PROTO_VERSION` on any wire change.

use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u32 = 1;
pub const VSOCK_PORT_CONTROL: u32 = 1024;
pub const VSOCK_PORT_SHELL: u32 = 1025;
/// Stream proxy to the guest's /var/run/docker.sock.
pub const VSOCK_PORT_DOCKER: u32 = 2375;
/// Stream proxy to the guest's /run/containerd/containerd.sock.
pub const VSOCK_PORT_CONTAINERD: u32 = 2376;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentRequest {
    Health,
    MemStats,
    Exec {
        cmd: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default = "default_exec_timeout_ms")]
        timeout_ms: u64,
    },
    /// Power off the guest cleanly.
    Shutdown,
}

fn default_exec_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum AgentResponse {
    Health(Health),
    MemStats(MemStats),
    Exec(ExecResult),
    Ok,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub proto_version: u32,
    pub agent_version: String,
    pub kernel: String,
    pub uptime_secs: u64,
}

/// Guest memory signals consumed by the balloon controller (Phase 4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemStats {
    pub total_kib: u64,
    pub free_kib: u64,
    pub available_kib: u64,
    pub cached_kib: u64,
    /// PSI memory pressure (avg10, "some"), 0.0-100.0. None if PSI unavailable.
    pub psi_some_avg10: Option<f64>,
    /// PSI memory pressure (avg10, "full").
    pub psi_full_avg10: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    /// First 64 KiB of stdout (UTF-8 lossy).
    pub stdout: String,
    /// First 64 KiB of stderr (UTF-8 lossy).
    pub stderr: String,
    pub timed_out: bool,
}

/// CLI <-> nebulad protocol (JSON-lines over the daemon's unix socket).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequest {
    Status,
    /// Stop the Vessel (graceful via agent unless force) and exit the daemon.
    Down {
        force: bool,
    },
    /// Proxy a control request to the guest agent.
    Agent {
        request: AgentRequest,
    },
    /// Upgrade this connection to a raw shell byte stream (bridged to vsock).
    Shell {
        open: ShellOpen,
    },
    /// Set the balloon target (guest-visible memory, MiB).
    Balloon {
        target_mib: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum DaemonResponse {
    Status(DaemonStatus),
    Agent {
        response: AgentResponse,
    },
    /// Shell accepted: the stream is now raw bytes both ways.
    ShellStarted,
    Ok,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub daemon_version: String,
    pub daemon_pid: u32,
    pub vm_state: String,
    pub backend: String,
    pub cpus: u32,
    pub mem_mib: u64,
    pub agent: Option<Health>,
    pub mem: Option<MemStats>,
    pub uptime_secs: u64,
}

/// First line sent by the client on the shell stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOpen {
    #[serde(default = "default_shell")]
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cols: u16,
    pub rows: u16,
}

fn default_shell() -> String {
    "/bin/sh".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_requests() {
        let req = AgentRequest::Exec {
            cmd: "uname".into(),
            args: vec!["-a".into()],
            env: vec![],
            timeout_ms: 1000,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: AgentRequest = serde_json::from_str(&s).unwrap();
        match back {
            AgentRequest::Exec { cmd, args, .. } => {
                assert_eq!(cmd, "uname");
                assert_eq!(args, vec!["-a"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn error_response_shape_is_stable() {
        let resp = AgentResponse::Error {
            message: "nope".into(),
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"result":"error","message":"nope"}"#
        );
    }
}
