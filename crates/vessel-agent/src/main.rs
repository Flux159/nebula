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
        k3s::autostart();
        std::thread::spawn(|| serve_loop("shell", VSOCK_PORT_SHELL, handle_shell));
        std::thread::spawn(|| serve_loop("docker-proxy", VSOCK_PORT_DOCKER, handle_docker_proxy));
        std::thread::spawn(|| serve_loop("tcp-proxy", VSOCK_PORT_TCPPROXY, handle_tcp_proxy));
        std::thread::spawn(|| {
            serve_loop(
                "containerd-proxy",
                VSOCK_PORT_CONTAINERD,
                handle_containerd_proxy,
            )
        });
        std::thread::spawn(dns_proxy);
        serve_loop("control", VSOCK_PORT_CONTROL, handle_control)
    }

    /// Relay DNS: guest 127.0.0.1:53 (UDP) <-> nebulad at the NAT gateway.
    /// Queries resolve on the HOST (getaddrinfo), so VPN/split-horizon DNS
    /// behaves exactly like the Mac's own.
    fn dns_proxy() {
        use std::net::UdpSocket;
        let Some(gw) = default_gateway() else {
            eprintln!("vessel-agent: dns: no default gateway; relay disabled");
            return;
        };
        let port: u16 = std::env::var("NEBULA_DNS_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(HOST_DNS_UDP_PORT);
        let upstream = format!("{gw}:{port}");
        let sock = loop {
            // 0.0.0.0: containers query via the docker0 address (daemon.json
            // sets "dns": ["172.17.0.1"]), the guest itself via 127.0.0.1.
            match UdpSocket::bind("0.0.0.0:53") {
                Ok(s) => break s,
                Err(e) => {
                    eprintln!("vessel-agent: dns bind failed: {e}; retrying");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        };
        eprintln!("vessel-agent: dns relay 127.0.0.1:53 -> {upstream}");
        let mut buf = [0u8; 1500];
        loop {
            let Ok((n, client)) = sock.recv_from(&mut buf) else {
                continue;
            };
            // One ephemeral upstream socket per query keeps responses matched.
            let Ok(up) = UdpSocket::bind("0.0.0.0:0") else {
                continue;
            };
            let _ = up.set_read_timeout(Some(Duration::from_secs(4)));
            if up.send_to(&buf[..n], &upstream).is_err() {
                continue;
            }
            let mut resp = [0u8; 1500];
            if let Ok(rn) = up.recv(&mut resp) {
                let _ = sock.send_to(&resp[..rn], client);
            }
        }
    }

    /// Default gateway from /proc/net/route (hex little-endian).
    fn default_gateway() -> Option<std::net::Ipv4Addr> {
        for _ in 0..50 {
            if let Ok(route) = std::fs::read_to_string("/proc/net/route") {
                for line in route.lines().skip(1) {
                    let cols: Vec<&str> = line.split_whitespace().collect();
                    if cols.len() >= 3 && cols[1] == "00000000" {
                        if let Ok(gw) = u32::from_str_radix(cols[2], 16) {
                            if gw != 0 {
                                return Some(std::net::Ipv4Addr::from(gw.to_le_bytes()));
                            }
                        }
                    }
                }
            }
            // DHCP may not have installed the route yet.
            std::thread::sleep(Duration::from_millis(200));
        }
        None
    }

    /// eth0 IPv4 via getifaddrs.
    fn eth0_ip() -> Option<String> {
        unsafe {
            let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
            if libc::getifaddrs(&mut addrs) != 0 {
                return None;
            }
            let mut cur = addrs;
            let mut found = None;
            while !cur.is_null() {
                let ifa = &*cur;
                if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
                    let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                    if name == "eth0" && (*ifa.ifa_addr).sa_family == libc::AF_INET as u16 {
                        let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                        found = Some(ip.to_string());
                        break;
                    }
                }
                cur = ifa.ifa_next;
            }
            libc::freeifaddrs(addrs);
            found
        }
    }

    fn handle_docker_proxy(conn: std::fs::File) {
        forward_to_unix(conn, "/var/run/docker.sock");
    }

    /// Generic host->guest TCP proxy: the first 2 bytes (BE) name a guest
    /// TCP port; the rest of the stream splices to 127.0.0.1:<port>. This is
    /// how published container ports reach the host on backends where the
    /// guest IP is not host-routable (libkrun/KVM today, WHP later).
    fn handle_tcp_proxy(conn: std::fs::File) {
        use std::io::{Read, Write};
        let mut conn = conn;
        let mut hdr = [0u8; 2];
        if conn.read_exact(&mut hdr).is_err() {
            return;
        }
        let port = u16::from_be_bytes(hdr);
        let Ok(tcp) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
            return;
        };
        let mut conn_r = conn.try_clone().expect("clone conn");
        let mut conn_w = conn;
        let mut tcp_r = tcp.try_clone().expect("clone tcp");
        let mut tcp_w = tcp;
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                match conn_r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tcp_w.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = tcp_w.shutdown(std::net::Shutdown::Write);
        });
        let mut buf = [0u8; 65536];
        loop {
            match tcp_r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if conn_w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = t.join();
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
            AgentRequest::ServiceCtl { name, action } => {
                if name != "k3s" {
                    return AgentResponse::Error {
                        message: format!("unknown service `{name}`"),
                    };
                }
                match action {
                    ServiceAction::Start => {
                        k3s::enable();
                        AgentResponse::Ok
                    }
                    ServiceAction::Stop => {
                        k3s::disable();
                        AgentResponse::Ok
                    }
                    ServiceAction::Status => AgentResponse::Service {
                        running: k3s::running(),
                        enabled: k3s::enabled(),
                    },
                }
            }
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
            ip: eth0_ip(),
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

    /// Agent-supervised k3s: started on demand, persisted across boots, and
    /// restarted on crash while enabled. Uses k3s's embedded containerd for
    /// now (shared-containerd unification is tracked in tasks/issues.md).
    pub mod k3s {
        use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
        use std::time::Duration;

        const MARKER: &str = "/var/lib/nebula/k3s.enabled";
        const K3S_BIN: &str = "/usr/local/bin/k3s";
        static SUPERVISING: AtomicBool = AtomicBool::new(false);
        static CHILD_PID: AtomicI32 = AtomicI32::new(-1);

        pub fn enabled() -> bool {
            std::path::Path::new(MARKER).exists()
        }

        pub fn running() -> bool {
            let pid = CHILD_PID.load(Ordering::SeqCst);
            pid > 0 && unsafe { libc::kill(pid, 0) == 0 }
        }

        pub fn autostart() {
            if enabled() {
                start_supervisor();
            }
        }

        pub fn enable() {
            let _ = std::fs::write(MARKER, b"");
            start_supervisor();
        }

        pub fn disable() {
            let _ = std::fs::remove_file(MARKER);
            let pid = CHILD_PID.load(Ordering::SeqCst);
            if pid > 0 {
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }

        fn start_supervisor() {
            if SUPERVISING.swap(true, Ordering::SeqCst) {
                return;
            }
            std::thread::spawn(|| loop {
                if !enabled() {
                    SUPERVISING.store(false, Ordering::SeqCst);
                    return;
                }
                let ip = super::eth0_ip().unwrap_or_default();
                let log = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("/var/log/k3s.log")
                    .ok();
                let mut cmd = std::process::Command::new(K3S_BIN);
                cmd.args([
                    "server",
                    "--write-kubeconfig-mode",
                    "644",
                    "--disable",
                    "traefik",
                ]);
                if !ip.is_empty() {
                    cmd.args(["--tls-san", &ip]);
                }
                cmd.env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                );
                if let Some(log) = log {
                    let l2 = log.try_clone().ok();
                    cmd.stdout(log);
                    if let Some(l2) = l2 {
                        cmd.stderr(l2);
                    }
                }
                match cmd.spawn() {
                    Ok(mut child) => {
                        CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
                        eprintln!("vessel-agent: k3s started (pid {})", child.id());
                        let _ = child.wait();
                        CHILD_PID.store(-1, Ordering::SeqCst);
                        eprintln!("vessel-agent: k3s exited");
                    }
                    Err(e) => eprintln!("vessel-agent: k3s spawn failed: {e}"),
                }
                std::thread::sleep(Duration::from_secs(1));
            });
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
