//! The VMM backend abstraction. Only what every backend actually shares is
//! abstracted here; backend-specific capabilities (e.g. Rosetta share, Venus GPU)
//! surface as spec fields that unsupported backends reject at `create` time.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::spec::VmSpec;

#[cfg(feature = "libkrun")]
pub mod krun;

/// Host-side stream for a guest vsock connection. Unix sockets on
/// macOS/Linux; on Windows the WHP backend will map guest vsock ports to
/// loopback TCP (what named-pipe-averse tooling expects anyway).
#[cfg(unix)]
pub type VsockStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type VsockStream = std::net::TcpStream;
#[cfg(all(target_os = "macos", feature = "vz"))]
pub mod vz;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmState {
    Created,
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

pub trait VmmBackend: Send + Sync {
    fn name(&self) -> &'static str;
    /// Cheap host capability probe (entitlements are only checkable by trying to start).
    fn is_available(&self) -> Result<()>;
    fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>>;
}

pub trait VmHandle: Send {
    fn name(&self) -> &str;
    fn state(&self) -> VmState;
    fn start(&mut self) -> Result<()>;
    /// Request guest stop. `force` skips graceful shutdown.
    fn stop(&mut self, force: bool) -> Result<()>;
    /// Resize the balloon: target is the amount of memory the GUEST should keep.
    fn balloon_set_guest_mib(&mut self, target_mib: u64) -> Result<()>;

    /// Open a host->guest vsock stream to `port`. Returns a connected socket.
    fn vsock_connect(&self, _port: u32) -> Result<VsockStream> {
        Err(Error::backend(
            "vm",
            "vsock_connect not supported by this backend",
        ))
    }

    /// Freeze all vCPUs (prerequisite for `save_state`).
    fn pause(&mut self) -> Result<()> {
        Err(Error::backend("vm", "pause not supported by this backend"))
    }

    /// Unfreeze a paused VM.
    fn resume(&mut self) -> Result<()> {
        Err(Error::backend("vm", "resume not supported by this backend"))
    }

    /// Save full machine state (RAM + devices) of a PAUSED vm to `path`.
    fn save_state(&mut self, _path: &std::path::Path) -> Result<()> {
        Err(Error::backend(
            "vm",
            "memory-state snapshots not supported by this backend",
        ))
    }

    /// Restore machine state into a freshly created (never started) VM.
    /// On success the VM is paused; call `resume` to continue execution.
    fn restore_state(&mut self, _path: &std::path::Path) -> Result<()> {
        Err(Error::backend(
            "vm",
            "memory-state restore not supported by this backend",
        ))
    }

    /// Block until the VM reaches `target` or `timeout` elapses.
    fn wait_for(&mut self, target: VmState, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            let s = self.state();
            if s == target {
                return Ok(());
            }
            if s == VmState::Failed && target != VmState::Failed {
                return Err(Error::backend("vm", "VM entered Failed state"));
            }
            if start.elapsed() >= timeout {
                return Err(Error::Timeout {
                    expected: target,
                    actual: s,
                    timeout_secs: timeout.as_secs(),
                });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Look up a backend implementation by name (CLI/daemon entry point).
pub fn backend_by_name(name: &str) -> Result<Box<dyn VmmBackend>> {
    match name {
        #[cfg(all(target_os = "macos", feature = "vz"))]
        "vz" => Ok(Box::new(vz::VzBackend::new())),
        #[cfg(feature = "libkrun")]
        "krun" => Ok(Box::new(krun::KrunBackend::new())),
        other => Err(Error::BackendUnavailable(
            "unknown",
            format!("no backend named `{other}` compiled into this build"),
        )),
    }
}
