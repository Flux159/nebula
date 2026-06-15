//! slim-net: bridges, veth pairs, IPAM, port publishing.
//!
//! v1 decision (logged in tasks/issues.md): shell out to `ip` (busybox) and
//! `iptables` instead of speaking rtnetlink — ~300 lines instead of a
//! netlink dependency tree, and the binaries are already in the rootfs.
//! In-container interface config runs `ip` with a pre_exec setns(net) hook,
//! so there is no nsenter dependency and no named-netns bookkeeping:
//! the container's /proc/<pid>/ns/net IS the handle.
//!
//! Container name resolution: /etc/hosts maintenance (slimd owns the hosts
//! files; they are bind-mounted so edits are live). DNS upstream is the
//! network gateway IP, where the vessel-agent dns relay listens on :53.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

pub const DEFAULT_NETWORK: &str = "bridge";
pub const DEFAULT_BRIDGE: &str = "slim0";
/// 10.88.<n>.0/24 per network, n allocated from this pool.
const SUBNET_BASE: (u8, u8) = (10, 88);

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkRecord {
    pub id: String,
    pub name: String,
    pub bridge: String,
    pub subnet_octet: u8, // 10.88.<this>.0/24
    pub internal: bool,
    pub created: String,
    pub labels: BTreeMap<String, String>,
    /// container id → endpoint
    pub endpoints: BTreeMap<String, Endpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Endpoint {
    pub ip: String,
    pub veth_host: String,
    pub container_name: String,
    pub aliases: Vec<String>,
    pub mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct State {
    networks: BTreeMap<String, NetworkRecord>,
    /// container id → published ports (for teardown): (proto, host_port, dest ip:port)
    published: BTreeMap<String, Vec<(String, u16, String)>>,
}

pub struct NetManager {
    state_path: PathBuf,
    state: Mutex<State>,
    /// false on tier-2 setups without iptables; publishing becomes a no-op
    /// with a warning instead of an error.
    pub firewall: bool,
}

impl NetworkRecord {
    pub fn gateway(&self) -> String {
        format!(
            "{}.{}.{}.1",
            SUBNET_BASE.0, SUBNET_BASE.1, self.subnet_octet
        )
    }
    pub fn subnet(&self) -> String {
        format!(
            "{}.{}.{}.0/24",
            SUBNET_BASE.0, SUBNET_BASE.1, self.subnet_octet
        )
    }
}

fn run(cmd: &str, args: &[&str]) -> io::Result<String> {
    let out = Command::new(cmd).args(args).output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "{cmd} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a command inside the network namespace of `pid`.
fn run_in_netns(pid: i32, cmd: &str, args: &[&str]) -> io::Result<String> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let ns = format!("/proc/{pid}/ns/net");
        let mut c = Command::new(cmd);
        c.args(args);
        unsafe {
            c.pre_exec(move || {
                let f = std::fs::File::open(&ns)?;
                use std::os::unix::io::AsRawFd;
                if libc::setns(f.as_raw_fd(), libc::CLONE_NEWNET) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let out = c.output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "[netns {pid}] {cmd} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, cmd, args);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "netns requires linux",
        ))
    }
}

impl NetManager {
    pub fn new(state_dir: &Path) -> io::Result<NetManager> {
        std::fs::create_dir_all(state_dir)?;
        let state_path = state_dir.join("networks.json");
        let state: State = std::fs::read(&state_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let firewall = Command::new("iptables")
            .args(["-t", "nat", "-L", "-n"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        Ok(NetManager {
            state_path,
            state: Mutex::new(state),
            firewall,
        })
    }

    fn save(&self, st: &State) {
        if let Ok(b) = serde_json::to_vec_pretty(st) {
            let tmp = self.state_path.with_extension("tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, &self.state_path);
            }
        }
    }

    /// Bring up the default bridge network + NAT rules. Also re-creates
    /// bridges for persisted user networks after a daemon/vessel restart.
    pub fn boot(&self) -> io::Result<()> {
        {
            let mut st = self.state.lock().unwrap();
            if !st.networks.contains_key(DEFAULT_NETWORK) {
                let rec = NetworkRecord {
                    id: rand_id(),
                    name: DEFAULT_NETWORK.into(),
                    bridge: DEFAULT_BRIDGE.into(),
                    subnet_octet: 0,
                    internal: false,
                    created: now_rfc3339(),
                    labels: BTreeMap::new(),
                    endpoints: BTreeMap::new(),
                };
                st.networks.insert(DEFAULT_NETWORK.into(), rec);
                self.save(&st);
            }
            // Stale endpoints don't survive a vessel reboot.
            for n in st.networks.values_mut() {
                n.endpoints.clear();
            }
            st.published.clear();
            self.save(&st);
        }
        let nets: Vec<NetworkRecord> = self
            .state
            .lock()
            .unwrap()
            .networks
            .values()
            .cloned()
            .collect();
        for n in &nets {
            self.ensure_bridge(n)?;
        }
        self.ensure_base_rules();
        Ok(())
    }

    fn ensure_bridge(&self, n: &NetworkRecord) -> io::Result<()> {
        if run("ip", &["link", "show", &n.bridge]).is_err() {
            run("ip", &["link", "add", &n.bridge, "type", "bridge"])?;
        }
        // addr add is idempotent-ish; ignore "File exists".
        let _ = run(
            "ip",
            &[
                "addr",
                "add",
                &format!("{}/24", n.gateway()),
                "dev",
                &n.bridge,
            ],
        );
        run("ip", &["link", "set", &n.bridge, "up"])?;
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        Ok(())
    }

    fn ensure_base_rules(&self) {
        if !self.firewall {
            return;
        }
        // Our own nat chain, attached once.
        let _ = run("iptables", &["-t", "nat", "-N", "SLIM"]);
        let have = run("iptables", &["-t", "nat", "-S", "PREROUTING"]).unwrap_or_default();
        if !have.contains("-j SLIM") {
            let _ = run(
                "iptables",
                &[
                    "-t",
                    "nat",
                    "-A",
                    "PREROUTING",
                    "-m",
                    "addrtype",
                    "--dst-type",
                    "LOCAL",
                    "-j",
                    "SLIM",
                ],
            );
        }
        let have = run("iptables", &["-t", "nat", "-S", "OUTPUT"]).unwrap_or_default();
        if !have.contains("-j SLIM") {
            let _ = run(
                "iptables",
                &[
                    "-t",
                    "nat",
                    "-A",
                    "OUTPUT",
                    "!",
                    "-d",
                    "127.0.0.0/8",
                    "-m",
                    "addrtype",
                    "--dst-type",
                    "LOCAL",
                    "-j",
                    "SLIM",
                ],
            );
        }
        let have = run("iptables", &["-t", "nat", "-S", "POSTROUTING"]).unwrap_or_default();
        let masq_src = format!("{}.{}.0.0/16", SUBNET_BASE.0, SUBNET_BASE.1);
        if !have.contains(&masq_src) {
            let _ = run(
                "iptables",
                &[
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "-s",
                    &masq_src,
                    "-j",
                    "MASQUERADE",
                ],
            );
        }
        // Hairpin: containers reaching their own published ports.
        let _ = run("iptables", &["-P", "FORWARD", "ACCEPT"]);
    }

    // ---------- network CRUD ----------

    pub fn create(
        &self,
        name: &str,
        internal: bool,
        labels: BTreeMap<String, String>,
    ) -> io::Result<NetworkRecord> {
        let mut st = self.state.lock().unwrap();
        if st.networks.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("network with name {name} already exists"),
            ));
        }
        let used: Vec<u8> = st.networks.values().map(|n| n.subnet_octet).collect();
        let octet = (1..250u8)
            .find(|o| !used.contains(o))
            .ok_or_else(|| io::Error::other("subnet pool exhausted"))?;
        let id = rand_id();
        let rec = NetworkRecord {
            bridge: format!("slim-{}", &id[..8]),
            id: id.clone(),
            name: name.to_string(),
            subnet_octet: octet,
            internal,
            created: now_rfc3339(),
            labels,
            endpoints: BTreeMap::new(),
        };
        self.ensure_bridge(&rec)?;
        st.networks.insert(name.to_string(), rec.clone());
        self.save(&st);
        Ok(rec)
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        let mut st = self.state.lock().unwrap();
        let n = st.networks.get(name).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("network {name} not found"))
        })?;
        if !n.endpoints.is_empty() {
            return Err(io::Error::other(format!(
                "network {name} has active endpoints"
            )));
        }
        if name == DEFAULT_NETWORK {
            return Err(io::Error::other(
                "bridge is a pre-defined network and cannot be removed",
            ));
        }
        let bridge = n.bridge.clone();
        let _ = run("ip", &["link", "del", &bridge]);
        st.networks.remove(name);
        self.save(&st);
        Ok(())
    }

    pub fn list(&self) -> Vec<NetworkRecord> {
        self.state
            .lock()
            .unwrap()
            .networks
            .values()
            .cloned()
            .collect()
    }

    pub fn get(&self, name_or_id: &str) -> Option<NetworkRecord> {
        let st = self.state.lock().unwrap();
        st.networks.get(name_or_id).cloned().or_else(|| {
            st.networks
                .values()
                .find(|n| n.id.starts_with(name_or_id))
                .cloned()
        })
    }

    // ---------- endpoints ----------

    /// Wire a started container (its netns = /proc/<pid>/ns/net) into a
    /// network. Returns the endpoint (ip etc.).
    pub fn connect(
        &self,
        network: &str,
        container_id: &str,
        container_name: &str,
        pid: i32,
        aliases: &[String],
    ) -> io::Result<Endpoint> {
        let (bridge, gateway, ip) = {
            let mut st = self.state.lock().unwrap();
            let n = st.networks.get_mut(network).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("network {network} not found"),
                )
            })?;
            let used: Vec<String> = n.endpoints.values().map(|e| e.ip.clone()).collect();
            let host = (2..254u8)
                .map(|h| {
                    format!(
                        "{}.{}.{}.{}",
                        SUBNET_BASE.0, SUBNET_BASE.1, n.subnet_octet, h
                    )
                })
                .find(|ip| !used.contains(ip))
                .ok_or_else(|| io::Error::other("network address pool exhausted"))?;
            (n.bridge.clone(), n.gateway(), host)
        };

        let veth_h = format!("ve{}", &container_id[..7.min(container_id.len())]);
        let veth_c = format!("vc{}", &container_id[..7.min(container_id.len())]);
        let _ = run("ip", &["link", "del", &veth_h]); // stale from a crash
        run(
            "ip",
            &[
                "link", "add", &veth_h, "type", "veth", "peer", "name", &veth_c,
            ],
        )?;
        run("ip", &["link", "set", &veth_h, "master", &bridge])?;
        run("ip", &["link", "set", &veth_h, "up"])?;
        run("ip", &["link", "set", &veth_c, "netns", &pid.to_string()])?;
        // Inside the container netns: rename to ethN, address, route.
        let eth = self.next_eth(pid);
        run_in_netns(pid, "ip", &["link", "set", &veth_c, "name", &eth])?;
        run_in_netns(
            pid,
            "ip",
            &["addr", "add", &format!("{ip}/24"), "dev", &eth],
        )?;
        run_in_netns(pid, "ip", &["link", "set", &eth, "up"])?;
        run_in_netns(pid, "ip", &["link", "set", "lo", "up"])?;
        if eth == "eth0" {
            run_in_netns(pid, "ip", &["route", "add", "default", "via", &gateway])?;
        }
        let mac = run_in_netns(pid, "cat", &[&format!("/sys/class/net/{eth}/address")])
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let ep = Endpoint {
            ip: ip.clone(),
            veth_host: veth_h,
            container_name: container_name.to_string(),
            aliases: aliases.to_vec(),
            mac,
        };
        let mut st = self.state.lock().unwrap();
        if let Some(n) = st.networks.get_mut(network) {
            n.endpoints.insert(container_id.to_string(), ep.clone());
        }
        self.save(&st);
        Ok(ep)
    }

    fn next_eth(&self, pid: i32) -> String {
        for i in 0..8 {
            let name = format!("eth{i}");
            if run_in_netns(pid, "ip", &["link", "show", &name]).is_err() {
                return name;
            }
        }
        "eth9".into()
    }

    pub fn disconnect(&self, network: &str, container_id: &str) {
        let mut st = self.state.lock().unwrap();
        if let Some(n) = st.networks.get_mut(network) {
            if let Some(ep) = n.endpoints.remove(container_id) {
                let _ = run("ip", &["link", "del", &ep.veth_host]);
            }
        }
        self.save(&st);
    }

    /// Remove a container from every network (rm/die path).
    pub fn disconnect_all(&self, container_id: &str) {
        let nets: Vec<String> = {
            let st = self.state.lock().unwrap();
            st.networks
                .iter()
                .filter(|(_, n)| n.endpoints.contains_key(container_id))
                .map(|(name, _)| name.clone())
                .collect()
        };
        for n in nets {
            self.disconnect(&n, container_id);
        }
    }

    /// All (ip, names) pairs a member of `network` should see in /etc/hosts.
    pub fn hosts_entries(&self, network: &str) -> Vec<(String, Vec<String>)> {
        let st = self.state.lock().unwrap();
        let Some(n) = st.networks.get(network) else {
            return vec![];
        };
        n.endpoints
            .values()
            .map(|e| {
                let mut names = vec![e.container_name.clone()];
                names.extend(e.aliases.iter().cloned());
                (e.ip.clone(), names)
            })
            .collect()
    }

    // ---------- port publishing ----------

    pub fn publish(
        &self,
        container_id: &str,
        dest_ip: &str,
        ports: &[(String, u16, u16)], // (proto, host_port, container_port)
    ) -> io::Result<()> {
        if ports.is_empty() {
            return Ok(());
        }
        if !self.firewall {
            eprintln!("slim-net: iptables unavailable; -p publishing skipped");
            return Ok(());
        }
        let mut recorded = Vec::new();
        for (proto, host, ctr) in ports {
            let dest = format!("{dest_ip}:{ctr}");
            run(
                "iptables",
                &[
                    "-t",
                    "nat",
                    "-A",
                    "SLIM",
                    "-p",
                    proto,
                    "--dport",
                    &host.to_string(),
                    "-j",
                    "DNAT",
                    "--to-destination",
                    &dest,
                ],
            )?;
            recorded.push((proto.clone(), *host, dest));
        }
        let mut st = self.state.lock().unwrap();
        st.published.insert(container_id.to_string(), recorded);
        self.save(&st);
        Ok(())
    }

    pub fn unpublish(&self, container_id: &str) {
        let rules = {
            let mut st = self.state.lock().unwrap();
            let r = st.published.remove(container_id);
            self.save(&st);
            r
        };
        let Some(rules) = rules else { return };
        for (proto, host, dest) in rules {
            let _ = run(
                "iptables",
                &[
                    "-t",
                    "nat",
                    "-D",
                    "SLIM",
                    "-p",
                    &proto,
                    "--dport",
                    &host.to_string(),
                    "-j",
                    "DNAT",
                    "--to-destination",
                    &dest,
                ],
            );
        }
    }

    pub fn published_ports(&self, container_id: &str) -> Vec<(String, u16, String)> {
        self.state
            .lock()
            .unwrap()
            .published
            .get(container_id)
            .cloned()
            .unwrap_or_default()
    }
}

pub fn rand_id() -> String {
    // /dev/urandom-backed 64-hex id (docker-style) without a rand crate.
    let mut buf = [0u8; 32];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_err()
    {
        // Fallback: time + pid (collision-improbable for our scale).
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let seed = t.as_nanos() as u64 ^ ((std::process::id() as u64) << 32);
        let mut x = seed;
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let days = (now.as_secs() / 86400) as i64;
    let rem = now.as_secs() % 86400;
    // Reuse the civil-date math shape from slim-runtime's jsonlog (kept tiny
    // and local here to avoid a dependency edge).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:09}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        now.subsec_nanos()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_and_time() {
        let id = rand_id();
        assert_eq!(id.len(), 64);
        assert_ne!(id, rand_id());
        let t = now_rfc3339();
        assert!(t.starts_with("20") && t.ends_with('Z'));
    }
}
