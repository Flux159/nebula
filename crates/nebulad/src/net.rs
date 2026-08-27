//! Host networking services:
//! - DNS resolver for the guest relay (resolves with the HOST's resolver, so
//!   VPN/split-horizon just works) plus the `*.nebula.local` zone
//! - dynamic port forwarding: published container ports appear on
//!   127.0.0.1:<port> automatically (no flags beyond `docker -p`)

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nebula_core::dns;
use nebula_core::proto::{AgentRequest, AgentResponse, VSOCK_PORT_TCPPROXY};

use crate::vessel::Vessel;

#[derive(Default)]
pub struct NetState {
    /// Guest eth0 address (forward target).
    pub guest_ip: Option<Ipv4Addr>,
    /// Container name -> () (DNS zone entries; all resolve to guest_ip).
    pub names: HashSet<String>,
    /// Host ports published by containers (tcp).
    pub published_tcp: HashSet<u16>,
}

pub struct NetConfig {
    pub dns_zone: String,
    pub dns_port: u16,
    pub k8s_port: u16,
    /// Bind published ports to the address the container asked for, rather
    /// than forcing 127.0.0.1. See [`crate::config::Config::allow_public_publish`].
    pub allow_public_publish: bool,
}

pub fn start(
    vessel: Arc<Vessel>,
    docker_sock: std::path::PathBuf,
    cfg: NetConfig,
) -> Arc<Mutex<NetState>> {
    let state = Arc::new(Mutex::new(NetState::default()));
    spawn_docker_watcher(
        vessel.clone(),
        docker_sock,
        state.clone(),
        cfg.k8s_port,
        cfg.allow_public_publish,
    );
    spawn_dns_server(state.clone(), cfg.dns_zone, cfg.dns_port);
    state
}

// --- docker state watcher ----------------------------------------------------

