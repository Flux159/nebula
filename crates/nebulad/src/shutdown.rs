//! One exit path, and a reason on every one of them.
//!
//! Ten restarts in an afternoon used to look identical in the log: a gap and
//! another `nebulad starting`. A clean `nebula down`, a SIGHUP from a closing
//! terminal, and a vessel that died were indistinguishable, so debugging
//! started from zero every time (issue #23). Every way nebulad can stop now
//! funnels through [`finish`], which logs *why*, how long it had been up and
//! what it was running, stamps the same into `run/instance.json`, and only
//! then exits.
//!
//! Signals are caught rather than left to the default disposition — the point
//! is the log line, and a killed process cannot write one. The uncatchable
//! ones (SIGKILL, a panic in the host) are covered from the other side: they
//! leave no exit record, and the next start reports that.

#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::instance::{self, ExitRecord};
use crate::net::NetState;
use crate::paths::Paths;
use crate::vessel::Vessel;

/// Why nebulad is stopping. The string form is stable and greppable — the
/// first thing anyone does with a daemon log is grep it.
#[derive(Debug, Clone)]
pub enum Reason {
    /// `nebula down` over the control socket.
    Down { force: bool },
    /// Unix only: Windows has no POSIX signals, and nebulad is stopped there
    /// through the control socket.
    #[cfg(unix)]
    Signal(i32),
    /// The watchdog saw the engine VM stop on its own.
    VesselDied(String),
    /// Failed before it ever served (bad config, port conflict, no images).
    StartupError(String),
    /// Failed while serving.
    Fatal(String),
    /// The control-socket listener ended without a `down`.
    ListenerClosed,
}

impl Reason {
    pub fn label(&self) -> &'static str {
        match self {
            Reason::Down { .. } => "down",
            #[cfg(unix)]
            Reason::Signal(_) => "signal",
            Reason::VesselDied(_) => "vessel-died",
            Reason::StartupError(_) => "startup-error",
            Reason::Fatal(_) => "fatal",
            Reason::ListenerClosed => "listener-closed",
        }
    }

    pub fn detail(&self) -> Option<String> {
        match self {
            Reason::Down { force } => Some(format!("force={force}")),
            #[cfg(unix)]
            Reason::Signal(sig) => Some(signal_name(*sig).to_string()),
            Reason::VesselDied(state) => Some(format!("vm state {state}")),
            // First line only: these can be several paragraphs of guidance
            // (a port conflict names every port and the fix), and the full
            // text is already in the ERROR line above and startup-error.txt.
            Reason::StartupError(e) | Reason::Fatal(e) => {
                Some(e.lines().next().unwrap_or(e).to_string())
            }
            Reason::ListenerClosed => None,
        }
    }

    fn exit_code(&self) -> i32 {
        match self {
            // EX_SOFTWARE: launchd (and `nebula up`) restart us.
            Reason::VesselDied(_) => 70,
            Reason::StartupError(_) | Reason::Fatal(_) => 1,
            _ => 0,
        }
    }
}

#[cfg(unix)]
pub fn signal_name(sig: i32) -> &'static str {
    match sig {
        x if x == libc::SIGTERM => "SIGTERM",
        x if x == libc::SIGINT => "SIGINT",
        x if x == libc::SIGHUP => "SIGHUP",
        _ => "signal",
    }
}

struct Ctx {
    paths: Paths,
    started: Instant,
    vessel: Mutex<Option<Arc<Vessel>>>,
    net: Mutex<Option<Arc<Mutex<NetState>>>>,
}

static CTX: OnceLock<Ctx> = OnceLock::new();
static FINISHING: AtomicBool = AtomicBool::new(false);
static SERVING: AtomicBool = AtomicBool::new(false);

/// Called once the control socket is listening. Separates "never came up"
/// (which `nebula up` should report to the user's face) from "died while
/// serving".
pub fn mark_serving() {
    SERVING.store(true, Ordering::SeqCst);
}

pub fn is_serving() -> bool {
    SERVING.load(Ordering::SeqCst)
}

/// Call once, as early as the paths are known — before anything that can fail
/// with a reason worth recording.
pub fn init(paths: &Paths) {
    let _ = CTX.set(Ctx {
        paths: Paths {
            root: paths.root.clone(),
        },
        started: Instant::now(),
        vessel: Mutex::new(None),
        net: Mutex::new(None),
    });
}

/// Hand the shutdown path the things it reports on. Both are optional: an
/// exit before the vessel boots still logs a reason, just without counts.
pub fn attach_vessel(vessel: Arc<Vessel>) {
    if let Some(ctx) = CTX.get() {
        *ctx.vessel.lock().unwrap() = Some(vessel);
    }
}

pub fn attach_net(net: Arc<Mutex<NetState>>) {
    if let Some(ctx) = CTX.get() {
        *ctx.net.lock().unwrap() = Some(net);
    }
}

