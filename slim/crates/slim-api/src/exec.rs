//! Exec endpoints.

use serde::{Deserialize, Serialize};

/// `POST /containers/{id}/exec`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExecConfig {
    #[serde(rename = "AttachStdin")]
    pub attach_stdin: bool,
    #[serde(rename = "AttachStdout")]
    pub attach_stdout: bool,
    #[serde(rename = "AttachStderr")]
    pub attach_stderr: bool,
    #[serde(rename = "Tty")]
    pub tty: bool,
    #[serde(rename = "Env", deserialize_with = "crate::container::null_to_default")]
    pub env: Vec<String>,
    #[serde(rename = "Cmd", deserialize_with = "crate::container::null_to_default")]
    pub cmd: Vec<String>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: String,
    #[serde(rename = "User")]
    pub user: String,
    #[serde(rename = "Privileged")]
    pub privileged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExecCreateResponse {
    #[serde(rename = "Id")]
    pub id: String,
}

/// `POST /exec/{id}/start`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExecStartConfig {
    #[serde(rename = "Detach")]
    pub detach: bool,
    #[serde(rename = "Tty")]
    pub tty: bool,
}

/// `GET /exec/{id}/json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExecInspect {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Running")]
    pub running: bool,
    #[serde(rename = "ExitCode", skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(rename = "ContainerID")]
    pub container_id: String,
    #[serde(rename = "Pid")]
    pub pid: i64,
}
