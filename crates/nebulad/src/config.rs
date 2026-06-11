//! ~/.nebula/config.toml — everything optional, defaults tuned for "just works".

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Maximum guest RAM in MiB. Default: min(host/2, 32 GiB). Ballooning makes
    /// generosity cheap (Phase 4); this is a ceiling, not a reservation.
    pub max_ram_mib: Option<u64>,
    /// vCPUs. Default: all host cores (scheduler-shared).
    pub cpus: Option<u32>,
    /// Data disk size in GiB (sparse; grows on demand). Default 64.
    pub data_disk_gib: Option<u64>,
    /// Override the kernel Image path (default: ~/.nebula/kernel/Image).
    pub kernel: Option<PathBuf>,
    /// Override the rootfs image path (default: ~/.nebula/disks/rootfs.img).
    pub rootfs: Option<PathBuf>,
    /// REST API port (default 7440; 0 disables the API).
    /// Embedders running alongside a standalone Nebula should set this.
    pub api_port: Option<u16>,
    /// REST API bind address (default 127.0.0.1). NEBULA_API_HOST overrides.
    /// Non-loopback binds require NEBULA_API_TOKEN (bearer auth).
    pub api_host: Option<String>,
    /// Local DNS zone for container names (default "nebula.local").
    /// Embedders can brand it: dns_zone = "galaxy.local".
    pub dns_zone: Option<String>,
    /// Host UDP port for the guest DNS relay (default 42053). Must be unique
    /// per instance when running multiple Nebulas.
    pub dns_port: Option<u16>,
    /// Host port forwarding to the k3s API (default 6443). Unique per
    /// k8s-enabled instance.
    pub k8s_port: Option<u16>,
}

pub struct Effective {
    pub mem_mib: u64,
    pub cpus: u32,
    pub data_disk_gib: u64,
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn effective(&self) -> Effective {
        let host_mem_mib = host_mem_bytes() / (1024 * 1024);
        Effective {
            mem_mib: self
                .max_ram_mib
                .unwrap_or_else(|| (host_mem_mib / 2).min(32 * 1024)),
            cpus: self.cpus.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get() as u32)
                    .unwrap_or(4)
            }),
            data_disk_gib: self.data_disk_gib.unwrap_or(64),
        }
    }
}

#[cfg(target_os = "macos")]
fn host_mem_bytes() -> u64 {
    let mut size: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let name = std::ffi::CString::new("hw.memsize").unwrap();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut size as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        size
    } else {
        8 * 1024 * 1024 * 1024
    }
}

#[cfg(windows)]
fn host_mem_bytes() -> u64 {
    // TODO(whp): GlobalMemoryStatusEx via windows-sys once the backend lands.
    8 * 1024 * 1024 * 1024
}

#[cfg(target_os = "linux")]
fn host_mem_bytes() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines().find_map(|l| {
                l.strip_prefix("MemTotal:")
                    .and_then(|v| v.trim().trim_end_matches(" kB").parse::<u64>().ok())
            })
        })
        .map(|kib| kib * 1024)
        .unwrap_or(8 * 1024 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let eff = Config::default().effective();
        assert!(eff.mem_mib >= 1024);
        assert!(eff.mem_mib <= 32 * 1024);
        assert!(eff.cpus >= 1);
        assert_eq!(eff.data_disk_gib, 64);
    }

    #[test]
    fn parses_partial_config() {
        let c: Config = toml::from_str("max_ram_mib = 96000\ncpus = 8\n").unwrap();
        let eff = c.effective();
        assert_eq!(eff.mem_mib, 96000);
        assert_eq!(eff.cpus, 8);
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<Config>("definitely_not_a_field = 1\n").is_err());
    }
}