fn spawn_docker_watcher(
    vessel: Arc<Vessel>,
    docker_sock: std::path::PathBuf,
    state: Arc<Mutex<NetState>>,
    k8s_port: u16,
    allow_public_publish: bool,
) {
    std::thread::spawn(move || {
        // port -> (stop flag, guest target ip, guest-loopback publish, host
        // bind address). Recreated when any of them changes: the guest IP
        // moves on every boot (fresh DHCP lease), and a container republished
        // from 127.0.0.1 to 0.0.0.0 must not keep its old loopback listener.
        let mut forwarders: HashMap<u16, (Arc<AtomicBool>, Ipv4Addr, bool, IpAddr)> =
            HashMap::new();
        // Consecutive failed container listings; only used to keep the log quiet
        // while the engine is down.
        let mut list_failures: u32 = 0;
        loop {
            std::thread::sleep(Duration::from_secs(2));

            // Guest IP (rarely changes; cheap to refresh).
            let guest_ip = match vessel.agent_request(&AgentRequest::Health) {
                Ok(AgentResponse::Health(h)) => h.ip.and_then(|s| s.parse().ok()),
                _ => None,
            };

            // A failed query is not an empty engine. Treating the two alike used
            // to tear down every forward on a single hiccup and rebuild it two
            // seconds later, which is invisible to short HTTP requests but
            // kills any long-lived connection through a published port. Skip
            // the tick instead and leave the existing forwards (and the DNS
            // name set) exactly as they are.
            let containers = match list_containers(&docker_sock) {
                Ok(containers) => {
                    if list_failures > 0 {
                        tracing::info!(
                            failures = list_failures,
                            "container list recovered; reconciling port forwards"
                        );
                        list_failures = 0;
                    }
                    containers
                }
                Err(e) => {
                    list_failures += 1;
                    if list_failures == 1 {
                        tracing::warn!(
                            "container list failed ({e}); keeping existing port forwards"
                        );
                    }
                    continue;
                }
            };
            let mut names = HashSet::new();
            let mut ports = HashSet::new();
            let mut loopback_only = HashSet::new();
            // Where each port should be listened for on the HOST. Distinct
            // from `loopback_only`, which is about where dockerd bound it
            // inside the GUEST; the two coincide today only because the same
            // publish spec produces both.
            let mut host_binds: HashMap<u16, IpAddr> = HashMap::new();
            for c in &containers {
                for n in &c.names {
                    names.insert(n.clone());
                }
                for pp in &c.tcp_ports {
                    ports.insert(pp.port);
                    if pp.guest_loopback {
                        loopback_only.insert(pp.port);
                    }
                    let want = effective_bind(pp.host_ip, allow_public_publish);
                    host_binds
                        .entry(pp.port)
                        .and_modify(|cur| {
                            if wider(&want, cur) {
                                *cur = want;
                            }
                        })
                        .or_insert(want);
                }
            }
            // Static service forwards (k3s API; certs cover 127.0.0.1, so this
            // one stays on loopback regardless of the publish policy).
            ports.insert(k8s_port);

            {
                let mut st = state.lock().unwrap();
                st.guest_ip = guest_ip;
                st.names = names;
                st.published_tcp = ports.clone();
            }

            // Reconcile forwarders. macOS dials the guest IP directly (the VZ
            // NAT subnet is host-routable); Linux and Windows tunnel through
            // the agent's vsock TCP proxy (the guest IP is not host-routable
            // behind the usermode NAT).
            //
            // A loopback-scoped publish is the exception: dockerd bound it to
            // the guest's own 127.0.0.1, so dialling the NAT address just hangs
            // and the host's connection dies a second later with no
            // explanation. Those go through the vsock proxy on every platform —
            // it already falls back to the guest's loopback.
            if cfg!(target_os = "macos") && guest_ip.is_none() {
                continue;
            }
            let target_for = |port: u16| -> ForwardTarget {
                match guest_ip {
                    Some(ip) if cfg!(target_os = "macos") && !loopback_only.contains(&port) => {
                        ForwardTarget::Ip(ip)
                    }
                    _ => ForwardTarget::Vsock(vessel.clone()),
                }
            };
            let ip = guest_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
            let bind_for = |port: u16| -> IpAddr { *host_binds.get(&port).unwrap_or(&LOOPBACK) };
            forwarders.retain(|port, (stop, fwd_ip, was_loopback, bind)| {
                if ports.contains(port)
                    && *fwd_ip == ip
                    && *was_loopback == loopback_only.contains(port)
                    && *bind == bind_for(*port)
                {
                    true
                } else {
                    stop.store(true, Ordering::SeqCst);
                    // Nudge the blocking accept() so the listener thread exits.
                    // Dial the address it is actually bound to — a listener on
                    // 192.168.1.5 never sees a connection to 127.0.0.1 and
                    // would leak the thread and hold the port.
                    let nudge = if bind.is_unspecified() {
                        LOOPBACK
                    } else {
                        *bind
                    };
                    let _ = TcpStream::connect((nudge, *port));
                    tracing::info!(port, "port forward removed (gone, IP moved, or rebound)");
                    false
                }
            });
            for port in ports {
                let bind = bind_for(port);
                if let std::collections::hash_map::Entry::Vacant(e) = forwarders.entry(port) {
                    let stop = Arc::new(AtomicBool::new(false));
                    if spawn_port_forward(bind, port, target_for(port), stop.clone()) {
                        tracing::info!(port, "port forward added ({bind}:{port} -> {ip}:{port})");
                        e.insert((stop, ip, loopback_only.contains(&port), bind));
                    }
                }
            }
        }
    });
}

/// Every published port used to land here, whatever the container asked for.
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// The host address a published port should be listened on.
///
/// `host_ip` is the `HostIp` docker reported for the mapping. Loopback stays
/// loopback; anything wider is honoured only when the instance has opted in,
/// and is otherwise clamped back to 127.0.0.1 — the historical behaviour, and
/// the safe end of the trade.
fn effective_bind(host_ip: IpAddr, allow_public_publish: bool) -> IpAddr {
    if host_ip.is_loopback() {
        // Normalise ::1 to 127.0.0.1: the forward dials IPv4 upstream anyway,
        // and this keeps a v6-loopback publish behaving as it always has.
        LOOPBACK
    } else if allow_public_publish {
        host_ip
    } else {
        LOOPBACK
    }
}

