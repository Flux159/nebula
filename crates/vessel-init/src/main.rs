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
            let _ = std::process::Command::new("/sbin/mkfs.ext4")
                .args(["-q", "-L", "nebula-data", DATA_DEV])
                .status();
        }
        mount(DATA_DEV, DATA_MNT, "ext4", 0, None);
    }

    fn vessel_mode() -> ! {
        attach_console();
        let _ = std::fs::write("/proc/sys/kernel/hostname", "nebula");
        setup_data_disk();
        println!(
            "NEBULA_VESSEL_UP init={} uname={}",
            env!("CARGO_PKG_VERSION"),
            uname_line()
        );

        // Supervise the agent; reap all zombies (we are PID 1).
        let mut agent = spawn_agent();
        loop {
            let mut status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid == agent {
                eprintln!("nebula-init: vessel-agent exited (status {status}), restarting");
                std::thread::sleep(Duration::from_millis(250));
                agent = spawn_agent();
            } else if pid < 0 {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    fn spawn_agent() -> libc::pid_t {
        match std::process::Command::new(AGENT_PATH).spawn() {
            Ok(child) => child.id() as libc::pid_t,
            Err(e) => {
                eprintln!("nebula-init: failed to start agent: {e}");
                -1
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
