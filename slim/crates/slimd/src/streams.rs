//! Attach/exec stream plumbing: docker stdcopy multiplexing for non-tty,
//! raw passthrough for tty, and the bidirectional socket<->process copy.

use crate::container::{Runtime, STREAM_STDERR, STREAM_STDOUT};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

/// 8-byte stdcopy header: [stream, 0,0,0, len u32 BE].
pub fn frame(stream: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.push(stream);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Pump live container output to the attach socket until EOF, then return.
/// `multiplexed` = wrap in stdcopy frames (non-tty); else raw.
pub fn pump_output_to_socket(
    rt: &Arc<Runtime>,
    mut sock: UnixStream,
    multiplexed: bool,
    want_stdout: bool,
    want_stderr: bool,
) {
    let rx = {
        let (tx, rx) = std::sync::mpsc::channel();
        rt.subscribers.lock().unwrap().push(tx);
        rx
    };
    // Also push any backlog? Live attach only — logs endpoint covers history.
    while let Ok((stream, chunk)) = rx.recv() {
        let want = (stream == STREAM_STDOUT && want_stdout)
            || (stream == STREAM_STDERR && want_stderr);
        if !want {
            continue;
        }
        let bytes = if multiplexed { frame(stream, &chunk) } else { chunk };
        if sock.write_all(&bytes).is_err() {
            break;
        }
        let _ = sock.flush();
    }
}

/// Copy the attach socket's input into the process stdin (pty master or pipe).
pub fn pump_socket_to_stdin(rt: &Arc<Runtime>, mut sock: UnixStream) {
    let mut buf = [0u8; 8192];
    loop {
        match sock.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let wrote = if let Some(pty) = &rt.pty {
                    pty.lock().unwrap().write_all(&buf[..n]).is_ok()
                } else if let Some(stdin) = &rt.stdin {
                    stdin.lock().unwrap().write_all(&buf[..n]).is_ok()
                } else {
                    false
                };
                if !wrote {
                    break;
                }
            }
        }
    }
    // NOTE: pipe-mode stdin EOF (so `echo x | docker run -i` exits) needs the
    // child's stdin write-end closed here. Doing that safely without a
    // double-close requires owning the File rather than a shared lock; the
    // attach/exec callers that need EOF hand us an owned stdin via a dedicated
    // path (S6 interactive). For now the process sees EOF when slimd exits or
    // the container is stopped.
}