/// Is `a` a broader binding than `b`? A port published twice (the v4 and v6
/// mappings of one `-p`, or by two containers) gets the widest of them, which
/// mirrors how `loopback_only` is the AND of every mapping.
fn wider(a: &IpAddr, b: &IpAddr) -> bool {
    let rank = |ip: &IpAddr| -> u8 {
        if ip.is_unspecified() {
            2
        } else if ip.is_loopback() {
            0
        } else {
            1
        }
    };
    match rank(a).cmp(&rank(b)) {
        std::cmp::Ordering::Greater => true,
        // Same breadth: prefer IPv4. Binding 0.0.0.0 is the predictable choice
        // on hosts where a v6 wildcard may or may not be dual-stack.
        std::cmp::Ordering::Equal => a.is_ipv4() && b.is_ipv6(),
        std::cmp::Ordering::Less => false,
    }
}

/// Where a host-port forwarder sends bytes.
#[derive(Clone)]
enum ForwardTarget {
    /// Dial <guest_ip>:<port> directly (VZ NAT).
    Ip(Ipv4Addr),
    /// Tunnel through the agent vsock TCP proxy (libkrun/KVM, WHP later).
    Vsock(Arc<Vessel>),
}

fn spawn_port_forward(
    bind: IpAddr,
    port: u16,
    target: ForwardTarget,
    stop: Arc<AtomicBool>,
) -> bool {
    let listener = match TcpListener::bind((bind, port)) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, "cannot forward (bind {bind}:{port} failed: {e})");
            return false;
        }
    };
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(client) = conn else { continue };
            let target = target.clone();
            std::thread::spawn(move || match target {
                ForwardTarget::Ip(ip) => {
                    let Ok(upstream) =
                        TcpStream::connect_timeout(&(ip, port).into(), Duration::from_secs(5))
                    else {
                        return;
                    };
                    pump(client, upstream);
                }
                ForwardTarget::Vsock(vessel) => {
                    let Ok(mut upstream) = vessel.vsock_connect(VSOCK_PORT_TCPPROXY) else {
                        return;
                    };
                    if upstream.write_all(&port.to_be_bytes()).is_err() {
                        return;
                    }
                    pump_unix(client, upstream);
                }
            });
        }
    });
    true
}

/// pump() for a TcpStream<->VsockStream pair (the vsock proxy path).
fn pump_unix(a: TcpStream, b: nebula_core::backend::VsockStream) {
    let _ = a.set_nodelay(true);
    let (mut ar, mut aw) = (a.try_clone().unwrap(), a);
    let (mut br, mut bw) = (b.try_clone().unwrap(), b);
    let t = std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match ar.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if bw.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = bw.shutdown(std::net::Shutdown::Write);
    });
    let mut buf = [0u8; 65536];
    loop {
        match br.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if aw.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = aw.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
}

fn pump(a: TcpStream, b: TcpStream) {
    let _ = a.set_nodelay(true);
    let _ = b.set_nodelay(true);
    let (mut ar, mut aw) = (a.try_clone().unwrap(), a);
    let (mut br, mut bw) = (b.try_clone().unwrap(), b);
    let t = std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match ar.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if bw.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = bw.shutdown(std::net::Shutdown::Write);
    });
    let mut buf = [0u8; 65536];
    loop {
        match br.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if aw.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = aw.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
}

// --- DNS server ---------------------------------------------------------------

