//! Break detection shared by the scale scenarios. The first tripped condition
//! is the recorded failure mode — "what actually broke" is the deliverable.

use crate::hostmem;
use crate::nebula::Nebula;

/// Stop sweeps before wedging the host. 4 GiB floor per the plan.
pub const HOST_FREE_FLOOR_MIB: u64 = 4096;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Brk {
    /// Engine-level command failure (create/start refused or errored).
    CmdError { detail: String },
    /// A call exceeded its hard timeout.
    Timeout { detail: String },
    /// Guest OOM-killer fired during the phase.
    GuestOom { new_kills: u64 },
    /// Status API or guest exec stopped answering.
    AgentUnhealthy { detail: String },
    /// Start latency blew past 10x the early-batch median.
    LatencyBlowup { median_ms: f64, baseline_ms: f64 },
    /// Host reclaimable RAM under the safety floor.
    HostLow { free_mib: u64 },
    /// Hit the configured ceiling without breaking (a result, not a failure).
    MaxN { n: usize },
}

impl Brk {
    pub fn label(&self) -> String {
        match self {
            Brk::CmdError { detail } => format!("cmd_error: {detail}"),
            Brk::Timeout { detail } => format!("timeout: {detail}"),
            Brk::GuestOom { new_kills } => format!("guest_oom: {new_kills} new kills"),
            Brk::AgentUnhealthy { detail } => format!("agent_unhealthy: {detail}"),
            Brk::LatencyBlowup {
                median_ms,
                baseline_ms,
            } => format!("latency_blowup: {median_ms:.0}ms vs baseline {baseline_ms:.0}ms"),
            Brk::HostLow { free_mib } => format!("host_low: {free_mib} MiB free"),
            Brk::MaxN { n } => format!("max_n: reached configured cap {n}"),
        }
    }
}

/// Checks that apply between batches in every scale scenario, cheapest first.
pub fn engine_checks(neb: &Nebula, oom_baseline: u64) -> Option<Brk> {
    if let Some(free) = hostmem::host_free_mib() {
        if free < HOST_FREE_FLOOR_MIB {
            return Some(Brk::HostLow { free_mib: free });
        }
    }
    let oom_now = neb.oom_count();
    if oom_now > oom_baseline {
        return Some(Brk::GuestOom {
            new_kills: oom_now - oom_baseline,
        });
    }
    if !neb.healthy() {
        return Some(Brk::AgentUnhealthy {
            detail: "status API or guest exec failed".into(),
        });
    }
    None
}
