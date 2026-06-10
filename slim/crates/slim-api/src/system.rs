//! /version, /info, /_ping.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VersionResponse {
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "ApiVersion")]
    pub api_version: String,
    #[serde(rename = "MinAPIVersion")]
    pub min_api_version: String,
    #[serde(rename = "GitCommit")]
    pub git_commit: String,
    #[serde(rename = "Os")]
    pub os: String,
    #[serde(rename = "Arch")]
    pub arch: String,
    #[serde(rename = "KernelVersion")]
    pub kernel_version: String,
    #[serde(rename = "BuildTime")]
    pub build_time: String,
    #[serde(rename = "Platform")]
    pub platform: PlatformName,
    #[serde(rename = "Components")]
    pub components: Vec<ComponentVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlatformName {
    #[serde(rename = "Name")]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComponentVersion {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Version")]
    pub version: String,
}

/// `GET /info` — the subset of fields the docker CLI and common tools read.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InfoResponse {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Containers")]
    pub containers: i64,
    #[serde(rename = "ContainersRunning")]
    pub containers_running: i64,
    #[serde(rename = "ContainersPaused")]
    pub containers_paused: i64,
    #[serde(rename = "ContainersStopped")]
    pub containers_stopped: i64,
    #[serde(rename = "Images")]
    pub images: i64,
    #[serde(rename = "Driver")]
    pub driver: String, // "overlay2"
    #[serde(rename = "DriverStatus")]
    pub driver_status: Vec<Vec<String>>,
    #[serde(rename = "MemoryLimit")]
    pub memory_limit: bool,
    #[serde(rename = "SwapLimit")]
    pub swap_limit: bool,
    #[serde(rename = "CpuCfsPeriod")]
    pub cpu_cfs_period: bool,
    #[serde(rename = "CpuCfsQuota")]
    pub cpu_cfs_quota: bool,
    #[serde(rename = "IPv4Forwarding")]
    pub ipv4_forwarding: bool,
    #[serde(rename = "OomKillDisable")]
    pub oom_kill_disable: bool,
    #[serde(rename = "NCPU")]
    pub ncpu: i64,
    #[serde(rename = "MemTotal")]
    pub mem_total: i64,
    #[serde(rename = "DockerRootDir")]
    pub docker_root_dir: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "KernelVersion")]
    pub kernel_version: String,
    #[serde(rename = "OperatingSystem")]
    pub operating_system: String,
    #[serde(rename = "OSType")]
    pub os_type: String, // "linux"
    #[serde(rename = "OSVersion")]
    pub os_version: String,
    #[serde(rename = "Architecture")]
    pub architecture: String, // "aarch64"
    #[serde(rename = "ServerVersion")]
    pub server_version: String,
    #[serde(rename = "DefaultRuntime")]
    pub default_runtime: String,
    #[serde(rename = "LiveRestoreEnabled")]
    pub live_restore_enabled: bool,
    #[serde(rename = "Warnings")]
    pub warnings: Vec<String>,
}

/// `GET /auth` body (docker login).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AuthConfig {
    pub username: String,
    pub password: String,
    pub email: String,
    pub serveraddress: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub identitytoken: String,
}