fn spawn_dns_server(state: Arc<Mutex<NetState>>, zone: String, port: u16) {
    std::thread::spawn(move || {
        let sock = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("dns bind :{port} failed: {e}");
                return;
            }
        };
        tracing::info!("dns resolver on udp:{port} (zone {zone})");
        let mut buf = [0u8; 1500];
        loop {
            let Ok((n, peer)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let Some((id, q)) = dns::parse_query(&buf[..n]) else {
                continue;
            };
            let resp = answer(id, &q, &state, &zone);
            let _ = sock.send_to(&resp, peer);
        }
    });
}

fn answer(id: u16, q: &dns::Question, state: &Arc<Mutex<NetState>>, zone: &str) -> Vec<u8> {
    let name = q.name.trim_end_matches('.').to_ascii_lowercase();

    // Our zone: <container>.<zone> (and the bare zone) -> guest IP.
    let suffix = format!(".{zone}");
    if name == zone || name.ends_with(&suffix) {
        let st = state.lock().unwrap();
        let Some(ip) = st.guest_ip else {
            return dns::build_error(id, q, false);
        };
        let label = name.trim_end_matches(&suffix);
        let known = name == zone || label == "vessel" || st.names.contains(label);
        return if known {
            dns::build_response(id, q, &[IpAddr::V4(ip)], 5)
        } else {
            dns::build_error(id, q, true)
        };
    }

    // Everything else: the host's resolver (getaddrinfo honors VPN/etc.).
    match (name.as_str(), 0u16).to_socket_addrs() {
        Ok(addrs) => {
            let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
            dns::build_response(id, q, &ips, 30)
        }
        Err(_) => dns::build_error(id, q, true),
    }
}

use std::net::ToSocketAddrs;

// --- minimal docker API client (HTTP/1.1 over the proxied unix socket) --------

pub struct ContainerInfo {
    pub names: Vec<String>,
    pub tcp_ports: Vec<PublishedPort>,
}

/// One published TCP port, and the two independent things docker's `HostIp`
/// tells us about it.
pub struct PublishedPort {
    pub port: u16,
    /// dockerd bound it to the *guest's* own 127.0.0.1, so it is invisible on
    /// the guest's NAT address and must be reached through the vsock proxy.
    /// A statement about the guest side.
    pub guest_loopback: bool,
    /// The address the publish asked us to listen on, on the *host* side.
    /// These coincide today only because one publish spec produces both;
    /// collapsing them into a single bool is what made `-p 0.0.0.0:...`
    /// unreachable from the LAN.
    pub host_ip: IpAddr,
}

