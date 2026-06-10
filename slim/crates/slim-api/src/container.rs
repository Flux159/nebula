//! Container config / inspect / list types (Engine API v1.43 subset).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Body of `POST /containers/create` — docker flattens Config + HostConfig +
/// NetworkingConfig into one object.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContainerCreateRequest {
    #[serde(flatten)]
    pub config: ContainerConfig,
    #[serde(rename = "HostConfig", default)]
    pub host_config: HostConfig,
    #[serde(rename = "NetworkingConfig", default)]
    pub networking_config: NetworkingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContainerCreateResponse {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Warnings", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContainerConfig {
    #[serde(rename = "Hostname", skip_serializing_if = "String::is_empty")]
    pub hostname: String,
    #[serde(rename = "Domainname", skip_serializing_if = "String::is_empty")]
    pub domainname: String,
    #[serde(rename = "User", skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(rename = "AttachStdin")]
    pub attach_stdin: bool,
    #[serde(rename = "AttachStdout")]
    pub attach_stdout: bool,
    #[serde(rename = "AttachStderr")]
    pub attach_stderr: bool,
    #[serde(rename = "ExposedPorts", skip_serializing_if = "Option::is_none")]
    pub exposed_ports: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(rename = "Tty")]
    pub tty: bool,
    #[serde(rename = "OpenStdin")]
    pub open_stdin: bool,
    #[serde(rename = "StdinOnce")]
    pub stdin_once: bool,
    #[serde(rename = "Env", deserialize_with = "null_to_default")]
    pub env: Vec<String>,
    #[serde(rename = "Cmd", deserialize_with = "null_to_default")]
    pub cmd: Vec<String>,
    #[serde(rename = "Entrypoint", skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "Volumes", skip_serializing_if = "Option::is_none")]
    pub volumes: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(rename = "WorkingDir", skip_serializing_if = "String::is_empty")]
    pub working_dir: String,
    #[serde(rename = "NetworkDisabled", skip_serializing_if = "is_false")]
    pub network_disabled: bool,
    #[serde(rename = "Labels", deserialize_with = "null_to_default")]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "StopSignal", skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
    #[serde(rename = "StopTimeout", skip_serializing_if = "Option::is_none")]
    pub stop_timeout: Option<i64>,
    #[serde(rename = "Healthcheck", skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthConfig>,
}

