//! Exec session lifecycle (impl on Engine).

use crate::container::ExecSession;
use crate::engine::Engine;
use slim_api::exec::*;
use slim_http::Ctx;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

impl Engine {
    pub fn exec_create(&self, container: &str, cfg: ExecConfig) -> io::Result<String> {
        let entry = self.get_entry(container)?;
        let c = entry.snapshot();
        if !c.running() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Container {} is not running", c.short_id()),
            ));
        }
        let id = slim_net::rand_id();
        let session = Arc::new(ExecSession {
            id: id.clone(),
            container_id: c.id.clone(),
            config: cfg,
            pid: Mutex::new(0),
            exit_code: Mutex::new(None),
            running: Mutex::new(false),
        });
        self.execs.lock().unwrap().insert(id.clone(), session);
        Ok(id)
    }

    pub fn exec_start(self: &Arc<Self>, exec_id: &str, start: &ExecStartConfig, ctx: &mut Ctx) -> io::Result<()> {
        let session = self
            .execs
            .lock()
            .unwrap()
            .get(exec_id)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("No such exec instance: {exec_id}")))?;
        let entry = self.get_entry(&session.container_id)?;
        let target_pid = {
            let c = entry.c.lock().unwrap();
            if !c.running() {
                return Err(io::Error::other("container is not running"));
            }
            c.state.pid
        };

        let cfg = &session.config;
        let spec = slim_runtime::ExecSpec {
            argv: cfg.cmd.clone(),
            env: cfg.env.clone(),
            cwd: cfg.working_dir.clone(),
            user: cfg.user.clone(),
            tty: cfg.tty,
            open_stdin: cfg.attach_stdin,
        };
        let handle = slim_runtime::exec_in_container_cg(target_pid, &spec, Some(&session.container_id))
            .map_err(|e| io::Error::other(format!("exec failed: {e}")))?;
        *session.pid.lock().unwrap() = handle.pid;
        *session.running.lock().unwrap() = true;

        if start.detach {
            // Reap in the background.
            let session2 = session.clone();
            std::thread::spawn(move || {
                let st = slim_runtime::wait_pid(session2.pid()).unwrap_or_default();
                *session2.exit_code.lock().unwrap() = Some(st.code);
                *session2.running.lock().unwrap() = false;
            });
            return ctx.respond_empty(200);
        }

        // Attached: hijack and pump.
        let tty = cfg.tty;
        let (sock, _buf) = ctx.hijack(!tty && cfg.attach_stdout && cfg.attach_stderr)?;
        pump_exec(handle, sock, tty, cfg.attach_stdin);

        // Reap.
        let st = slim_runtime::wait_pid(session.pid()).unwrap_or_default();
        *session.exit_code.lock().unwrap() = Some(st.code);
        *session.running.lock().unwrap() = false;
        Ok(())
    }

    pub fn exec_inspect(&self, exec_id: &str) -> io::Result<ExecInspect> {
        let session = self
            .execs
            .lock()
            .unwrap()
            .get(exec_id)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("No such exec instance: {exec_id}")))?;
        let running = *session.running.lock().unwrap();
        let exit_code = session.exit_code.lock().unwrap().map(|c| c as i64);
        let pid = *session.pid.lock().unwrap() as i64;
        Ok(ExecInspect {
            id: session.id.clone(),
            running,
            exit_code,
            container_id: session.container_id.clone(),
            pid,
        })
    }

    pub fn exec_resize(&self, exec_id: &str, w: u16, h: u16) {
        // pty master fd for the exec isn't retained past start in this simple
        // model; resize is best-effort and currently a no-op for exec (the
        // interactive run path handles its own resize). Kept for API shape.
        let _ = (exec_id, w, h);
    }
}

impl ExecSession {
    fn pid(&self) -> i32 {
        *self.pid.lock().unwrap()
    }
}

/// Bidirectional copy between the hijacked socket and an exec process.
fn pump_exec(handle: slim_runtime::Handle, sock: std::os::unix::net::UnixStream, tty: bool, stdin: bool) {
    let mut joins = Vec::new();

    if tty {
        if let Some(pty) = handle.pty_master {
            // socket -> pty
            if stdin {
                if let Ok(mut pty_in) = pty.try_clone() {
                    let mut sin = sock.try_clone().ok();
                    joins.push(std::thread::spawn(move || {
                        if let Some(s) = sin.as_mut() {
                            let mut buf = [0u8; 8192];
                            while let Ok(n) = s.read(&mut buf) {
                                if n == 0 || pty_in.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        }
                    }));
                }
            }
            // pty -> socket
            let mut pty_out = pty;
            let mut sout = sock;
            joins.push(std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = pty_out.read(&mut buf) {
                    if n == 0 || sout.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = sout.flush();
                }
            }));
        }
    } else {
        let multiplexed = true;
        if stdin {
            if let Some(mut child_in) = handle.stdin {
                let mut sin = sock.try_clone().ok();
                joins.push(std::thread::spawn(move || {
                    if let Some(s) = sin.as_mut() {
                        let mut buf = [0u8; 8192];
                        while let Ok(n) = s.read(&mut buf) {
                            if n == 0 || child_in.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }));
            }
        }
        for (stream, file) in [(1u8, handle.stdout), (2u8, handle.stderr)] {
            let Some(mut file) = file else { continue };
            let mut out = sock.try_clone().ok();
            joins.push(std::thread::spawn(move || {
                if let Some(s) = out.as_mut() {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = file.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let framed = if multiplexed {
                            crate::streams::frame(stream, &buf[..n])
                        } else {
                            buf[..n].to_vec()
                        };
                        if s.write_all(&framed).is_err() {
                            break;
                        }
                        let _ = s.flush();
                    }
                }
            }));
        }
    }
    for j in joins {
        let _ = j.join();
    }
}
