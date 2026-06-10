//! slim-runtime: the minimal container runtime under slimd.
//!
//! Design constraints (see slim/BRIEF.md): the microVM is the security
//! boundary, so this is namespaces + cgroup2 + pivot_root with NO seccomp,
//! NO userns, NO apparmor — bounded blast radius, not in-guest hardening.
//! ~1.5k lines instead of vendoring a runc.
//!
//! Process model: slimd sets PR_SET_CHILD_SUBREAPER. Container start is a
//! double fork — the intermediate unshares namespaces (it must, because a
//! multithreaded process cannot setns/unshare mount namespaces) and the
//! grandchild becomes PID 1 of the new pid namespace, reparented to slimd
//! when the intermediate exits. One waiter thread per container/exec reaps
//! it and reports the exit code.

pub mod jsonlog;
pub mod spec;

pub use spec::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

// On macOS the crate compiles (so slimd's API layer is host-testable) but
// every runtime entry point fails cleanly.
#[cfg(not(target_os = "linux"))]
mod stub {
    use super::spec::*;
    use std::io;

    pub fn become_subreaper() {}

    pub fn start_container(_spec: &ContainerSpec) -> io::Result<Handle> {
        Err(unsupported())
    }

    pub fn exec_in_container(_pid: i32, _spec: &ExecSpec) -> io::Result<Handle> {
        Err(unsupported())
    }

    pub fn exec_in_container_cg(
        _pid: i32,
        _spec: &ExecSpec,
        _cgroup_id: Option<&str>,
    ) -> io::Result<Handle> {
        Err(unsupported())
    }

    pub fn signal_pid(_pid: i32, _signal: i32) -> io::Result<()> {
        Err(unsupported())
    }

    pub fn read_oom(_id: &str) -> bool {
        false
    }

    pub fn parse_signal(_s: &str) -> i32 {
        15 // SIGTERM
    }

    pub fn kill_cgroup(_id: &str) -> io::Result<()> {
        Err(unsupported())
    }

    pub fn remove_cgroup(_id: &str) {}

    pub fn wait_pid(_pid: i32) -> io::Result<ExitStatus> {
        Err(unsupported())
    }

    pub fn read_stats(_id: &str, _pid: i32) -> CgroupStats {
        CgroupStats::default()
    }

    pub fn resize_pty(_fd: std::os::unix::io::RawFd, _w: u16, _h: u16) {}

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "slim-runtime: containers require linux (this is the macOS host build)",
        )
    }
}
#[cfg(not(target_os = "linux"))]
pub use stub::*;
