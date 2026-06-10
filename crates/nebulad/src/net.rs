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
use nebula_core::proto::{AgentRequest, AgentResponse};

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
}

pub fn start(
    vessel: Arc<Vessel>,
    docker_sock: std::path::PathBuf,
    cfg: NetConfig,
) -> Arc<Mutex<NetState>> {
    let state = Arc::new(Mutex::new(NetState::default()));
    spawn_docker_watcher(vessel.clone(), docker_sock, state.clone(), cfg.k8s_port);
    spawn_dns_server(state.clone(), cfg.dns_zone, cfg.dns_port);
    state
}

// --- docker state watcher ----------------------------------------------------

fn spawn_docker_watcher(
    vessel: Arc<Vessel>,
    docker_sock: std::path::PathBuf,
    state: Arc<Mutex<NetState>>,
    k8s_port: u16,
) {
    std::thread::spawn(move || {
        // port -> (stop flag, target ip). Recreated when the guest IP moves
        // (DHCP hands out a fresh address on every boot).
        let mut forwarders: HashMap<u16, (Arc<AtomicBool>, Ipv4Addr)> = HashMap::new();
        loop {
            std::thread::sleep(Duration::from_secs(2));

            // Guest IP (rarely changes; cheap to refresh).
            let guest_ip = match vessel.agent_request(&AgentRequest::Health) {
                Ok(AgentResponse::Health(h)) => h.ip.and_then(|s| s.parse().ok()),
                _ => None,
            };

            let containers = list_containers(&docker_sock).unwrap_or_default();
            let mut names = HashSet::new();
            let mut ports = HashSet::new();
            for c in &containers {
                for n in &c.names {
                    names.insert(n.clone());
                }
                ports.extend(c.tcp_ports.iter().copied());
            }
            // Static service forwards (k3s API; certs cover 127.0.0.1).
            ports.insert(k8s_port);

            {
                let mut st = state.lock().unwrap();
                st.guest_ip = guest_ip;
                st.names = names;
                st.published_tcp = ports.clone();
            }

            // Reconcile forwarders.
            let Some(ip) = guest_ip else { continue };
            forwarders.retain(|port, (stop, fwd_ip)| {
                if ports.contains(port) && *fwd_ip == ip {
                    true
                } else {
                    stop.store(true, Ordering::SeqCst);
                    // Nudge the blocking accept() so the listener thread exits.
                    let _ = TcpStream::connect(("127.0.0.1", *port));
                    tracing::info!(port, "port forward removed (gone or IP moved)");
                    false
                }
            });
            for port in ports {
                if let std::collections::hash_map::Entry::Vacant(e) = forwarders.entry(port) {
                    let stop = Arc::new(AtomicBool::new(false));
                    if spawn_port_forward(port, ip, stop.clone()) {
                        tracing::info!(
                            port,
                            "port forward added (127.0.0.1:{port} -> {ip}:{port})"
                        );
                        e.insert((stop, ip));
                    }
                }
            }
        }
    });
}

fn spawn_port_forward(port: u16, guest_ip: Ipv4Addr, stop: Arc<AtomicBool>) -> bool {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, "cannot forward (bind failed: {e})");
            return false;
        }
    };
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(client) = conn else { continue };
            std::thread::spawn(move || {
                let Ok(upstream) =
                    TcpStream::connect_timeout(&(guest_ip, port).into(), Duration::from_secs(5))
                else {
                    return;
                };
                pump(client, upstream);
            });
        }
    });
    true
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
    pub tcp_ports: Vec<u16>,
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
        let mut tcp_ports = Vec::new();
        for p in c["Ports"].as_array().cloned().unwrap_or_default() {
            if p["Type"].as_str() == Some("tcp") {
                if let Some(public) = p["PublicPort"].as_u64() {
                    tcp_ports.push(public as u16);
                }
            }
        }
        out.push(ContainerInfo { names, tcp_ports });
    }
    Ok(out)
}

/// GET over a unix socket with Connection: close; handles Content-Length and
/// chunked transfer encoding (the two things the docker API actually sends).
fn http_get_unix(sock: &std::path::Path, path: &str) -> anyhow::Result<Vec<u8>> {
    let mut stream = std::os::unix::net::UnixStream::connect(sock)?;
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
