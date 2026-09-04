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
    pub fn boot(
        paths: &Paths,
        cfg: &Config,
        plan: &crate::ports::PortPlan,
    ) -> anyhow::Result<Self> {
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
        //
        // Recreated when it is missing *or* empty, not merely when it is
        // missing. A 0-byte image is the fingerprint of the old failure mode:
        // creation that died at set_len left the file behind, existence said
        // "already done", and every later boot handed the guest an empty block
        // device -- one transient full disk, and the install never recovered
        // even after space was freed. create_sized cannot produce that state
        // any more, so this only heals installs already broken by it.
        //
        // Deliberately only zero. A non-empty image of an unexpected size is
        // somebody's data disk after a data_disk_gib change, and silently
        // recreating it would delete everything they have.
        let want = eff.data_disk_gib * 1024 * 1024 * 1024;
        let have = std::fs::metadata(paths.data_img()).ok().map(|m| m.len());
        if have.is_none_or(|n| n == 0) {
            if have == Some(0) {
                tracing::warn!(
                    path = %paths.data_img().display(),
                    "data disk is empty -- an earlier creation failed part-way; recreating it"
                );
            }
            nebula_core::sparse::create_sized(&paths.data_img(), want)?;
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
        // No virtiofs on the Windows fork yet (and HOME is unix-only): home +
        // any configured extra shares. Computed once so the kernel cmdline can
        // hand the guest the SAME tag→path map the host attaches.
        let shares = if cfg!(windows) {
            vec![]
        } else {
            vessel_shares(cfg)
        };
        let spec = VmSpec {
            name: "vessel".into(),
            cpus: eff.cpus,
            mem_mib: eff.mem_mib,
            boot: BootSpec::Kernel {
                kernel,
                initramfs: None,
                cmdline: vessel_cmdline(plan, &shares),
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
            shares,
            // Nat everywhere: VZ NAT on macOS; the fork's usernet virtio-net
            // on Linux and Windows (the engine needs outbound for image
            // pulls — TSI's guest-side hijack doesn't apply to our own-init
            // disk boots).
            net: NetSpec::Nat,
            vsock: macos,
            console: ConsoleSpec::File(paths.console_log()),
            balloon: macos,
            rng: true,
            rosetta: macos,
            gpu: false,
            control_path: None,
            restore_path: None,
            vsock_ports,
            backend: None,
            // DHCP-lease stability: a fresh random MAC every boot leaks one
            // bootpd lease per restart — the battle-test sweeps exhausted the
            // VZ NAT /24 in a day (tasks/issues.md 2026-06-12). Mint once,
            // persist, reuse forever.
            mac: if macos {
                Some(load_or_create_mac(&paths.engine_mac_file())?)
            } else {
                None
            },
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
            // Short read timeout: a probe sent before the guest agent binds
            // its vsock port can be held open by the VMM instead of being
            // refused, and an untimed read_line would eat seconds of boot.
            match self
                .agent_request_with_timeout(&AgentRequest::Health, Some(Duration::from_millis(500)))
            {
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

    pub fn vsock_connect(&self, port: u32) -> anyhow::Result<nebula_core::backend::VsockStream> {
        Ok(self.vm.lock().unwrap().vsock_connect(port)?)
    }

    pub fn agent_request(&self, req: &AgentRequest) -> anyhow::Result<AgentResponse> {
        // No read timeout: exec requests legitimately stream for minutes.
        self.agent_request_with_timeout(req, None)
    }

    /// Agent request with a caller-chosen read timeout (image builds run
    /// multi-minute scripts in the guest).
    pub fn agent_request_long(
        &self,
        req: &AgentRequest,
        timeout: Duration,
    ) -> anyhow::Result<AgentResponse> {
        self.agent_request_with_timeout(req, Some(timeout))
    }

    fn agent_request_with_timeout(
        &self,
        req: &AgentRequest,
        read_timeout: Option<Duration>,
    ) -> anyhow::Result<AgentResponse> {
        let stream = self.vm.lock().unwrap().vsock_connect(VSOCK_PORT_CONTROL)?;
        stream.set_read_timeout(read_timeout)?;
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
    pub fn open_shell(
        &self,
        open: &ShellOpen,
    ) -> anyhow::Result<nebula_core::backend::VsockStream> {
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

/// The engine vessel's MAC, minted once and persisted — same contract as
/// named vessels (vessels.rs), so the bootpd lease is reused across restarts.
fn load_or_create_mac(path: &std::path::Path) -> anyhow::Result<String> {
    if let Ok(s) = std::fs::read_to_string(path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let mac = nebula_core::vessels::random_mac()?;
    std::fs::write(path, &mac)?;
    Ok(mac)
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

/// All virtiofs shares for the engine vessel: `$HOME` (tag `home`) plus each
/// configured `[[shares]]` entry that still points at a directory. Extra shares
/// get stable tags `share0`, `share1`, … in config order (skipped entries don't
/// consume a tag), and the guest mounts each at its identical host path — same
/// contract as `$HOME`. A configured path that has vanished is dropped with a
/// warning rather than failing the whole engine boot.
fn vessel_shares(cfg: &Config) -> Vec<nebula_core::ShareSpec> {
    let mut shares = home_share();
    let mut n = 0;
    for entry in &cfg.shares {
        if !entry.path.is_dir() {
            tracing::warn!(
                "configured share {} is not a directory — skipping",
                entry.path.display()
            );
            continue;
        }
        shares.push(nebula_core::ShareSpec {
            tag: format!("share{n}"),
            host_path: entry.path.clone(),
            read_only: entry.read_only,
        });
        n += 1;
    }
    shares
}

/// Lowercase hex (the guest decodes it). Paths can contain spaces/colons/commas
/// that would break kernel-cmdline word splitting; hex keeps each value to
/// `[0-9a-f]` so `NEBULA_SHARES=tag=hexpath,…` is always one clean word.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn vessel_cmdline(plan: &crate::ports::PortPlan, shares: &[nebula_core::ShareSpec]) -> String {
    let mut cmdline = String::from(
        "console=hvc0 root=/dev/vda rw rootfstype=ext4 init=/sbin/nebula-init reboot=k panic=10",
    );

    if let Ok(home) = std::env::var("HOME") {
        // Kernel passes unknown key=value words to init's environment.
        cmdline.push_str(&format!(" NEBULA_HOME={home}"));
    }
    // The guest agent relays 127.0.0.1:53 to the host gateway at this port.
    // From the preflighted plan, not the raw config: with `port_conflict =
    // "auto"` the resolver may have landed somewhere else, and a guest told
    // the wrong port has no DNS at all.
    cmdline.push_str(&format!(" NEBULA_DNS_PORT={}", plan.dns_port));
    // Hand the guest the tag→path map for every non-home share so vessel-init
    // can `mount -t virtiofs <tag> <path>` at the identical host path.
    let extras: Vec<String> = shares
        .iter()
        .filter(|s| s.tag != "home")
        .map(|s| {
            format!(
                "{}={}{}",
                s.tag,
                hex_encode(s.host_path.to_string_lossy().as_bytes()),
                if s.read_only { ":ro" } else { "" }
            )
        })
        .collect();
    if !extras.is_empty() {
        cmdline.push_str(&format!(" NEBULA_SHARES={}", extras.join(",")));
    }
    cmdline
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_core::ShareSpec;

    fn hex_decode(s: &str) -> String {
        let bytes: Vec<u8> = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn hex_encode_is_lowercase_bytes() {
        assert_eq!(hex_encode(b"/tmp/x"), "2f746d702f78");
        assert_eq!(hex_decode(&hex_encode(b"/Volumes/a b")), "/Volumes/a b");
    }

    #[test]
    fn cmdline_encodes_extra_shares_only() {
        let shares = vec![
            ShareSpec {
                tag: "home".into(),
                host_path: "/Users/x".into(),
                read_only: false,
            },
            ShareSpec {
                tag: "share0".into(),
                host_path: "/Volumes/a b".into(),
                read_only: false,
            },
        ];
        let cl = vessel_cmdline(&test_plan(), &shares);
        // home is handled via NEBULA_HOME, never re-encoded as an extra share.
        assert!(!cl.contains("home="));
        let word = cl
            .split_whitespace()
            .find(|w| w.starts_with("NEBULA_SHARES="))
            .expect("NEBULA_SHARES present");
        // One whitespace-free cmdline word, even though the path has a space.
        assert!(!word.contains(' '));
        let hex = word.trim_start_matches("NEBULA_SHARES=share0=");
        assert_eq!(hex_decode(hex), "/Volumes/a b");
    }

    #[test]
    fn cmdline_omits_shares_when_only_home() {
        let shares = vec![ShareSpec {
            tag: "home".into(),
            host_path: "/Users/x".into(),
            read_only: false,
        }];
        assert!(!vessel_cmdline(&test_plan(), &shares).contains("NEBULA_SHARES="));
    }

    fn test_plan() -> crate::ports::PortPlan {
        crate::ports::PortPlan::resolve(&Config::default())
    }

    #[test]
    fn cmdline_carries_the_planned_dns_port() {
        // Not cfg.dns_port: `port_conflict = "auto"` can move the resolver,
        // and the guest must be told where it actually landed.
        let plan = crate::ports::PortPlan {
            dns_port: 42099,
            ..crate::ports::PortPlan::resolve(&Config::default())
        };
        assert!(vessel_cmdline(&plan, &[]).contains("NEBULA_DNS_PORT=42099"));
    }
}
