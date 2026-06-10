//! Nebula guest init (PID 1).
//!
//! Two modes, decided by what's present on the filesystem:
//! - **Vessel mode** (`/usr/bin/vessel-agent` exists — booted from the real
//!   rootfs): mount pseudo-filesystems + cgroup2, set hostname, mount the data
//!   disk, then supervise vessel-agent forever (restart on crash, reap zombies).
//! - **Spike mode** (initramfs-only boot): print a boot marker and power off —
//!   used by `nebula up --dev` and the backend acceptance tests.
//!
//! Console dance: under libkrun the console is virtio-MMIO; with a non-tty
//! output fd libkrun routes hvc0 to its log and wires our capture file to a
//! virtio port named "krun-stdout", so prefer that port when present. Under VZ
//! hvc0 is correct from the start.

#[cfg(target_os = "linux")]
mod init {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    const AGENT_PATH: &str = "/usr/bin/vessel-agent";
    const DATA_DEV: &str = "/dev/vdb";
    const DATA_MNT: &str = "/var/lib/nebula";

    fn mount(src: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: Option<&str>) {
        let _ = std::fs::create_dir_all(target);
        let src = CString::new(src).unwrap();
        let target = CString::new(target).unwrap();
        let fstype = CString::new(fstype).unwrap();
        let data_c = data.map(|d| CString::new(d).unwrap());
        unsafe {
            libc::mount(
                src.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                flags,
                data_c
                    .as_ref()
                    .map_or(std::ptr::null(), |d| d.as_ptr() as *const libc::c_void),
            );
        }
    }

    fn mount_pseudo() {
        mount("proc", "/proc", "proc", 0, None);
        mount("sysfs", "/sys", "sysfs", 0, None);
        mount("devtmpfs", "/dev", "devtmpfs", 0, None);
        mount(
            "devpts",
            "/dev/pts",
            "devpts",
            0,
            Some("mode=0620,ptmxmode=0666"),
        );
        mount("tmpfs", "/run", "tmpfs", 0, Some("mode=0755"));
        mount("tmpfs", "/tmp", "tmpfs", 0, None);
        mount("cgroup2", "/sys/fs/cgroup", "cgroup2", 0, None);
        mount(
            "binfmt_misc",
            "/proc/sys/fs/binfmt_misc",
            "binfmt_misc",
            0,
            None,
        );
    }

