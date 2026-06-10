//! The Vessel: the single managed VZ VM plus the agent control channel.

use std::io::{BufRead, BufReader, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use nebula_core::backend::{backend_by_name, VmHandle, VmState};
use nebula_core::proto::*;
use nebula_core::{BootSpec, ConsoleSpec, DiskSpec, NetSpec, VmSpec};

use crate::config::Config;
use crate::paths::Paths;

pub struct Vessel {
    vm: Mutex<Box<dyn VmHandle>>,
    pub spec: VmSpec,
    pub started_at: Instant,
}

impl Vessel {
    pub fn boot(paths: &Paths, cfg: &Config) -> anyhow::Result<Self> {
        let eff = cfg.effective();

        let kernel = cfg.kernel.clone().unwrap_or_else(|| paths.kernel_image());
        anyhow::ensure!(
            kernel.is_file(),
            "guest kernel not found at {} — run `nebula install-image` (dev: vessel/build-kernel.sh then nebula up --install)",
            kernel.display()
        );
        let rootfs = cfg.rootfs.clone().unwrap_or_else(|| paths.rootfs_img());
        anyhow::ensure!(
            rootfs.is_file(),
            "rootfs image not found at {} — run `nebula install-image` (dev: vessel/build-rootfs.sh then nebula up --install)",
            rootfs.display()
        );

        // Sparse data disk on first boot; guest formats it (nebula-init).
        if !paths.data_img().is_file() {
            let f = std::fs::File::create(paths.data_img())?;
            f.set_len(eff.data_disk_gib * 1024 * 1024 * 1024)?;
            tracing::info!(gib = eff.data_disk_gib, "created sparse data disk");
        }

        // macOS: VZ (Rosetta + first-party balloon + vsock device).
        // Linux: krun/KVM — guest vsock ports are mapped to host unix sockets
        // (vsock_connect in the krun handle goes through them), there is no
        // balloon/Rosetta, and TSI handles networking without a NIC.
        let macos = cfg!(target_os = "macos");
        let vsock_ports = if macos {
            vec![]
        } else {
            let m = |port, name: &str| nebula_core::VsockPortMap {
                port,
                host_path: paths.run_dir().join(name),
            };
            vec![
                m(VSOCK_PORT_CONTROL, "agent.vsock"),
                m(VSOCK_PORT_TCPPROXY, "tcpproxy.vsock"),
                m(VSOCK_PORT_SHELL, "shell.vsock"),
                m(VSOCK_PORT_DOCKER, "docker.vsock"),
                m(VSOCK_PORT_CONTAINERD, "containerd.vsock"),
            ]
        };
        let spec = VmSpec {
            name: "vessel".into(),
            cpus: eff.cpus,
            mem_mib: eff.mem_mib,
            boot: BootSpec::Kernel {
                kernel,
                initramfs: None,
                cmdline: vessel_cmdline(cfg),
            },
            disks: vec![
                DiskSpec {
                    path: rootfs,
                    read_only: false,
                },
                DiskSpec {
                    path: paths.data_img(),
                    read_only: false,
                },
            ],
            shares: home_share(),
            // Nat everywhere: VZ NAT on macOS; passt-backed virtio-net on
            // Linux (the engine needs outbound for image pulls — TSI's
            // guest-side hijack doesn't apply to our own-init disk boots).
            net: NetSpec::Nat,
            vsock: macos,
            console: ConsoleSpec::File(paths.console_log()),
            balloon: macos,
            rng: true,
            rosetta: macos,
            gpu: false,
            vsock_ports,
            backend: None,
            mac: None,
            machine_id: None,
        };

        let backend = backend_by_name(if macos { "vz" } else { "krun" })?;

        let mut vm = backend.create(&spec)?;
        let t0 = Instant::now();
        vm.start()?;
        tracing::info!(elapsed = ?t0.elapsed(), cpus = eff.cpus, mem_mib = eff.mem_mib, "vessel started");

        let vessel = Self {
            vm: Mutex::new(vm),
            spec,
            started_at: Instant::now(),
        };

        // Wait for the agent to come up.
        let health = vessel.wait_agent(Duration::from_secs(20))?;
        tracing::info!(agent = health.agent_version, kernel = health.kernel, boot = ?t0.elapsed(), "agent healthy");
        Ok(vessel)
    }

    fn wait_agent(&self, timeout: Duration) -> anyhow::Result<Health> {
        let start = Instant::now();
        loop {
            match self.agent_request(&AgentRequest::Health) {
                Ok(AgentResponse::Health(h)) => return Ok(h),
                _ if start.elapsed() > timeout => {
                    anyhow::bail!(
                        "agent did not become healthy within {timeout:?} (console: see logs/vessel-console.log)"
                    )
                }
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    pub fn state(&self) -> VmState {
        self.vm.lock().unwrap().state()
    }

    pub fn vsock_connect(&self, port: u32) -> anyhow::Result<std::os::unix::net::UnixStream> {
        Ok(self.vm.lock().unwrap().vsock_connect(port)?)
    }

    pub fn agent_request(&self, req: &AgentRequest) -> anyhow::Result<AgentResponse> {
        let stream = self.vm.lock().unwrap().vsock_connect(VSOCK_PORT_CONTROL)?;
        let mut writer = stream.try_clone()?;
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        writer.write_all(line.as_bytes())?;
        let mut reader = BufReader::new(stream);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line)?;
        Ok(serde_json::from_str(resp_line.trim())?)
    }

    /// Open the raw shell stream and send the ShellOpen header.
    pub fn open_shell(&self, open: &ShellOpen) -> anyhow::Result<std::os::unix::net::UnixStream> {
        let stream = self.vm.lock().unwrap().vsock_connect(VSOCK_PORT_SHELL)?;
        let mut writer = stream.try_clone()?;
        let mut line = serde_json::to_string(open)?;
        line.push('\n');
        writer.write_all(line.as_bytes())?;
        Ok(stream)
    }

    pub fn balloon_set(&self, target_mib: u64) -> anyhow::Result<()> {
        self.vm.lock().unwrap().balloon_set_guest_mib(target_mib)?;
        Ok(())
    }

    /// Stop the Vessel: graceful via agent shutdown, falling back to VZ stop.
    pub fn stop(&self, force: bool) -> anyhow::Result<()> {
        if !force {
            let _ = self.agent_request(&AgentRequest::Shutdown);
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if self.state() == VmState::Stopped {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            tracing::warn!("graceful shutdown timed out; forcing stop");
        }
        let mut vm = self.vm.lock().unwrap();
        vm.stop(true)?;
        vm.wait_for(VmState::Stopped, Duration::from_secs(10))?;
        Ok(())
    }
}

/// Share the user's home directory into the Vessel at the same path, so bind
/// mounts like `-v ~/code/app:/app` resolve identically on host and guest.
fn home_share() -> Vec<nebula_core::ShareSpec> {
    match std::env::var("HOME") {
        Ok(home) if std::path::Path::new(&home).is_dir() => vec![nebula_core::ShareSpec {
            tag: "home".into(),
            host_path: home.into(),
            read_only: false,
        }],
        _ => vec![],
    }
}

fn vessel_cmdline(cfg: &Config) -> String {
    let mut cmdline = String::from(
        "console=hvc0 root=/dev/vda rw rootfstype=ext4 init=/sbin/nebula-init reboot=k panic=10",
    );
    if let Ok(home) = std::env::var("HOME") {
        // Kernel passes unknown key=value words to init's environment.
        cmdline.push_str(&format!(" NEBULA_HOME={home}"));
    }
    if let Some(port) = cfg.dns_port {
        // The guest agent relays 127.0.0.1:53 to the host gateway at this port.
        cmdline.push_str(&format!(" NEBULA_DNS_PORT={port}"));
    }
    cmdline
}