/// Parsed and stored; enforcement is best-effort (S3+).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HealthConfig {
    #[serde(rename = "Test")]
    pub test: Vec<String>,
    #[serde(rename = "Interval")]
    pub interval: i64,
    #[serde(rename = "Timeout")]
    pub timeout: i64,
    #[serde(rename = "Retries")]
    pub retries: i64,
    #[serde(rename = "StartPeriod")]
    pub start_period: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HostConfig {
    #[serde(rename = "Binds", deserialize_with = "null_to_default")]
    pub binds: Vec<String>,
    #[serde(rename = "NetworkMode", skip_serializing_if = "String::is_empty")]
    pub network_mode: String,
    #[serde(rename = "PortBindings", deserialize_with = "null_to_default")]
    pub port_bindings: BTreeMap<String, Vec<PortBinding>>,
    #[serde(rename = "RestartPolicy")]
    pub restart_policy: RestartPolicy,
    #[serde(rename = "AutoRemove")]
    pub auto_remove: bool,
    #[serde(rename = "VolumesFrom", deserialize_with = "null_to_default")]
    pub volumes_from: Vec<String>,
    #[serde(rename = "Mounts", deserialize_with = "null_to_default")]
    pub mounts: Vec<MountSpec>,
    #[serde(rename = "CapAdd", deserialize_with = "null_to_default")]
    pub cap_add: Vec<String>,
    #[serde(rename = "CapDrop", deserialize_with = "null_to_default")]
    pub cap_drop: Vec<String>,
    #[serde(rename = "Dns", deserialize_with = "null_to_default")]
    pub dns: Vec<String>,
    #[serde(rename = "ExtraHosts", deserialize_with = "null_to_default")]
    pub extra_hosts: Vec<String>,
    #[serde(rename = "GroupAdd", deserialize_with = "null_to_default")]
    pub group_add: Vec<String>,
    #[serde(rename = "Privileged")]
    pub privileged: bool,
    #[serde(rename = "PublishAllPorts")]
    pub publish_all_ports: bool,
    #[serde(rename = "ReadonlyRootfs")]
    pub readonly_rootfs: bool,
    #[serde(rename = "SecurityOpt", deserialize_with = "null_to_default")]
    pub security_opt: Vec<String>,
    #[serde(rename = "Tmpfs", deserialize_with = "null_to_default")]
    pub tmpfs: BTreeMap<String, String>,
    #[serde(rename = "ShmSize")]
    pub shm_size: i64,
    #[serde(rename = "Memory")]
    pub memory: i64,
    #[serde(rename = "MemorySwap")]
    pub memory_swap: i64,
    #[serde(rename = "NanoCpus")]
    pub nano_cpus: i64,
    #[serde(rename = "CpuShares")]
    pub cpu_shares: i64,
    #[serde(rename = "CpusetCpus", skip_serializing_if = "String::is_empty")]
    pub cpuset_cpus: String,
    #[serde(rename = "PidsLimit", skip_serializing_if = "Option::is_none")]
    pub pids_limit: Option<i64>,
    #[serde(rename = "Ulimits", deserialize_with = "null_to_default")]
    pub ulimits: Vec<Ulimit>,
    #[serde(rename = "OomKillDisable", skip_serializing_if = "Option::is_none")]
    pub oom_kill_disable: Option<bool>,
    #[serde(rename = "Init", skip_serializing_if = "Option::is_none")]
    pub init: Option<bool>,
    #[serde(rename = "Ipc", skip_serializing_if = "String::is_empty")]
    pub ipc_mode: String,
    #[serde(rename = "Pid", skip_serializing_if = "String::is_empty")]
    pub pid_mode: String,
    #[serde(rename = "LogConfig")]
    pub log_config: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LogConfig {
    #[serde(rename = "Type")]
    pub typ: String,
    #[serde(rename = "Config", deserialize_with = "null_to_default")]
    pub config: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Ulimit {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Soft")]
    pub soft: i64,
    #[serde(rename = "Hard")]
    pub hard: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RestartPolicy {
    #[serde(rename = "Name")]
    pub name: String, // "", no, on-failure, always, unless-stopped
    #[serde(rename = "MaximumRetryCount")]
    pub maximum_retry_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PortBinding {
    #[serde(rename = "HostIp")]
    pub host_ip: String,
    #[serde(rename = "HostPort")]
    pub host_port: String,
}

/// `--mount` style spec (subset: bind, volume, tmpfs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MountSpec {
    #[serde(rename = "Type")]
    pub typ: String,
    #[serde(rename = "Source")]
    pub source: String,
    #[serde(rename = "Target")]
    pub target: String,
    #[serde(rename = "ReadOnly")]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NetworkingConfig {
    #[serde(rename = "EndpointsConfig", deserialize_with = "null_to_default")]
    pub endpoints_config: BTreeMap<String, EndpointSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EndpointSettings {
    #[serde(rename = "Aliases", deserialize_with = "null_to_default")]
    pub aliases: Vec<String>,
    #[serde(rename = "NetworkID")]
    pub network_id: String,
    #[serde(rename = "EndpointID")]
    pub endpoint_id: String,
    #[serde(rename = "Gateway")]
    pub gateway: String,
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
    #[serde(rename = "IPPrefixLen")]
    pub ip_prefix_len: i64,
    #[serde(rename = "MacAddress")]
    pub mac_address: String,
}

/// One element of `GET /containers/json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContainerSummary {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Names")]
    pub names: Vec<String>,
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "ImageID")]
    pub image_id: String,
    #[serde(rename = "Command")]
    pub command: String,
    #[serde(rename = "Created")]
    pub created: i64,
    #[serde(rename = "Ports")]
    pub ports: Vec<PortSummary>,
    #[serde(rename = "Labels")]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "State")]
    pub state: String, // created|running|exited|...
    #[serde(rename = "Status")]
    pub status: String, // human: "Up 2 minutes", "Exited (0) 3 seconds ago"
    #[serde(rename = "NetworkSettings")]
    pub network_settings: SummaryNetworkSettings,
    #[serde(rename = "Mounts")]
    pub mounts: Vec<MountPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SummaryNetworkSettings {
    #[serde(rename = "Networks")]
    pub networks: BTreeMap<String, EndpointSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PortSummary {
    #[serde(rename = "IP", skip_serializing_if = "String::is_empty")]
    pub ip: String,
    #[serde(rename = "PrivatePort")]
    pub private_port: u16,
    #[serde(rename = "PublicPort", skip_serializing_if = "is_zero_u16")]
    pub public_port: u16,
    #[serde(rename = "Type")]
    pub typ: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MountPoint {
    #[serde(rename = "Type")]
    pub typ: String,
    #[serde(rename = "Name", skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(rename = "Source")]
    pub source: String,
    #[serde(rename = "Destination")]
    pub destination: String,
    #[serde(rename = "Mode")]
    pub mode: String,
    #[serde(rename = "RW")]
    pub rw: bool,
}

/// `GET /containers/{id}/json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContainerInspect {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Created")]
    pub created: String, // RFC3339Nano
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Args")]
    pub args: Vec<String>,
    #[serde(rename = "State")]
    pub state: ContainerState,
    #[serde(rename = "Image")]
    pub image: String, // image id (sha256:...)
    #[serde(rename = "Name")]
    pub name: String, // "/foo"
    #[serde(rename = "RestartCount")]
    pub restart_count: i64,
    #[serde(rename = "Driver")]
    pub driver: String, // "overlay2"
    #[serde(rename = "Platform")]
    pub platform: String, // "linux"
    #[serde(rename = "HostConfig")]
    pub host_config: HostConfig,
    #[serde(rename = "Config")]
    pub config: ContainerConfig,
    #[serde(rename = "NetworkSettings")]
    pub network_settings: NetworkSettings,
    #[serde(rename = "Mounts")]
    pub mounts: Vec<MountPoint>,
    #[serde(rename = "LogPath")]
    pub log_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContainerState {
    #[serde(rename = "Status")]
    pub status: String, // created|running|paused|restarting|removing|exited|dead
    #[serde(rename = "Running")]
    pub running: bool,
    #[serde(rename = "Paused")]
    pub paused: bool,
    #[serde(rename = "Restarting")]
    pub restarting: bool,
    #[serde(rename = "OOMKilled")]
    pub oom_killed: bool,
    #[serde(rename = "Dead")]
    pub dead: bool,
    #[serde(rename = "Pid")]
    pub pid: i64,
    #[serde(rename = "ExitCode")]
    pub exit_code: i64,
    #[serde(rename = "Error")]
    pub error: String,
    #[serde(rename = "StartedAt")]
    pub started_at: String,
    #[serde(rename = "FinishedAt")]
    pub finished_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NetworkSettings {
    #[serde(rename = "Bridge")]
    pub bridge: String,
    #[serde(rename = "SandboxKey")]
    pub sandbox_key: String,
    #[serde(rename = "Ports")]
    pub ports: BTreeMap<String, Option<Vec<PortBinding>>>,
    #[serde(rename = "Gateway")]
    pub gateway: String,
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
    #[serde(rename = "IPPrefixLen")]
    pub ip_prefix_len: i64,
    #[serde(rename = "MacAddress")]
    pub mac_address: String,
    #[serde(rename = "Networks")]
    pub networks: BTreeMap<String, EndpointSettings>,
}

/// `GET /containers/{id}/wait`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WaitResponse {
    #[serde(rename = "StatusCode")]
    pub status_code: i64,
    #[serde(rename = "Error", skip_serializing_if = "Option::is_none")]
    pub error: Option<WaitError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WaitError {
    #[serde(rename = "Message")]
    pub message: String,
}

/// `GET /containers/{id}/stats` (lite — the fields `docker stats` renders).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StatsResponse {
    pub read: String,
    pub preread: String,
    pub pids_stats: PidsStats,
    pub cpu_stats: CpuStats,
    pub precpu_stats: CpuStats,
    pub memory_stats: MemoryStats,
    pub name: String,
    pub id: String,
    pub networks: BTreeMap<String, NetworkStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PidsStats {
    pub current: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CpuStats {
    pub cpu_usage: CpuUsage,
    pub system_cpu_usage: u64,
    pub online_cpus: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CpuUsage {
    pub total_usage: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MemoryStats {
    pub usage: u64,
    pub limit: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NetworkStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

/// Docker clients send `"Env": null` etc.; treat JSON null as the default.
pub fn null_to_default<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    let opt = Option::<T>::deserialize(de)?;
    Ok(opt.unwrap_or_default())
}
