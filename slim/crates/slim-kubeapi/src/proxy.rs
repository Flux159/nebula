//! Hook for serving pod subresources (log/exec) from whatever runs the
//! containers. slim-kubeapi stays engine-agnostic; slimd implements this
//! against its in-process engine, so `kubectl logs`/`kubectl exec` are served
//! by the apiserver itself — no kubelet, no separate proxy hop.

use std::io::Write;

pub struct LogOpts {
    pub follow: bool,
    pub tail: Option<usize>,
    pub timestamps: bool,
}

/// A started exec process; the apiserver's WebSocket layer bridges its
/// channels to these. tty mode uses `pty` for both directions.
pub struct ExecHandle {
    pub pid: i32,
    pub tty: bool,
    pub pty: Option<std::fs::File>,
    pub stdin: Option<std::fs::File>,
    pub stdout: Option<std::fs::File>,
    pub stderr: Option<std::fs::File>,
}

pub trait PodProxy: Send + Sync {
    /// Stream a pod container's logs to `w` (plain bytes; follow keeps streaming
    /// until the container exits). `container` selects a specific container
    /// (empty = the pod's first/holder). Err with NotFound if not running here.
    fn logs(
        &self,
        ns: &str,
        pod: &str,
        container: &str,
        opts: &LogOpts,
        w: &mut dyn Write,
    ) -> std::io::Result<()>;

    /// Start `cmd` in the pod's `container` (empty = first/holder). The apiserver
    /// wires WebSocket stdin/stdout/stderr/resize channels to the returned handle.
    fn exec_start(
        &self,
        ns: &str,
        pod: &str,
        container: &str,
        cmd: &[String],
        tty: bool,
        stdin: bool,
    ) -> std::io::Result<ExecHandle>;

    /// Block until the exec process exits; returns its exit code.
    fn exec_wait(&self, pid: i32) -> i32;

    /// Resize the exec pty (tty mode), given the pty fd from the handle.
    fn exec_resize(&self, pty_fd: i32, cols: u16, rows: u16);
}
