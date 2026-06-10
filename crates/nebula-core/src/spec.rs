use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Declarative description of a VM. Backends consume this through `VmmBackend::create`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    pub name: String,
    pub cpus: u32,
    pub mem_mib: u64,
    pub boot: BootSpec,
    #[serde(default)]
    pub disks: Vec<DiskSpec>,
    #[serde(default)]
    pub shares: Vec<ShareSpec>,
    #[serde(default)]
    pub net: NetSpec,
    /// Attach a virtio-vsock device (host<->guest control channel).
    #[serde(default)]
    pub vsock: bool,
    #[serde(default)]
    pub console: ConsoleSpec,
    /// Attach a memory balloon device.
    #[serde(default)]
    pub balloon: bool,
    /// Attach an entropy (virtio-rng) device.
    #[serde(default = "default_true")]
    pub rng: bool,
    /// Attach Apple's Rosetta directory share (VZ only) so the guest can run
    /// amd64 binaries via binfmt_misc.
    #[serde(default)]
    pub rosetta: bool,
    /// Attach a virtio-gpu with Venus (Vulkan) support (libkrun only).
    #[serde(default)]
    pub gpu: bool,
    /// Host unix socket <-> guest vsock port maps (libkrun only): connecting
    /// to `host_path` reaches a guest listener on `port`.
    #[serde(default)]
    pub vsock_ports: Vec<VsockPortMap>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VsockPortMap {
    pub port: u32,
    pub host_path: PathBuf,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BootSpec {
    /// Direct kernel boot: uncompressed arm64 `Image` + optional initramfs.
    /// Both VZ (VZLinuxBootLoader) and libkrun (krun_set_kernel) support this,
    /// so the Vessel image is built once and boots on either backend.
    Kernel {
        kernel: PathBuf,
        initramfs: Option<PathBuf>,
        cmdline: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskSpec {
    pub path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
}

/// virtiofs directory share.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareSpec {
    /// Mount tag the guest uses (`mount -t virtiofs <tag> /mnt`).
    pub tag: String,
    pub host_path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum NetSpec {
    /// No network device.
    #[default]
    None,
    /// NAT through the host (VZ: VZNATNetworkDeviceAttachment; libkrun: TSI/passt).
    Nat,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ConsoleSpec {
    #[default]
    None,
    /// Append kernel console output to a host file.
    File(PathBuf),
}

impl VmSpec {
    /// Validate invariants that hold for every backend.
    pub fn validate(&self) -> Result<()> {
        if self.cpus == 0 {
            return Err(Error::InvalidSpec("cpus must be >= 1".into()));
        }
        if self.mem_mib < 64 {
            return Err(Error::InvalidSpec("mem_mib must be >= 64".into()));
        }
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(Error::InvalidSpec(format!(
                "name `{}` must be non-empty [a-zA-Z0-9_-]",
                self.name
            )));
        }
        let BootSpec::Kernel {
            kernel, initramfs, ..
        } = &self.boot;
        if !kernel.is_file() {
            return Err(Error::FileNotFound(kernel.clone()));
        }
        if let Some(initrd) = initramfs {
            if !initrd.is_file() {
                return Err(Error::FileNotFound(initrd.clone()));
            }
        }
        for d in &self.disks {
            if !d.path.is_file() {
                return Err(Error::FileNotFound(d.path.clone()));
            }
        }
        for s in &self.shares {
            if !s.host_path.is_dir() {
                return Err(Error::FileNotFound(s.host_path.clone()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with(name: &str, cpus: u32, mem: u64) -> VmSpec {
        // Use a file guaranteed to exist as a stand-in kernel for validation tests.
        VmSpec {
            name: name.into(),
            cpus,
            mem_mib: mem,
            boot: BootSpec::Kernel {
                kernel: PathBuf::from("/bin/sh"),
                initramfs: None,
                cmdline: "console=hvc0".into(),
            },
            disks: vec![],
            shares: vec![],
            net: NetSpec::None,
            vsock: false,
            console: ConsoleSpec::None,
            balloon: false,
            rng: true,
            rosetta: false,
            gpu: false,
            vsock_ports: vec![],
        }
    }

    #[test]
    fn valid_spec_passes() {
        assert!(spec_with("vessel", 2, 512).validate().is_ok());
    }

    #[test]
    fn rejects_zero_cpus_and_tiny_memory() {
        assert!(spec_with("a", 0, 512).validate().is_err());
        assert!(spec_with("a", 1, 32).validate().is_err());
    }

    #[test]
    fn rejects_bad_names_and_missing_kernel() {
        assert!(spec_with("has space", 1, 512).validate().is_err());
        assert!(spec_with("", 1, 512).validate().is_err());
        let mut s = spec_with("ok", 1, 512);
        s.boot = BootSpec::Kernel {
            kernel: PathBuf::from("/nonexistent/kernel"),
            initramfs: None,
            cmdline: String::new(),
        };
        assert!(s.validate().is_err());
    }
}