    fn load_bundled_modules() {
        if let Ok(entries) = std::fs::read_dir("/") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "ko") {
                    if let Ok(file) = std::fs::File::open(&path) {
                        let empty = CString::new("").unwrap();
                        unsafe {
                            libc::syscall(
                                libc::SYS_finit_module,
                                file.as_raw_fd(),
                                empty.as_ptr(),
                                0,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Prefer libkrun's "krun-stdout" port (hvc0 goes to the VMM log there),
    /// fall back to /dev/hvc0. Retries because virtio may probe after PID 1.
    fn attach_console() -> bool {
        for _ in 0..40 {
            let mut candidates: Vec<std::path::PathBuf> = Vec::new();
            if let Ok(ports) = std::fs::read_dir("/sys/class/virtio-ports") {
                for p in ports.flatten() {
                    let name = std::fs::read_to_string(p.path().join("name")).unwrap_or_default();
                    if name.trim() == "krun-stdout" {
                        candidates.push(std::path::Path::new("/dev").join(p.file_name()));
                    }
                }
            }
            candidates.push("/dev/hvc0".into());
            for dev in candidates {
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&dev) {
                    let fd = f.as_raw_fd();
                    unsafe {
                        libc::dup2(fd, 1);
                        libc::dup2(fd, 2);
                    }
                    std::mem::forget(f);
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn uname_line() -> String {
        let mut uts = unsafe { std::mem::zeroed::<libc::utsname>() };
        unsafe { libc::uname(&mut uts) };
        let field = |f: &[libc::c_char]| -> String {
            unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        format!(
            "{} {} {}",
            field(&uts.sysname),
            field(&uts.release),
            field(&uts.machine)
        )
    }

    fn poweroff() -> ! {
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_millis(150));
        unsafe {
            libc::sync();
            libc::reboot(libc::RB_POWER_OFF);
        }
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    fn spike_mode() -> ! {
        let have_console = attach_console();
        if !have_console && std::env::var_os("NEBULA_HANG_NO_CONSOLE").is_some() {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        // Sandbox mode: run the bundled command, bracketed by markers the
        // host parses, then power off.
        if std::path::Path::new("/sandbox-cmd").exists() {
            println!("NEBULA_SANDBOX_BEGIN");
            let _ = std::io::stdout().flush();
            let status = std::process::Command::new("/bin/sh")
                .arg("/sandbox-cmd")
                .status();
            let code = status.ok().and_then(|s| s.code()).unwrap_or(127);
            println!("NEBULA_SANDBOX_END={code}");
            poweroff();
        }
        println!(
            "NEBULA_SPIKE_OK init={} uname={}",
            env!("CARGO_PKG_VERSION"),
            uname_line()
        );
        poweroff();
    }

    /// Format (first boot) and mount the data disk.
    fn setup_data_disk() {
        if !std::path::Path::new(DATA_DEV).exists() {
            return;
        }
        // Blank disk detection: no ext4 superblock magic at offset 0x438.
        let is_ext4 = std::fs::File::open(DATA_DEV)
            .and_then(|mut f| {
                use std::io::{Read, Seek, SeekFrom};
                let mut magic = [0u8; 2];
                f.seek(SeekFrom::Start(0x438))?;
                f.read_exact(&mut magic)?;
                Ok(magic == [0x53, 0xef])
            })
            .unwrap_or(false);
        if !is_ext4 {
            println!("nebula-init: formatting data disk {DATA_DEV}");
            // Leave 1 MiB of tail slack: the libkrun raw-image layer shaves
            // 64 KiB off sparse backing files on first open, and ext4 refuses
            // to mount when its block count exceeds the device.
            let blocks = std::fs::read_to_string("/sys/class/block/vdb/size")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|sectors| (sectors * 512 / 4096).saturating_sub(256));
            let mut cmd = std::process::Command::new("/sbin/mkfs.ext4");
            cmd.args(["-q", "-L", "nebula-data", DATA_DEV]);
            if let Some(blocks) = blocks {
                cmd.arg(blocks.to_string());
            }
            let _ = cmd.status();
        }
        mount(DATA_DEV, DATA_MNT, "ext4", 0, None);
    }

    /// Bring up eth0 via busybox udhcpc (writes resolv.conf via its default script).
    fn setup_network() {
        // busybox applets via the multicall binary (PATH/symlinks vary by image).
        let bb = "/bin/busybox";
        let _ = std::process::Command::new(bb)
            .args(["ip", "link", "set", "lo", "up"])
            .status();
        let _ = std::process::Command::new(bb)
            .args(["ip", "link", "set", "eth0", "up"])
            .status();
        // Background daemon: handles lease + renewals.
        let _ = std::process::Command::new(bb)
            .args(["udhcpc", "-i", "eth0", "-b", "-S"])
            .spawn();
        // The VZ NAT gateway doesn't serve DNS on current macOS, so the
        // DHCP-provided nameserver is dead weight. Public resolvers until the
        // Phase 3 host-backed resolver lands (see tasks/issues.md).
        std::thread::spawn(|| {
            // VZ (real NIC + gateway): the agent's DNS relay on 127.0.0.1
            // resolves via the host. libkrun (TSI, no NIC): outbound UDP is
            // hijacked and proxied by the VMM, so a public resolver works.
            for i in 0..20 {
                std::thread::sleep(Duration::from_millis(500));
                let has_gw = std::fs::read_to_string("/proc/net/route")
                    .map(|r| {
                        r.lines()
                            .skip(1)
                            .any(|l| l.split_whitespace().nth(1) == Some("00000000"))
                    })
                    .unwrap_or(false);
                // Give DHCP a moment before concluding there is no gateway.
                if !has_gw && i < 6 {
                    continue;
                }
                let want = if has_gw {
                    "nameserver 127.0.0.1\n"
                } else {
                    "nameserver 1.1.1.1\n"
                };
                let cur = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
                if cur != want {
                    let _ = std::fs::write("/etc/resolv.conf", want);
                }
            }
        });
    }

    /// Mount the Rosetta directory share (if the host attached one) and register
    /// the x86_64 binfmt handler so amd64 binaries run transparently.
    fn setup_rosetta() {
        let target = "/media/rosetta";
        let _ = std::fs::create_dir_all(target);
        let mounted = std::process::Command::new("/bin/mount")
            .args(["-t", "virtiofs", "rosetta", target])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !mounted || !std::path::Path::new("/media/rosetta/rosetta").exists() {
            return;
        }
        let register = concat!(
            ":rosetta:M::",
            "\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\x3e\\x00",
            ":",
            "\\xff\\xff\\xff\\xff\\xff\\xfe\\xfe\\x00\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xfe\\xff\\xff\\xff",
            ":/media/rosetta/rosetta:OCF"
        );
        match std::fs::write("/proc/sys/fs/binfmt_misc/register", register) {
            Ok(()) => println!("nebula-init: rosetta binfmt registered"),
            Err(e) => eprintln!("nebula-init: rosetta binfmt registration failed: {e}"),
        }
    }

    struct Service {
        name: &'static str,
        cmd: &'static str,
        args: &'static [&'static str],
        /// Don't start until this path exists (cheap dependency ordering).
        wait_for: Option<&'static str>,
        pid: libc::pid_t,
    }

    fn spawn_service(svc: &Service) -> libc::pid_t {
        if let Some(path) = svc.wait_for {
            for _ in 0..100 {
                if std::path::Path::new(path).exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("/var/log/{}.log", svc.name))
            .ok();
        let mut cmd = std::process::Command::new(svc.cmd);
        cmd.args(svc.args);
        cmd.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
        cmd.env("HOME", "/root");
        if let Some(log) = log {
            let log2 = log.try_clone().ok();
            cmd.stdout(log);
            if let Some(l2) = log2 {
                cmd.stderr(l2);
            }
        }
        match cmd.spawn() {
            Ok(child) => {
                println!("nebula-init: started {} (pid {})", svc.name, child.id());
                child.id() as libc::pid_t
            }
            Err(e) => {
                eprintln!("nebula-init: failed to start {}: {e}", svc.name);
                -1
            }
        }
    }

    /// Mount the host home share at the same absolute path as on macOS, so
    /// `docker -v ~/x:/x` paths resolve identically on both sides.
    fn setup_home_share() {
        let Some(home) = std::env::var_os("NEBULA_HOME") else {
            return;
        };
        let home = home.to_string_lossy().into_owned();
        let _ = std::fs::create_dir_all(&home);
        let status = std::process::Command::new("/bin/mount")
            .args(["-t", "virtiofs", "home", &home])
            .status();
        match status {
            Ok(s) if s.success() => println!("nebula-init: home share mounted at {home}"),
            _ => eprintln!("nebula-init: home share mount failed"),
        }
    }

    fn vessel_mode() -> ! {
        attach_console();
        let _ = std::fs::write("/proc/sys/kernel/hostname", "nebula");
        let _ = std::fs::create_dir_all("/var/log");
        setup_data_disk();
        setup_network();
        setup_rosetta();
        setup_home_share();
        // dockerd requires forwarding for container NAT.
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        println!(
            "NEBULA_VESSEL_UP init={} uname={}",
            env!("CARGO_PKG_VERSION"),
            uname_line()
        );

        let agent_only = std::env::var_os("NEBULA_AGENT_ONLY").is_some();
        let mut services = vec![
            Service {
                name: "vessel-agent",
                cmd: AGENT_PATH,
                args: &[],
                wait_for: None,
                pid: -1,
            },
            Service {
                name: "containerd",
                cmd: "/usr/bin/containerd",
                args: &["--config", "/etc/containerd/config.toml"],
                wait_for: Some(DATA_MNT),
                pid: -1,
            },
            Service {
                name: "dockerd",
                cmd: "/usr/bin/dockerd",
                args: &[
                    "--host=unix:///var/run/docker.sock",
                    "--containerd=/run/containerd/containerd.sock",
                ],
                wait_for: Some("/run/containerd/containerd.sock"),
                pid: -1,
            },
        ];
        if agent_only {
            // Named vessels: a clean Linux VM with just the agent (the user
            // installs what they want; docker/k8s live in the engine vessel).
            services.truncate(1);
        }
        // Stagger startup so wait_for dependencies resolve in order.
        for svc in services.iter_mut() {
            svc.pid = spawn_service(svc);
        }

        // Supervise: restart dead services; reap all zombies (we are PID 1).
        loop {
            let mut status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid > 0 {
                if let Some(svc) = services.iter_mut().find(|s| s.pid == pid) {
                    eprintln!(
                        "nebula-init: {} exited (status {status}), restarting",
                        svc.name
                    );
                    std::thread::sleep(Duration::from_millis(250));
                    svc.pid = spawn_service(svc);
                }
            } else {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    pub fn main() -> ! {
        mount_pseudo();
        load_bundled_modules();
        if std::path::Path::new(AGENT_PATH).exists() {
            vessel_mode()
        } else {
            spike_mode()
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    init::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("vessel-init only runs inside a Linux guest");
    std::process::exit(1);
}
