//! Port planning, the startup preflight, and the live bind report.
//!
//! Two nebulad instances — a standalone install and an embedded one under its
//! own `NEBULA_HOME` — share the host's port space. Before this module the
//! second one to start bound what it could, logged a warning for the rest and
//! served a half-working engine: `up` reported success, `status` looked
//! healthy, and the damage surfaced minutes later as an unrelated-looking
//! error somewhere else (issue #22).
//!
//! So the configured ports are probed *before* the VM boots. On a conflict the
//! daemon either refuses to start, naming the port and (best effort) the
//! instance holding it, or — with `port_conflict = "auto"` — picks free ports
//! and says which. Whatever binds afterwards is recorded in [`binds`], so a
//! damaged instance is visible in `nebula status` rather than only in a log
//! line nobody reads.

use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use nebula_core::proto::{PortBinding, HOST_DNS_UDP_PORT};

use crate::config::Config;

pub const DEFAULT_K8S_PORT: u16 = 6443;
pub const DEFAULT_DNS_ZONE: &str = "nebula.local";

/// Every host port this instance intends to own, resolved from config + env.
/// One source of truth: the preflight probes exactly what the services later
/// bind, so "the check passed" and "the bind worked" cannot disagree.
#[derive(Debug, Clone, PartialEq)]
pub struct PortPlan {
    pub api_host: String,
    /// 0 disables the REST API (and its preflight check).
    pub api_port: u16,
    pub dns_port: u16,
    pub k8s_port: u16,
    pub dns_zone: String,
}

impl PortPlan {
    /// Resolve the plan the way the services themselves do — `api::start`
    /// honours NEBULA_API_HOST over config, `serve` falls back to the proto
    /// defaults. Kept here so all three read the same values.
    pub fn resolve(cfg: &Config) -> Self {
        Self {
            api_host: std::env::var("NEBULA_API_HOST")
                .ok()
                .filter(|h| !h.is_empty())
                .or_else(|| cfg.api_host.clone())
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            api_port: cfg.api_port.unwrap_or(crate::api::DEFAULT_API_PORT),
            dns_port: cfg.dns_port.unwrap_or(HOST_DNS_UDP_PORT),
            k8s_port: cfg.k8s_port.unwrap_or(DEFAULT_K8S_PORT),
            dns_zone: cfg
                .dns_zone
                .clone()
                .unwrap_or_else(|| DEFAULT_DNS_ZONE.to_string()),
        }
    }

