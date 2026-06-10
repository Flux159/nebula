//! Nebula vessel-agent: runs inside the guest, owned by nebula-init (PID 1).
//!
//! Serves the v0 JSON-lines protocol over vsock (see nebula_core::proto):
//! - control port: health, memory stats, exec, shutdown
//! - shell port: interactive pty sessions

#[cfg(target_os = "linux")]
mod agent {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::time::{Duration, Instant};

    use nebula_core::proto::*;

    const OUTPUT_CAP: usize = 64 * 1024;

    /// Minimal AF_VSOCK listener.
    pub struct VsockListener {
        fd: OwnedFd,
    }

    impl VsockListener {
        pub fn bind(port: u32) -> std::io::Result<Self> {
            unsafe {
                let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let fd = OwnedFd::from_raw_fd(fd);
                let mut addr: libc::sockaddr_vm = std::mem::zeroed();
                addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
                addr.svm_cid = libc::VMADDR_CID_ANY;
                addr.svm_port = port;
                let rc = libc::bind(
                    fd.as_raw_fd(),
                    &addr as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
                );
                if rc < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::listen(fd.as_raw_fd(), 16) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(Self { fd })
            }
        }

        pub fn accept(&self) -> std::io::Result<std::fs::File> {
            unsafe {
                let fd = libc::accept(
                    self.fd.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(std::fs::File::from_raw_fd(fd))
            }
        }
    }

    pub fn run() -> ! {
        eprintln!("vessel-agent {} starting", env!("CARGO_PKG_VERSION"));
        std::thread::spawn(|| serve_loop("shell", VSOCK_PORT_SHELL, handle_shell));
        std::thread::spawn(|| serve_loop("docker-proxy", VSOCK_PORT_DOCKER, handle_docker_proxy));
        std::thread::spawn(|| {
            serve_loop(
                "containerd-proxy",
                VSOCK_PORT_CONTAINERD,
                handle_containerd_proxy,
            )
        });
        serve_loop("control", VSOCK_PORT_CONTROL, handle_control)
    }

    fn handle_docker_proxy(conn: std::fs::File) {
        forward_to_unix(conn, "/var/run/docker.sock");
    }

    fn handle_containerd_proxy(conn: std::fs::File) {
        forward_to_unix(conn, "/run/containerd/containerd.sock");
    }

    /// Pump a vsock connection into a local unix socket (and back).
    fn forward_to_unix(conn: std::fs::File, sock: &str) {
        let Ok(unix) = std::os::unix::net::UnixStream::connect(sock) else {
            return;
        };
        let mut conn_r = conn.try_clone().expect("clone conn");
        let mut conn_w = conn;
        let mut unix_r = unix.try_clone().expect("clone unix");
        let mut unix_w = unix;

        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                match conn_r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if unix_w.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = unix_w.shutdown(std::net::Shutdown::Write);
        });
        let mut buf = [0u8; 65536];
        loop {
            match unix_r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if conn_w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        unsafe { libc::shutdown(conn_w.as_raw_fd(), libc::SHUT_RDWR) };
        let _ = t.join();
    }

