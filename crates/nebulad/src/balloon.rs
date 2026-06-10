//! Balloon control loop: guest memory samples in, balloon targets out.
//! Policy lives in `nebula-balloon` (pure, unit-tested); this is just wiring.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nebula_balloon::{Action, Config, Controller, Sample};
use nebula_core::proto::{AgentRequest, AgentResponse};

use crate::vessel::Vessel;

/// Shared view of the current balloon target for the stats endpoint.
pub struct BalloonState {
    pub target_mib: AtomicU64,
    pub max_mib: u64,
}

pub fn start(vessel: Arc<Vessel>) -> Arc<BalloonState> {
    let max_mib = vessel.spec.mem_mib;
    let state = Arc::new(BalloonState {
        target_mib: AtomicU64::new(max_mib),
        max_mib,
    });

    // Opt-out for debugging/characterization.
    if std::env::var_os("NEBULA_NO_BALLOON").is_some() {
        tracing::warn!("balloon controller disabled (NEBULA_NO_BALLOON)");
        return state;
    }

    let state2 = state.clone();
    std::thread::spawn(move || {
        let mut ctrl = Controller::new(Config::for_max(max_mib));
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Ok(AgentResponse::MemStats(m)) = vessel.agent_request(&AgentRequest::MemStats)
            else {
                continue;
            };
            let sample = Sample {
                total_mib: m.total_kib / 1024,
                available_mib: m.available_kib / 1024,
                psi_some_avg10: m.psi_some_avg10,
            };
            if let Action::SetTarget(target) = ctrl.tick(sample) {
                match vessel.balloon_set(target) {
                    Ok(()) => {
                        state2.target_mib.store(target, Ordering::Relaxed);
                        tracing::info!(
                            target_mib = target,
                            balloon_mib = max_mib - target,
                            avail_mib = sample.available_mib,
                            "balloon target updated"
                        );
                    }
                    Err(e) => tracing::warn!("balloon set failed: {e:#}"),
                }
            }
        }
    });
    state
}

/// What macOS actually charges for the VM, in MiB. Virtualization.framework
/// runs guests in `com.apple.Virtualization.VirtualMachine` XPC processes, so
/// the honest number is the phys_footprint of those processes (plus ours).
#[cfg(target_os = "macos")]
pub fn host_footprint_mib() -> u64 {
    footprint_of(std::process::id() as i32) + vm_xpc_footprint_mib()
}

/// On Linux the krun workers ARE the VMs: our RSS plus every child
/// krun-worker's RSS (from /proc) is the whole story.
#[cfg(target_os = "linux")]
pub fn host_footprint_mib() -> u64 {
    fn rss_mib(pid: u32) -> u64 {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|s| {
                s.lines().find_map(|l| {
                    l.strip_prefix("VmRSS:")
                        .and_then(|v| v.trim().trim_end_matches(" kB").parse::<u64>().ok())
                })
            })
            .unwrap_or(0)
            / 1024
    }
    let me = std::process::id();
    let mut total = rss_mib(me);
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let cmdline =
                std::fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if cmdline.contains("krun-worker") {
                total += rss_mib(pid);
            }
        }
    }
    total
}

#[cfg(target_os = "macos")]
fn vm_xpc_footprint_mib() -> u64 {
    let mut pids = vec![0i32; 4096];
    let bytes = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr() as *mut libc::c_void,
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    if bytes <= 0 {
        return 0;
    }
    let count = bytes as usize;
    pids.truncate(count);
    let mut total = 0;
    for pid in pids {
        if pid <= 0 {
            continue;
        }
        // proc_name truncates to 32 chars; proc_pidpath gives the full path.
        let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let n = unsafe {
            libc::proc_pidpath(
                pid,
                path.as_mut_ptr() as *mut libc::c_void,
                path.len() as u32,
            )
        };
        if n > 0 {
            let path = String::from_utf8_lossy(&path[..n as usize]);
            if path.contains("Virtualization.VirtualMachine") {
                total += footprint_of(pid);
            }
        }
    }
    total
}

#[cfg(target_os = "macos")]
fn footprint_of(pid: i32) -> u64 {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            &mut info as *mut _ as *mut libc::rusage_info_t,
        )
    };
    if rc == 0 {
        info.ri_phys_footprint / (1024 * 1024)
    } else {
        0
    }
}
