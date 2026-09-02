//! Filesystem layout under ~/.nebula. Single source of truth for the daemon
//! and (via the control socket path) the CLI.

use std::path::PathBuf;

pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new() -> anyhow::Result<Self> {
        // Keep in sync with the CLI: NEBULA_HOME isolates embedded instances.
        if let Some(custom) = std::env::var_os("NEBULA_HOME") {
            return Ok(Self {
                root: PathBuf::from(custom),
            });
        }
        // USERPROFILE is the Windows spelling of HOME.
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| anyhow::anyhow!("HOME/USERPROFILE not set"))?;
        Ok(Self {
            root: PathBuf::from(home).join(".nebula"),
        })
    }

    pub fn config_toml(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }
    pub fn control_sock(&self) -> PathBuf {
        self.run_dir().join("nebulad.sock")
    }
    pub fn pid_file(&self) -> PathBuf {
        self.run_dir().join("nebulad.pid")
    }
    /// Why the last start failed, in plain text — read by `nebula up`, which
    /// spawns the daemon detached and so cannot see its stderr.
    pub fn startup_error(&self) -> PathBuf {
        self.run_dir().join("startup-error.txt")
    }
    pub fn docker_sock(&self) -> PathBuf {
        self.run_dir().join("docker.sock")
    }
    pub fn containerd_sock(&self) -> PathBuf {
        self.run_dir().join("containerd.sock")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn console_log(&self) -> PathBuf {
        self.logs_dir().join("vessel-console.log")
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.logs_dir().join("nebulad.log")
    }
    pub fn disks_dir(&self) -> PathBuf {
        self.root.join("disks")
    }
    pub fn rootfs_img(&self) -> PathBuf {
        self.disks_dir().join("rootfs.img")
    }
    pub fn data_img(&self) -> PathBuf {
        self.disks_dir().join("data.img")
    }
    pub fn kernel_dir(&self) -> PathBuf {
        self.root.join("kernel")
    }
    pub fn kernel_image(&self) -> PathBuf {
        self.kernel_dir().join("Image")
    }
    /// Engine vessel's persistent MAC (DHCP-lease stability across restarts).
    pub fn engine_mac_file(&self) -> PathBuf {
        self.root.join("engine.mac")
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for d in [
            self.run_dir(),
            self.logs_dir(),
            self.disks_dir(),
            self.kernel_dir(),
        ] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }
}
