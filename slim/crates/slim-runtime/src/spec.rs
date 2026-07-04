//! Runtime input/output types (platform-independent).

use std::fs::File;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct BindMount {
    pub source: PathBuf,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ContainerSpec {
    pub id: String,
    /// Prepared rootfs (overlay merged dir). The runtime pivots into it.
    pub rootfs: PathBuf,
    pub argv: Vec<String>,
    pub env: Vec<String>, // KEY=VALUE
    pub cwd: String,
    /// "", "uid", "uid:gid", "name", "name:group" — names resolved against
    /// the container's /etc/passwd//etc/group.
    pub user: String,
    pub hostname: String,
    pub tty: bool,
    pub open_stdin: bool,
    pub binds: Vec<BindMount>,
    pub tmpfs: Vec<(String, String)>, // target, options
    /// Path to a network namespace to join (bind-mounted by slim-net).
    /// None → fresh isolated netns with only loopback.
    pub netns: Option<PathBuf>,
    pub readonly_rootfs: bool,
    pub shm_size: i64,

    // cgroup2 knobs; 0 = unlimited.
    pub memory: i64,
    pub memory_swap: i64,
    pub nano_cpus: i64,
    pub cpu_shares: i64,
    pub pids_limit: i64,

    /// Keep the entrypoint held at the exec gate until Handle::release_exec().
    /// slimd wires the container network (veth/IP/routes, /etc/hosts,
    /// resolv.conf, published ports) between fork and exec, so the first
    /// instruction never races setup — docker parity: dockerd configures the
    /// sandbox netns before the process runs.
    pub hold: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ExecSpec {
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub user: String,
    pub tty: bool,
    pub open_stdin: bool,
}

/// A started container or exec process, as seen from slimd.
#[derive(Debug)]
pub struct Handle {
    pub pid: i32,
    /// TTY mode: the pty master (read+write). Pipe mode: None.
    pub pty_master: Option<File>,
    /// Pipe mode: child's stdin write end (None when !open_stdin).
    pub stdin: Option<File>,
    /// Pipe mode: child's stdout / stderr read ends.
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    /// hold mode: write end of the exec gate (release_exec signals it).
    pub gate: Option<File>,
    /// hold mode: the runtime error pipe, read after release (EOF = exec'd).
    pub err_pipe: Option<File>,
}

impl Handle {
    /// Open the exec gate (no-op when the spec didn't hold) and collect the
    /// runtime's verdict: Ok = the entrypoint exec'd, Err = it could not
    /// (bad argv, missing interpreter, …). Dropping an unreleased Handle
    /// closes the gate instead, and the held child exits without ever
    /// running the workload.
    pub fn release_exec(&mut self) -> std::io::Result<()> {
        use std::io::{Read, Write};
        if let Some(mut gate) = self.gate.take() {
            let _ = gate.write_all(&[1u8]);
        }
        if let Some(mut err) = self.err_pipe.take() {
            let mut msg = String::new();
            let _ = err.read_to_string(&mut msg);
            if !msg.is_empty() {
                return Err(std::io::Error::other(msg));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExitStatus {
    pub code: i32,
    pub oom_killed: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CgroupStats {
    pub memory_current: u64,
    pub memory_limit: u64,
    pub cpu_usage_usec: u64,
    pub pids_current: u64,
}

pub const CGROUP_ROOT: &str = "/sys/fs/cgroup/slim";