    fn serve_loop(name: &'static str, port: u32, handler: fn(std::fs::File)) -> ! {
        loop {
            match VsockListener::bind(port) {
                Ok(listener) => {
                    eprintln!("vessel-agent: {name} listening on vsock:{port}");
                    loop {
                        match listener.accept() {
                            Ok(conn) => {
                                std::thread::spawn(move || handler(conn));
                            }
                            Err(e) => {
                                eprintln!("vessel-agent: {name} accept error: {e}");
                                std::thread::sleep(Duration::from_millis(200));
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("vessel-agent: bind vsock:{port} failed: {e}; retrying");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    fn handle_control(conn: std::fs::File) {
        let mut reader = BufReader::new(conn.try_clone().expect("clone conn"));
        let mut writer = conn;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {}
                Err(_) => return,
            }
            let resp = match serde_json::from_str::<AgentRequest>(line.trim()) {
                Ok(req) => dispatch(req),
                Err(e) => AgentResponse::Error {
                    message: format!("bad request: {e}"),
                },
            };
            let mut out = serde_json::to_string(&resp).expect("serialize response");
            out.push('\n');
            if writer.write_all(out.as_bytes()).is_err() {
                return;
            }
        }
    }

    fn dispatch(req: AgentRequest) -> AgentResponse {
        match req {
            AgentRequest::Health => AgentResponse::Health(health()),
            AgentRequest::MemStats => match mem_stats() {
                Ok(m) => AgentResponse::MemStats(m),
                Err(e) => AgentResponse::Error {
                    message: e.to_string(),
                },
            },
            AgentRequest::Exec {
                cmd,
                args,
                env,
                timeout_ms,
            } => match exec(&cmd, &args, &env, Duration::from_millis(timeout_ms)) {
                Ok(r) => AgentResponse::Exec(r),
                Err(e) => AgentResponse::Error {
                    message: e.to_string(),
                },
            },
            AgentRequest::Shutdown => {
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(150));
                    unsafe {
                        libc::sync();
                        libc::reboot(libc::RB_POWER_OFF);
                    }
                });
                AgentResponse::Ok
            }
        }
    }

    fn health() -> Health {
        let mut uts = unsafe { std::mem::zeroed::<libc::utsname>() };
        unsafe { libc::uname(&mut uts) };
        let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let uptime = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next().map(str::to_string))
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        Health {
            proto_version: PROTO_VERSION,
            agent_version: env!("CARGO_PKG_VERSION").into(),
            kernel: release,
            uptime_secs: uptime as u64,
        }
    }

    fn mem_stats() -> std::io::Result<MemStats> {
        let meminfo = std::fs::read_to_string("/proc/meminfo")?;
        let field = |name: &str| -> u64 {
            meminfo
                .lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };
        let psi = std::fs::read_to_string("/proc/pressure/memory").ok();
        let psi_avg10 = |kind: &str| -> Option<f64> {
            psi.as_ref()?
                .lines()
                .find(|l| l.starts_with(kind))?
                .split_whitespace()
                .find_map(|tok| tok.strip_prefix("avg10="))
                .and_then(|v| v.parse().ok())
        };
        Ok(MemStats {
            total_kib: field("MemTotal:"),
            free_kib: field("MemFree:"),
            available_kib: field("MemAvailable:"),
            cached_kib: field("Cached:"),
            psi_some_avg10: psi_avg10("some"),
            psi_full_avg10: psi_avg10("full"),
        })
    }

    fn exec(
        cmd: &str,
        args: &[String],
        env: &[(String, String)],
        timeout: Duration,
    ) -> std::io::Result<ExecResult> {
        let mut child = std::process::Command::new(cmd)
            .args(args)
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("HOME", "/root")
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let start = Instant::now();
        let mut timed_out = false;
        loop {
            match child.try_wait()? {
                Some(_) => break,
                None if start.elapsed() >= timeout => {
                    timed_out = true;
                    let _ = child.kill();
                    break;
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let out = child.wait_with_output()?;
        let cap = |mut v: Vec<u8>| {
            v.truncate(OUTPUT_CAP);
            String::from_utf8_lossy(&v).into_owned()
        };
        Ok(ExecResult {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: cap(out.stdout),
            stderr: cap(out.stderr),
            timed_out,
        })
    }

    /// Shell stream: first line is a ShellOpen JSON, then raw pty bytes both ways.
    fn handle_shell(conn: std::fs::File) {
        let mut reader = BufReader::new(conn.try_clone().expect("clone conn"));
        let mut first_line = String::new();
        if reader.read_line(&mut first_line).is_err() {
            return;
        }
        let open: ShellOpen = match serde_json::from_str(first_line.trim()) {
            Ok(o) => o,
            Err(_) => return,
        };

        // Allocate pty.
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let mut winsize = libc::winsize {
            ws_row: open.rows,
            ws_col: open.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut winsize,
            )
        };
        if rc != 0 {
            return;
        }

        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // Child: become session leader on the pty slave and exec the shell.
            unsafe {
                libc::close(master);
                libc::setsid();
                libc::ioctl(slave, libc::TIOCSCTTY, 0);
                libc::dup2(slave, 0);
                libc::dup2(slave, 1);
                libc::dup2(slave, 2);
                if slave > 2 {
                    libc::close(slave);
                }
                let cmd = std::ffi::CString::new(open.cmd.as_str()).unwrap();
                let mut argv: Vec<std::ffi::CString> = vec![cmd.clone()];
                argv.extend(
                    open.args
                        .iter()
                        .map(|a| std::ffi::CString::new(a.as_str()).unwrap()),
                );
                let mut argv_ptrs: Vec<*const libc::c_char> =
                    argv.iter().map(|c| c.as_ptr()).collect();
                argv_ptrs.push(std::ptr::null());
                let term = std::ffi::CString::new("TERM=xterm-256color").unwrap();
                let path = std::ffi::CString::new(
                    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                )
                .unwrap();
                let home = std::ffi::CString::new("HOME=/root").unwrap();
                let envp: Vec<*const libc::c_char> = vec![
                    term.as_ptr(),
                    path.as_ptr(),
                    home.as_ptr(),
                    std::ptr::null(),
                ];
                libc::execve(cmd.as_ptr(), argv_ptrs.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }
        unsafe { libc::close(slave) };

        // Parent: pump conn <-> pty master.
        let master_file = unsafe { std::fs::File::from_raw_fd(master) };
        let mut pty_writer = master_file.try_clone().expect("clone pty");
        let conn_to_pty = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // `reader` still holds the buffered remainder after ShellOpen.
            let mut r = reader;
            loop {
                match r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if pty_writer.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            unsafe { libc::kill(pid, libc::SIGHUP) };
        });

        let mut pty_reader = master_file;
        let mut writer = conn;
        let mut buf = [0u8; 8192];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = writer.flush();
        // Unblock the conn->pty thread (blocked in read) before joining.
        unsafe { libc::shutdown(writer.as_raw_fd(), libc::SHUT_RDWR) };
        let _ = conn_to_pty.join();
        unsafe {
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    agent::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("vessel-agent only runs inside a Linux guest");
    std::process::exit(1);
}