    /// The three fixed service ports, in the order they are reported.
    fn checks(&self) -> Vec<PortCheck> {
        let mut v = Vec::new();
        if self.api_port != 0 {
            v.push(PortCheck {
                setting: "api_port",
                host: self.api_host.clone(),
                port: self.api_port,
                proto: Proto::Tcp,
            });
        }
        v.push(PortCheck {
            setting: "dns_port",
            // spawn_dns_server binds the wildcard: the guest reaches it on the
            // NAT gateway address, not on loopback.
            host: "0.0.0.0".into(),
            port: self.dns_port,
            proto: Proto::Udp,
        });
        v.push(PortCheck {
            setting: "k8s_port",
            host: "127.0.0.1".into(),
            port: self.k8s_port,
            proto: Proto::Tcp,
        });
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    fn as_str(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

struct PortCheck {
    setting: &'static str,
    host: String,
    port: u16,
    proto: Proto,
}

/// What to do when a configured port is already taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Refuse to start (default): an explicit port in config.toml is a
    /// promise to clients, and silently serving on a different one is worse
    /// than not serving at all.
    Fail,
    /// Pick the next free port and log it loudly. For embedders that would
    /// rather come up than match a number.
    Auto,
}

impl ConflictPolicy {
    pub fn resolve(cfg: &Config) -> anyhow::Result<Self> {
        let raw = std::env::var("NEBULA_PORT_CONFLICT")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| cfg.port_conflict.clone());
        match raw.as_deref() {
            None | Some("fail") => Ok(Self::Fail),
            Some("auto") => Ok(Self::Auto),
            Some(other) => anyhow::bail!(
                "port_conflict = {other:?} is not valid (expected \"fail\" or \"auto\")"
            ),
        }
    }
}

/// Probe every fixed port, and either return the plan unchanged, return one
/// with free ports substituted (`Auto`), or fail with a message that names the
/// conflict and whoever is holding it.
pub fn preflight(plan: &PortPlan, policy: ConflictPolicy) -> anyhow::Result<PortPlan> {
    let mut out = plan.clone();
    let mut conflicts: Vec<Conflict> = Vec::new();

    for check in plan.checks() {
        let Err(e) = probe(&check.host, check.port, check.proto) else {
            continue;
        };
        if e.kind() != std::io::ErrorKind::AddrInUse {
            // Not a collision: an unroutable api_host, a privileged port, a
            // sandbox denial. Still fatal, and still clearer here than as a
            // warning from a background thread ten seconds later.
            anyhow::bail!(
                "cannot bind {} {}:{} ({}): {e}",
                check.proto.as_str(),
                check.host,
                check.port,
                check.setting
            );
        }
        let holder = holder_of(check.port, check.proto);
        match policy {
            ConflictPolicy::Auto => {
                let Some(free) = free_near(&check) else {
                    conflicts.push(Conflict { check, holder });
                    continue;
                };
                tracing::warn!(
                    setting = check.setting,
                    wanted = check.port,
                    chosen = free,
                    holder = holder.as_ref().map(|h| h.describe()).unwrap_or_default(),
                    "port in use; picked a free one (port_conflict = \"auto\")"
                );
                match check.setting {
                    "api_port" => out.api_port = free,
                    "dns_port" => out.dns_port = free,
                    "k8s_port" => out.k8s_port = free,
                    _ => {}
                }
            }
            ConflictPolicy::Fail => conflicts.push(Conflict { check, holder }),
        }
    }

    if conflicts.is_empty() {
        return Ok(out);
    }
    Err(anyhow::anyhow!(conflict_message(plan, &conflicts)))
}

struct Conflict {
    check: PortCheck,
    holder: Option<Holder>,
}

fn conflict_message(plan: &PortPlan, conflicts: &[Conflict]) -> String {
    let mut m = String::new();
    for c in conflicts {
        m.push_str(&format!(
            "{} {} ({}) is already in use",
            c.check.proto.as_str(),
            c.check.port,
            c.check.setting
        ));
        match &c.holder {
            Some(h) => m.push_str(&format!(" by {}\n", h.describe())),
            None => m.push_str(" by another process\n"),
        }
    }

    // The whole point of failing here is that the next step is obvious.
    let peer_home = conflicts
        .iter()
        .find_map(|c| c.holder.as_ref().and_then(|h| h.home.clone()));
    m.push_str("\nTwo Nebula instances cannot share a port. Either:\n");
    match &peer_home {
        Some(home) => m.push_str(&format!(
            "  - stop the instance holding them:  NEBULA_HOME={} nebula down\n",
            shell_quote(&home.to_string_lossy())
        )),
        None => m.push_str("  - stop whatever holds those ports\n"),
    }
    m.push_str("  - or give this instance its own ports in config.toml:\n");
    for line in suggested_config(plan, conflicts) {
        m.push_str(&format!("        {line}\n"));
    }
    m.push_str("  - or set port_conflict = \"auto\" to bind free ports at startup\n");
    m
}

/// A config.toml block using free ports, so the fix is copy-paste rather than
/// another round of guessing which numbers are safe.
fn suggested_config(plan: &PortPlan, conflicts: &[Conflict]) -> Vec<String> {
    let mut lines = Vec::new();
    for c in conflicts {
        let value = free_near(&c.check).map(|p| p.to_string());
        match value {
            Some(p) => lines.push(format!("{} = {p}", c.check.setting)),
            None => lines.push(format!("{} = <a free port>", c.check.setting)),
        }
    }
    // Sharing a zone across instances makes name resolution ambiguous in a
    // way nothing else complains about, so mention it while we are here.
    if plan.dns_zone == DEFAULT_DNS_ZONE {
        lines.push(format!("dns_zone = \"{}\"", "my-instance.local"));
    }
    lines
}

// --- who holds the port ------------------------------------------------------

/// The process holding a port, and — when it is a nebulad — which instance.
pub struct Holder {
    pub pid: u32,
    pub command: String,
    pub home: Option<PathBuf>,
}

impl Holder {
    pub fn describe(&self) -> String {
        match &self.home {
            Some(home) => format!("nebulad pid {} (NEBULA_HOME={})", self.pid, home.display()),
            None => format!("{} pid {}", self.command, self.pid),
        }
    }
}

/// Best effort: no holder information is a worse message, never a failure.
fn holder_of(port: u16, proto: Proto) -> Option<Holder> {
    let (pid, command) = port_owner(port, proto)?;
    let home = if command.contains("nebulad") {
        nebula_home_of(pid)
    } else {
        None
    };
    Some(Holder { pid, command, home })
}

#[cfg(unix)]
fn port_owner(port: u16, proto: Proto) -> Option<(u32, String)> {
    // -n -P: no DNS or /etc/services lookups, which is what makes lsof slow.
    let sel = match proto {
        Proto::Tcp => format!("-iTCP:{port}"),
        Proto::Udp => format!("-iUDP:{port}"),
    };
    let mut cmd = std::process::Command::new("lsof");
    cmd.args(["-n", "-P", &sel, "-Fpc"]);
    if proto == Proto::Tcp {
        cmd.arg("-sTCP:LISTEN");
    }
    let out = cmd.output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pid = None;
    for line in text.lines() {
        match line.split_at(1) {
            ("p", rest) => pid = rest.parse::<u32>().ok(),
            ("c", rest) => {
                if let Some(pid) = pid {
                    return Some((pid, rest.to_string()));
                }
            }
            _ => {}
        }
    }
    pid.map(|p| (p, "unknown".to_string()))
}

#[cfg(windows)]
fn port_owner(port: u16, proto: Proto) -> Option<(u32, String)> {
    let out = std::process::Command::new("netstat")
        .args([
            "-ano",
            "-p",
            if proto == Proto::Tcp { "TCP" } else { "UDP" },
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let needle = format!(":{port}");
    let pid = text.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        // TCP rows carry a state column, UDP rows do not.
        let local = f.get(1)?;
        if !local.ends_with(&needle) {
            return None;
        }
        if proto == Proto::Tcp && f.get(3) != Some(&"LISTENING") {
            return None;
        }
        f.last()?.parse::<u32>().ok()
    })?;
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let name = text.split('"').nth(1).unwrap_or("unknown").to_string();
    Some((pid, name))
}

/// Which `NEBULA_HOME` a running nebulad belongs to, read off its open files:
/// every instance holds `<home>/run/nebulad.sock`. A process's environment is
/// not readable on macOS, but its open files are — and the socket path is
/// exactly the identifier the other instance is addressed by.
#[cfg(unix)]
fn nebula_home_of(pid: u32) -> Option<PathBuf> {
    let out = std::process::Command::new("lsof")
        .args(["-n", "-P", "-p", &pid.to_string(), "-Fn"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let Some(path) = line.strip_prefix('n') else {
            continue;
        };
        if let Some(home) = path.strip_suffix("/run/nebulad.sock") {
            return Some(PathBuf::from(home));
        }
    }
    None
}

#[cfg(windows)]
fn nebula_home_of(_pid: u32) -> Option<PathBuf> {
    // No lsof equivalent worth shelling out to; the pid alone identifies it.
    None
}

// --- probing -----------------------------------------------------------------

/// Bind and immediately drop. TOCTOU against a racing process is fine: the
/// real bind follows within seconds, and its failure is now reported through
/// [`set_bind`] instead of vanishing into a warning.
fn probe(host: &str, port: u16, proto: Proto) -> std::io::Result<()> {
    match proto {
        Proto::Tcp => TcpListener::bind((host, port)).map(drop),
        Proto::Udp => UdpSocket::bind((host, port)).map(drop),
    }
}

/// The next free port at or above the configured one (never below: a lower
/// number is more likely to belong to something else entirely).
fn free_near(check: &PortCheck) -> Option<u16> {
    (check.port.saturating_add(1)..=check.port.saturating_add(200))
        .find(|&p| probe(&check.host, p, check.proto).is_ok())
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

// --- live bind report ---------------------------------------------------------

/// Every listener nebulad currently owns, keyed by service name (`api`, `dns`)
/// or by `port <n>` for a published container port. `ok = false` entries are
/// the state that used to be invisible: they answer "why does this healthy
/// instance not serve?" in `nebula status`, where someone would look.
static BINDS: OnceLock<Mutex<std::collections::BTreeMap<String, PortBinding>>> = OnceLock::new();

fn binds_map() -> &'static Mutex<std::collections::BTreeMap<String, PortBinding>> {
    BINDS.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

pub fn set_bind(service: impl Into<String>, addr: impl Into<String>, error: Option<String>) {
    let service = service.into();
    let binding = PortBinding {
        service: service.clone(),
        addr: addr.into(),
        ok: error.is_none(),
        error,
    };
    binds_map().lock().unwrap().insert(service, binding);
}

pub fn clear_bind(service: &str) {
    binds_map().lock().unwrap().remove(service);
}

pub fn binds() -> Vec<PortBinding> {
    binds_map().lock().unwrap().values().cloned().collect()
}

/// How many listeners are currently failing — the one number worth putting in
/// a shutdown or status line.
pub fn failing() -> usize {
    binds_map()
        .lock()
        .unwrap()
        .values()
        .filter(|b| !b.ok)
        .count()
}

// --- peer instances -----------------------------------------------------------

/// Other nebulad instances running on this host, by `NEBULA_HOME`.
///
/// Found the same way a port holder is: every instance holds
/// `<home>/run/nebulad.sock` open, so one `lsof` over live nebulads maps the
/// whole set. Best effort — no lsof, no peers, no error.
#[cfg(unix)]
pub fn peer_homes() -> Vec<PathBuf> {
    let me = std::process::id();
    let Ok(out) = std::process::Command::new("lsof")
        .args(["-n", "-P", "-c", "nebulad", "-a", "-U", "-Fpn"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut homes = Vec::new();
    let mut pid = None;
    for line in text.lines() {
        match line.split_at(1) {
            ("p", rest) => pid = rest.parse::<u32>().ok(),
            ("n", rest) => {
                if pid == Some(me) {
                    continue;
                }
                if let Some(home) = rest.strip_suffix("/run/nebulad.sock") {
                    let home = PathBuf::from(home);
                    if !homes.contains(&home) {
                        homes.push(home);
                    }
                }
            }
            _ => {}
        }
    }
    homes
}

#[cfg(not(unix))]
pub fn peer_homes() -> Vec<PathBuf> {
    Vec::new()
}

/// Warn when a peer instance we could identify shares this one's DNS zone:
/// two zones with the same name resolve container names ambiguously, and
/// nothing else in the stack complains about it.
pub fn warn_shared_dns_zone(plan: &PortPlan, peer_home: &Path) {
    let peer_cfg = match Config::load(&peer_home.join("config.toml")) {
        Ok(c) => c,
        Err(_) => return,
    };
    let peer_zone = peer_cfg
        .dns_zone
        .unwrap_or_else(|| DEFAULT_DNS_ZONE.to_string());
    if peer_zone == plan.dns_zone {
        tracing::warn!(
            zone = %plan.dns_zone,
            peer = %peer_home.display(),
            "another instance uses the same dns_zone — container names resolve ambiguously; \
             give one of them its own dns_zone"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(toml_src: &str) -> Config {
        toml::from_str(toml_src).unwrap()
    }

    #[test]
    fn plan_uses_defaults() {
        let p = PortPlan::resolve(&Config::default());
        assert_eq!(p.api_port, crate::api::DEFAULT_API_PORT);
        assert_eq!(p.dns_port, HOST_DNS_UDP_PORT);
        assert_eq!(p.k8s_port, DEFAULT_K8S_PORT);
        assert_eq!(p.dns_zone, DEFAULT_DNS_ZONE);
    }

    #[test]
    fn plan_reads_config() {
        let p = PortPlan::resolve(&cfg_from(
            "api_port = 7462\ndns_port = 42062\nk8s_port = 6462\ndns_zone = \"galaxy.local\"\n",
        ));
        assert_eq!(p.api_port, 7462);
        assert_eq!(p.dns_port, 42062);
        assert_eq!(p.k8s_port, 6462);
        assert_eq!(p.dns_zone, "galaxy.local");
    }

    #[test]
    fn api_port_zero_is_not_checked() {
        let plan = PortPlan::resolve(&cfg_from("api_port = 0\n"));
        assert!(plan.checks().iter().all(|c| c.setting != "api_port"));
    }

    #[test]
    fn policy_parses() {
        assert_eq!(
            ConflictPolicy::resolve(&Config::default()).unwrap(),
            ConflictPolicy::Fail
        );
        assert_eq!(
            ConflictPolicy::resolve(&cfg_from("port_conflict = \"auto\"\n")).unwrap(),
            ConflictPolicy::Auto
        );
        assert!(ConflictPolicy::resolve(&cfg_from("port_conflict = \"maybe\"\n")).is_err());
    }

    #[test]
    fn preflight_fails_on_a_held_port() {
        let held = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let plan = PortPlan {
            api_host: "127.0.0.1".into(),
            api_port: port,
            dns_port: 0, // 0 = ephemeral, always bindable
            k8s_port: 0,
            dns_zone: DEFAULT_DNS_ZONE.into(),
        };
        let err = preflight(&plan, ConflictPolicy::Fail)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!("tcp {port} (api_port) is already in use")),
            "{err}"
        );
        assert!(err.contains("port_conflict"), "{err}");
    }

    #[test]
    fn auto_policy_moves_off_a_held_port() {
        let held = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let plan = PortPlan {
            api_host: "127.0.0.1".into(),
            api_port: port,
            dns_port: 0,
            k8s_port: 0,
            dns_zone: DEFAULT_DNS_ZONE.into(),
        };
        let out = preflight(&plan, ConflictPolicy::Auto).unwrap();
        assert_ne!(out.api_port, port);
        assert!(out.api_port > port);
    }

    #[test]
    fn preflight_passes_when_ports_are_free() {
        let plan = PortPlan {
            api_host: "127.0.0.1".into(),
            api_port: 0, // API disabled: not probed at all
            dns_port: 0,
            k8s_port: 0,
            dns_zone: DEFAULT_DNS_ZONE.into(),
        };
        assert_eq!(preflight(&plan, ConflictPolicy::Fail).unwrap(), plan);
    }

    #[test]
    fn bind_report_tracks_failures() {
        set_bind("test-ok", "127.0.0.1:1", None);
        set_bind(
            "test-bad",
            "127.0.0.1:2",
            Some("Address already in use".into()),
        );
        let all = binds();
        assert!(all.iter().any(|b| b.service == "test-ok" && b.ok));
        assert!(all
            .iter()
            .any(|b| b.service == "test-bad" && !b.ok && b.error.is_some()));
        assert!(failing() >= 1);
        clear_bind("test-ok");
        clear_bind("test-bad");
        assert!(!binds().iter().any(|b| b.service.starts_with("test-")));
    }

    #[test]
    fn shell_quote_handles_spaces() {
        assert_eq!(shell_quote("/Users/x/.nebula"), "/Users/x/.nebula");
        assert_eq!(
            shell_quote("/Users/x/Application Support/n"),
            "'/Users/x/Application Support/n'"
        );
    }
}
