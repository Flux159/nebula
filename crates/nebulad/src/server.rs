//! Control socket server: JSON-lines over a unix socket, one request per
//! connection (the `shell` op upgrades the connection to a raw byte bridge).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use nebula_core::proto::*;

use crate::paths::Paths;
use crate::vessel::Vessel;

pub fn serve(paths: &Paths, vessel: Vessel) -> anyhow::Result<()> {
    let sock_path = paths.control_sock();
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;
    tracing::info!(sock = %sock_path.display(), "control socket ready");

    let vessel = Arc::new(vessel);
    let shutdown = Arc::new(AtomicBool::new(false));

    // Engine socket proxies: host unix socket <-> guest vsock <-> guest daemon.
    spawn_unix_proxy(vessel.clone(), paths.docker_sock(), VSOCK_PORT_DOCKER);
    spawn_unix_proxy(
        vessel.clone(),
        paths.containerd_sock(),
        VSOCK_PORT_CONTAINERD,
    );

    // DNS resolver + dynamic port forwarding (Phase 3).
    let _net = crate::net::start(vessel.clone(), paths.docker_sock());

    // Elastic memory (Phase 4).
    let balloon = crate::balloon::start(vessel.clone());

    // Watchdog: if the Vessel dies unexpectedly, exit the daemon so the next
    // `nebula up` (or launchd KeepAlive) gets a clean restart instead of a
    // zombie control socket.
    {
        let vessel = vessel.clone();
        let shutdown = shutdown.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            if shutdown.load(Ordering::SeqCst) {
                return;
            }
            let st = vessel.state();
            if matches!(
                st,
                nebula_core::backend::VmState::Failed | nebula_core::backend::VmState::Stopped
            ) {
                tracing::error!(?st, "vessel died unexpectedly; exiting for clean restart");
                std::process::exit(70); // EX_SOFTWARE; launchd restarts us
            }
        });
    }

    for conn in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let Ok(conn) = conn else { continue };
        let vessel = vessel.clone();
        let balloon = balloon.clone();
        let shutdown_for_conn = shutdown.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(conn, &vessel, &balloon, &shutdown_for_conn) {
                tracing::debug!("connection error: {e:#}");
            }
        });
        // `down` is handled inline below via the shutdown flag; the listener
        // loop needs a nudge, which the closing client connection provides.
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
    }

    tracing::info!("shutting down vessel");
    vessel.stop(false)?;
    Ok(())
}

fn handle(
    conn: UnixStream,
    vessel: &Vessel,
    balloon: &crate::balloon::BalloonState,
    shutdown: &AtomicBool,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut writer = conn;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let req: DaemonRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            respond(
                &mut writer,
                &DaemonResponse::Error {
                    message: format!("bad request: {e}"),
                },
            )?;
            return Ok(());
        }
    };

    match req {
        DaemonRequest::Status => {
            let agent = match vessel.agent_request(&AgentRequest::Health) {
                Ok(AgentResponse::Health(h)) => Some(h),
                _ => None,
            };
            let mem = match vessel.agent_request(&AgentRequest::MemStats) {
                Ok(AgentResponse::MemStats(m)) => Some(m),
                _ => None,
            };
            let status = DaemonStatus {
                daemon_version: env!("CARGO_PKG_VERSION").into(),
                daemon_pid: std::process::id(),
                vm_state: format!("{:?}", vessel.state()),
                backend: "vz".into(),
                cpus: vessel.spec.cpus,
                mem_mib: vessel.spec.mem_mib,
                agent,
                mem,
                uptime_secs: vessel.started_at.elapsed().as_secs(),
            };
            respond(&mut writer, &DaemonResponse::Status(status))
        }
        DaemonRequest::Down { force } => {
            shutdown.store(true, Ordering::SeqCst);
            let resp = match vessel.stop(force) {
                Ok(()) => DaemonResponse::Ok,
                Err(e) => DaemonResponse::Error {
                    message: format!("{e:#}"),
                },
            };
            respond(&mut writer, &resp)?;
            // Exit the daemon once the reply is on the wire.
            std::process::exit(0);
        }
        DaemonRequest::Agent { request } => {
            let resp = match vessel.agent_request(&request) {
                Ok(r) => DaemonResponse::Agent { response: r },
                Err(e) => DaemonResponse::Error {
                    message: format!("{e:#}"),
                },
            };
            respond(&mut writer, &resp)
        }
        DaemonRequest::Stats => {
            let guest = match vessel.agent_request(&AgentRequest::MemStats) {
                Ok(AgentResponse::MemStats(m)) => Some(m),
                _ => None,
            };
            let view = StatsView {
                guest,
                balloon_target_mib: balloon
                    .target_mib
                    .load(std::sync::atomic::Ordering::Relaxed),
                max_mib: balloon.max_mib,
                host_footprint_mib: crate::balloon::host_footprint_mib(),
            };
            respond(&mut writer, &DaemonResponse::Stats(view))
        }
        DaemonRequest::Balloon { target_mib } => {
            let resp = match vessel.balloon_set(target_mib) {
                Ok(()) => DaemonResponse::Ok,
                Err(e) => DaemonResponse::Error {
                    message: format!("{e:#}"),
                },
            };
            respond(&mut writer, &resp)
        }
        DaemonRequest::Shell { open } => match vessel.open_shell(&open) {
            Ok(vsock) => {
                respond(&mut writer, &DaemonResponse::ShellStarted)?;
                bridge(reader, writer, vsock)
            }
            Err(e) => respond(
                &mut writer,
                &DaemonResponse::Error {
                    message: format!("{e:#}"),
                },
            ),
        },
    }
}

/// Listen on a host unix socket and forward each connection to a guest vsock
/// port (which the agent forwards to the matching guest unix socket).
fn spawn_unix_proxy(vessel: Arc<Vessel>, sock_path: std::path::PathBuf, vsock_port: u32) {
    std::thread::spawn(move || {
        let _ = std::fs::remove_file(&sock_path);
        let listener = match UnixListener::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("proxy bind {} failed: {e}", sock_path.display());
                return;
            }
        };
        tracing::info!(sock = %sock_path.display(), port = vsock_port, "socket proxy ready");
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let vessel = vessel.clone();
            std::thread::spawn(move || match vessel.vsock_connect(vsock_port) {
                Ok(vsock) => {
                    let _ = bridge(BufReader::new(conn.try_clone().unwrap()), conn, vsock);
                }
                Err(e) => {
                    tracing::debug!("proxy vsock:{vsock_port} connect failed: {e:#}");
                }
            });
        }
    });
}

fn respond(writer: &mut UnixStream, resp: &DaemonResponse) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    Ok(())
}

/// Bidirectional byte pump between the CLI connection and the vsock stream.
fn bridge(
    mut client_r: BufReader<UnixStream>,
    mut client_w: UnixStream,
    vsock: UnixStream,
) -> anyhow::Result<()> {
    let mut vsock_w = vsock.try_clone()?;
    let mut vsock_r = vsock;

    let up = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match client_r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if vsock_w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = vsock_w.shutdown(std::net::Shutdown::Write);
    });

    let mut buf = [0u8; 8192];
    loop {
        match vsock_r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if client_w.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = client_w.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
    Ok(())
}