/// Log the reason, stop the vessel, clean up, exit. Never returns.
pub fn finish(reason: Reason) -> ! {
    let Some(ctx) = CTX.get() else {
        // init() was never reached; still say something before going.
        tracing::info!(reason = reason.label(), "nebulad exiting");
        std::process::exit(reason.exit_code());
    };

    // A `down` racing a signal must not interleave two shutdowns.
    if FINISHING.swap(true, Ordering::SeqCst) {
        tracing::debug!(reason = reason.label(), "shutdown already in progress");
        // Let the first caller finish; if it wedges, leave anyway rather than
        // hanging forever holding the ports.
        std::thread::sleep(std::time::Duration::from_secs(20));
        std::process::exit(reason.exit_code());
    }

    let uptime = ctx.started.elapsed().as_secs();
    let containers = ctx
        .net
        .lock()
        .unwrap()
        .as_ref()
        .map(|n| n.lock().unwrap().names.len());
    // Local filesystem only (spec + pid files); safe on a shutdown path.
    let vessels_running = nebula_core::vessels::list()
        .ok()
        .map(|v| v.iter().filter(|s| s.running).count());

    tracing::info!(
        reason = reason.label(),
        detail = reason.detail().unwrap_or_default(),
        uptime_secs = uptime,
        containers = containers.unwrap_or(0),
        vessels_running = vessels_running.unwrap_or(0),
        failing_ports = crate::ports::failing(),
        "nebulad shutting down"
    );

    let vessel = ctx.vessel.lock().unwrap().clone();
    if let Some(vessel) = vessel {
        let state = vessel.state();
        if !matches!(state, nebula_core::backend::VmState::Stopped) {
            let force = matches!(reason, Reason::Down { force: true });
            if let Err(e) = vessel.stop(force) {
                tracing::error!("stopping vessel: {e:#}");
            }
        }
    }

    instance::record_exit(
        &ctx.paths,
        ExitRecord {
            reason: reason.label().to_string(),
            detail: reason.detail(),
            at: instance::now_unix(),
            uptime_secs: uptime,
            containers,
            vessels_running,
        },
    );
    let _ = std::fs::remove_file(ctx.paths.pid_file());
    let _ = std::fs::remove_file(ctx.paths.control_sock());

    tracing::info!(reason = reason.label(), "nebulad stopped");
    std::process::exit(reason.exit_code());
}

// --- signals ------------------------------------------------------------------

/// Self-pipe write end. A signal handler may only call async-signal-safe
/// functions, which rules out tracing, allocation and locks — `write(2)` is on
/// the list, so the handler does nothing but hand the number to a thread.
#[cfg(unix)]
static SIGNAL_PIPE_W: AtomicI32 = AtomicI32::new(-1);

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    let fd = SIGNAL_PIPE_W.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = [sig as u8];
        unsafe {
            libc::write(fd, byte.as_ptr() as *const libc::c_void, 1);
        }
    }
}

/// Catch SIGTERM/SIGINT/SIGHUP and turn them into a logged shutdown.
///
/// SIGHUP matters more than it looks: `nebula up` spawns the daemon without a
/// session of its own, so closing the terminal that started it delivers one —
/// which was, until now, an unexplained restart.
#[cfg(unix)]
pub fn install_signal_handlers() {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        tracing::warn!("signal pipe unavailable; signals will not be logged");
        return;
    }
    // Never leak the pipe into vz-worker/krun-worker children.
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    SIGNAL_PIPE_W.store(write_fd, Ordering::SeqCst);

    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe {
            let mut act: libc::sigaction = std::mem::zeroed();
            act.sa_sigaction = on_signal as *const () as usize;
            act.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut act.sa_mask);
            libc::sigaction(sig, &act, std::ptr::null_mut());
        }
    }

    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || loop {
            let mut buf = [0u8; 1];
            let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
            if n == 1 {
                finish(Reason::Signal(buf[0] as i32));
            }
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                tracing::warn!("signal thread: {err}");
                return;
            }
        })
        .ok();
}

#[cfg(not(unix))]
pub fn install_signal_handlers() {
    // Windows: nebulad runs under krun and is stopped through the control
    // socket; console control handlers can follow if that changes.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_stable() {
        assert_eq!(Reason::Down { force: false }.label(), "down");
        #[cfg(unix)]
        assert_eq!(Reason::Signal(libc::SIGTERM).label(), "signal");
        assert_eq!(Reason::VesselDied("Failed".into()).label(), "vessel-died");
        assert_eq!(Reason::ListenerClosed.label(), "listener-closed");
    }

    #[test]
    fn details_name_the_cause() {
        #[cfg(unix)]
        assert_eq!(
            Reason::Signal(libc::SIGHUP).detail().as_deref(),
            Some("SIGHUP")
        );
        assert_eq!(
            Reason::Down { force: true }.detail().as_deref(),
            Some("force=true")
        );
        assert_eq!(
            Reason::VesselDied("Failed".into()).detail().as_deref(),
            Some("vm state Failed")
        );
        // A multi-paragraph startup error is summarised, not pasted into a
        // log field.
        assert_eq!(
            Reason::StartupError("port 7440 in use\n\nfix it by...".into())
                .detail()
                .as_deref(),
            Some("port 7440 in use")
        );
    }

    #[test]
    fn only_failures_exit_nonzero() {
        assert_eq!(Reason::Down { force: false }.exit_code(), 0);
        #[cfg(unix)]
        assert_eq!(Reason::Signal(libc::SIGTERM).exit_code(), 0);
        assert_eq!(Reason::VesselDied("Failed".into()).exit_code(), 70);
        assert_eq!(Reason::Fatal("boom".into()).exit_code(), 1);
        assert_eq!(Reason::StartupError("boom".into()).exit_code(), 1);
    }
}