fn list_containers(docker_sock: &std::path::Path) -> anyhow::Result<Vec<ContainerInfo>> {
    let body = http_get_unix(docker_sock, "/v1.43/containers/json")?;
    let parsed: serde_json::Value = serde_json::from_slice(&body)?;
    let mut out = Vec::new();
    for c in parsed.as_array().cloned().unwrap_or_default() {
        let names = c["Names"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim_start_matches('/').to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        // A port can appear twice (v4 and v6): it is loopback-only when every
        // mapping is loopback, and takes the widest host address of them.
        let mut bindings: std::collections::HashMap<u16, (bool, IpAddr)> =
            std::collections::HashMap::new();
        for p in c["Ports"].as_array().cloned().unwrap_or_default() {
            if p["Type"].as_str() != Some("tcp") {
                continue;
            }
            let Some(public) = p["PublicPort"].as_u64() else {
                continue;
            };
            let raw = p["IP"].as_str().unwrap_or_default();
            // An absent or unparseable IP means the wildcard, which is what
            // docker reports for a bare `-p 8080:80`.
            let host_ip: IpAddr = raw.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
            let loopback = host_ip.is_loopback();
            bindings
                .entry(public as u16)
                .and_modify(|(only, ip)| {
                    *only &= loopback;
                    if wider(&host_ip, ip) {
                        *ip = host_ip;
                    }
                })
                .or_insert((loopback, host_ip));
        }
        let tcp_ports = bindings
            .into_iter()
            .map(|(port, (guest_loopback, host_ip))| PublishedPort {
                port,
                guest_loopback,
                host_ip,
            })
            .collect();
        out.push(ContainerInfo { names, tcp_ports });
    }
    Ok(out)
}

/// GET over a unix socket with Connection: close; handles Content-Length and
/// chunked transfer encoding (the two things the docker API actually sends).
fn http_get_unix(sock: &std::path::Path, path: &str) -> anyhow::Result<Vec<u8>> {
    let mut stream = nebula_core::ipc::connect(sock)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n"
    )?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("no header terminator"))?;
    let headers = String::from_utf8_lossy(&raw[..header_end]).to_ascii_lowercase();
    anyhow::ensure!(
        headers.starts_with("http/1.1 200"),
        "docker API: {}",
        &headers[..headers.len().min(40)]
    );
    let body = &raw[header_end + 4..];

    if headers.contains("transfer-encoding: chunked") {
        let mut out = Vec::new();
        let mut pos = 0;
        loop {
            let line_end = body[pos..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or_else(|| anyhow::anyhow!("bad chunk header"))?;
            let size = usize::from_str_radix(
                String::from_utf8_lossy(&body[pos..pos + line_end]).trim(),
                16,
            )?;
            pos += line_end + 2;
            if size == 0 {
                break;
            }
            out.extend_from_slice(&body[pos..pos + size]);
            pos += size + 2;
        }
        Ok(out)
    } else {
        Ok(body.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn loopback_publishes_stay_loopback_either_way() {
        for allow in [false, true] {
            assert_eq!(effective_bind(ip("127.0.0.1"), allow), LOOPBACK);
            // ::1 normalises to 127.0.0.1 — the upstream dial is v4 anyway.
            assert_eq!(effective_bind(ip("::1"), allow), LOOPBACK);
        }
    }

    #[test]
    fn wide_publishes_are_clamped_until_opted_in() {
        // The bug: 0.0.0.0 was unreachable from the LAN because it landed here.
        assert_eq!(effective_bind(ip("0.0.0.0"), false), LOOPBACK);
        assert_eq!(effective_bind(ip("192.168.1.5"), false), LOOPBACK);
        // ...and is honoured once the instance opts in.
        assert_eq!(effective_bind(ip("0.0.0.0"), true), ip("0.0.0.0"));
        assert_eq!(effective_bind(ip("192.168.1.5"), true), ip("192.168.1.5"));
    }

    #[test]
    fn widest_binding_wins_across_mappings() {
        assert!(wider(&ip("0.0.0.0"), &ip("127.0.0.1")));
        assert!(wider(&ip("192.168.1.5"), &ip("127.0.0.1")));
        assert!(wider(&ip("0.0.0.0"), &ip("192.168.1.5")));
        assert!(!wider(&ip("127.0.0.1"), &ip("0.0.0.0")));
        assert!(!wider(&ip("0.0.0.0"), &ip("0.0.0.0")));
        // One `-p 8080:80` reports both wildcards; take the v4 one.
        assert!(wider(&ip("0.0.0.0"), &ip("::")));
        assert!(!wider(&ip("::"), &ip("0.0.0.0")));
    }

    #[test]
    fn a_bare_publish_parses_as_the_wildcard() {
        // Docker omits IP for `-p 8080:80` in some versions; absent must not
        // silently become loopback, or opting in would change nothing.
        let raw = "";
        let host_ip: IpAddr = raw.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert!(host_ip.is_unspecified());
        assert!(!host_ip.is_loopback());
    }

    #[test]
    fn parses_container_json_shape() {
        let body =
            br#"[{"Names":["/web"],"Ports":[{"Type":"tcp","PrivatePort":80,"PublicPort":8080}]}]"#;
        let parsed: serde_json::Value = serde_json::from_slice(body).unwrap();
        let c = &parsed.as_array().unwrap()[0];
        assert_eq!(c["Names"][0].as_str().unwrap(), "/web");
        assert_eq!(c["Ports"][0]["PublicPort"].as_u64().unwrap(), 8080);
    }
}
