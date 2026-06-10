//! Nebula guest init (PID 1). Phase 0 scope: prove the boot path on both
//! backends — mount pseudo-filesystems, print a machine-readable marker with
//! uname, and power off. Grows into the real Vessel init in Phase 1.
//!
//! Console dance: under libkrun the console is virtio-MMIO and stock kernels
//! ship `virtio_mmio` as a module, so PID 1 starts before hvc0 exists. We
//! insmod any /*.ko bundled in the initramfs, wait for /dev/hvc0, and re-point
//! stdio at it (the stdio the kernel gave us went nowhere).

#[cfg(target_os = "linux")]
fn main() {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    fn mount(src: &str, target: &str, fstype: &str) {
        let _ = std::fs::create_dir_all(target);
        let src = CString::new(src).unwrap();
        let target = CString::new(target).unwrap();
        let fstype = CString::new(fstype).unwrap();
        unsafe {
            libc::mount(
                src.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                std::ptr::null(),
            );
        }
    }

    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");
    mount("devtmpfs", "/dev", "devtmpfs");

    // Load any kernel modules bundled at the initramfs root (e.g. virtio_mmio).
    if let Ok(entries) = std::fs::read_dir("/") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "ko") {
                if let Ok(file) = std::fs::File::open(&path) {
                    let empty = CString::new("").unwrap();
                    unsafe {
                        libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), empty.as_ptr(), 0);
                    }
                }
            }
        }
    }

    // Re-point stdio at the best console available. Preference order:
    // 1. The virtio port libkrun names "krun-stdout" (its non-tty output fd —
    //    guest hvc0 goes to the VMM log there, not to our capture file).
    // 2. /dev/hvc0 (VZ, and libkrun-with-tty), which may appear late when
    //    virtio_mmio was just insmod'ed.
    let mut have_console = false;
    'outer: for _ in 0..40 {
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
                have_console = true;
                break 'outer;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Host-observable debug breadcrumb (kernel passes unknown FOO=bar cmdline
    // words to init as env): hang instead of powering off when no console was
    // found, so the host can tell "no console in guest" (Running timeout) from
    // "console capture broken host-side" (clean stop, empty file).
    if !have_console && std::env::var_os("NEBULA_HANG_NO_CONSOLE").is_some() {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    let mut uts = unsafe { std::mem::zeroed::<libc::utsname>() };
    unsafe { libc::uname(&mut uts) };
    let field = |f: &[libc::c_char]| -> String {
        unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };

    println!(
        "NEBULA_SPIKE_OK init={} uname={} {} {}",
        env!("CARGO_PKG_VERSION"),
        field(&uts.sysname),
        field(&uts.release),
        field(&uts.machine),
    );
    let _ = std::io::stdout().flush();

    // Give the console transport a beat to drain, then power off.
    std::thread::sleep(std::time::Duration::from_millis(150));
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    // If reboot ever fails, idle instead of exiting (PID 1 exit panics the kernel).
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("vessel-init only runs inside a Linux guest");
    std::process::exit(1);
}
