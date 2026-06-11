//! libkrun backend — GPU/sandbox sidecar microVMs (our vendored fork; the
//! brewed `libkrun-efi` dylib works for bring-up).
//!
//! Process model: `krun_start_enter` takes over the calling process and exits
//! with it, so every microVM runs in a worker subprocess (same as krunkit).
//! The handle spawns the current executable with a hidden `krun-worker` arg;
//! any binary embedding nebula-core must route that arg to [`run_worker`].

use std::ffi::{c_char, c_void, CString};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, Result};
use crate::spec::{BootSpec, ConsoleSpec, VmSpec};

use super::{VmHandle, VmState, VmmBackend};

const BACKEND: &str = "krun";

/// Candidate dylib paths, first hit wins. NEBULA_LIBKRUN_PATH overrides.
#[cfg(target_os = "macos")]
const DYLIB_CANDIDATES: &[&str] = &[
    "/opt/homebrew/opt/libkrun-efi/lib/libkrun-efi.dylib",
    "/opt/homebrew/lib/libkrun-efi.dylib",
    "/opt/homebrew/lib/libkrun.dylib",
    "/usr/local/lib/libkrun.dylib",
];
#[cfg(target_os = "linux")]
const DYLIB_CANDIDATES: &[&str] = &[
    "/usr/local/lib64/libkrun.so.1",
    "/usr/local/lib/libkrun.so.1",
    "/usr/lib64/libkrun.so.1",
    "/usr/lib/libkrun.so.1",
];
#[cfg(target_os = "windows")]
const DYLIB_CANDIDATES: &[&str] = &[];

/// Near-binary fork locations checked before the system candidates
/// (app bundle / embed kit / dev tree).
#[cfg(target_os = "macos")]
const NEAR_EXE_CANDIDATES: &[&str] = &[
    "Frameworks/libkrun.dylib", // Nebula.app/Contents/Frameworks
    "lib/libkrun.dylib",        // embed kit / zip install layout
    "third_party/libkrun/target/release/libkrun.1.18.0.dylib",
];
#[cfg(target_os = "linux")]
const NEAR_EXE_CANDIDATES: &[&str] = &[
    "lib/libkrun.so.1",
    // plain `cargo build` output first: the versioned name is only refreshed
    // by scripts/build-libkrun.sh and goes stale in dev trees
    "third_party/libkrun/target/release/libkrun.so",
    "third_party/libkrun/target/release/libkrun.so.1.18.0",
];
#[cfg(target_os = "windows")]
const NEAR_EXE_CANDIDATES: &[&str] = &[
    "krun.dll",
    "lib/krun.dll",
    "third_party/libkrun/target/release/krun.dll",
];

/// Direct kernel boot format per guest arch: arm64 takes the raw `Image`,
/// x86_64 takes an ELF `vmlinux`.
#[cfg(target_arch = "aarch64")]
const KRUN_KERNEL_FORMAT: u32 = 0; // KRUN_KERNEL_FORMAT_RAW
#[cfg(target_arch = "x86_64")]
const KRUN_KERNEL_FORMAT: u32 = 1; // KRUN_KERNEL_FORMAT_ELF

#[cfg(unix)]
type ConsoleIo = i32;
#[cfg(windows)]
type ConsoleIo = *mut c_void;

#[allow(non_snake_case)]
struct KrunApi {
    krun_create_ctx: unsafe extern "C" fn() -> i32,
    krun_set_log_level: unsafe extern "C" fn(u32) -> i32,
    krun_set_vm_config: unsafe extern "C" fn(u32, u8, u32) -> i32,
    krun_set_kernel:
        unsafe extern "C" fn(u32, *const c_char, u32, *const c_char, *const c_char) -> i32,
    /// Wire a virtio-console (hvc0) to explicit host fds (unix) or
    /// kernel HANDLEs (windows).
    krun_add_virtio_console_default:
        unsafe extern "C" fn(u32, ConsoleIo, ConsoleIo, ConsoleIo) -> i32,
    /// Optional: only exported when libkrun is built with BLK=1.
    krun_add_disk2:
        Option<unsafe extern "C" fn(u32, *const c_char, *const c_char, u32, bool) -> i32>,
    /// Optional: virtiofs is unix-only in our fork (absent from krun.dll).
    krun_add_virtiofs: Option<unsafe extern "C" fn(u32, *const c_char, *const c_char) -> i32>,
    /// Optional: only exported when libkrun is built with GPU=1.
    krun_set_gpu_options: Option<unsafe extern "C" fn(u32, u32) -> i32>,
    /// Map a host unix socket to a guest vsock port (listen=true: host-initiated).
    krun_add_vsock_port2: unsafe extern "C" fn(u32, u32, *const c_char, bool) -> i32,
    /// Optional: only exported when libkrun is built with NET=1. Attaches a
    /// virtio-net device backed by a userspace proxy (passt) unix socket.
    krun_add_net_unixstream:
        Option<unsafe extern "C" fn(u32, *const c_char, i32, *const u8, u32, u32) -> i32>,
    /// Optional: our fork's in-process usermode NAT (no passt needed).
    krun_add_net_usernet: Option<unsafe extern "C" fn(u32, *const u8) -> i32>,
    krun_start_enter: unsafe extern "C" fn(u32) -> i32,
    /// Optional: pause/resume all vCPUs (snapshot support; linux/windows fork).
    krun_vm_pause: Option<unsafe extern "C" fn(u32) -> i32>,
    krun_vm_resume: Option<unsafe extern "C" fn(u32) -> i32>,
    krun_vm_save: Option<unsafe extern "C" fn(u32, *const c_char) -> i32>,
    /// Optional: restore from a snapshot directory instead of booting
    /// (linux x86_64 fork).
    krun_set_restore: Option<unsafe extern "C" fn(u32, *const c_char) -> i32>,
}

#[cfg(windows)]
mod win_dl {
    use std::ffi::{c_char, c_void};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn LoadLibraryW(lp_lib_file_name: *const u16) -> *mut c_void;
        pub fn GetProcAddress(h_module: *mut c_void, lp_proc_name: *const c_char) -> *mut c_void;
    }
}

/// virglrenderer flags (libkrun.h): VENUS | no_virgl | thread_sync | external_blob.
const VIRGLRENDERER_THREAD_SYNC: u32 = 1 << 1;
const VIRGLRENDERER_USE_EXTERNAL_BLOB: u32 = 1 << 5;
const VIRGLRENDERER_VENUS: u32 = 1 << 6;
const VIRGLRENDERER_NO_VIRGL: u32 = 1 << 7;

fn dylib_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("NEBULA_LIBKRUN_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(Error::FileNotFound(p));
    }
    // Our fork first: stock libkrun-efi cannot direct-kernel-boot the raw
    // Image (FirmwareInvalidAddress) and lacks the GPU/vsock-port options.
    // Look near the running binary: app bundle Frameworks, then the dev tree.
    if let Ok(exe) = std::env::current_exe() {
        for anc in exe.ancestors() {
            for rel in NEAR_EXE_CANDIDATES {
                let candidate = anc.join(rel);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    DYLIB_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Error::BackendUnavailable(
                BACKEND,
                "libkrun dylib not found (brew install slp/krunkit/libkrun-efi, or set NEBULA_LIBKRUN_PATH)"
                    .into(),
            )
        })
}

fn load_api() -> Result<&'static KrunApi> {
    static API: OnceLock<std::result::Result<KrunApi, String>> = OnceLock::new();
    let res = API.get_or_init(|| {
        let path = dylib_path().map_err(|e| e.to_string())?;
        #[cfg(unix)]
        let handle = {
            let cpath = CString::new(path.to_string_lossy().as_bytes()).unwrap();
            let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() {
                return Err(format!("dlopen({}) failed", path.display()));
            }
            handle
        };
        #[cfg(windows)]
        let handle = {
            let mut wide: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
            wide.push(0);
            let handle = unsafe { win_dl::LoadLibraryW(wide.as_ptr()) };
            if handle.is_null() {
                return Err(format!(
                    "LoadLibraryW({}) failed: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            handle
        };
        unsafe fn sym<T>(handle: *mut c_void, name: &str) -> std::result::Result<T, String> {
            let cname = CString::new(name).unwrap();
            #[cfg(unix)]
            let ptr = unsafe { libc::dlsym(handle, cname.as_ptr()) };
            #[cfg(windows)]
            let ptr = unsafe { win_dl::GetProcAddress(handle, cname.as_ptr()) };
            if ptr.is_null() {
                return Err(format!("symbol `{name}` missing from libkrun"));
            }
            Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&ptr) })
        }
        unsafe {
            Ok(KrunApi {
                krun_create_ctx: sym(handle, "krun_create_ctx")?,
                krun_set_log_level: sym(handle, "krun_set_log_level")?,
                krun_set_vm_config: sym(handle, "krun_set_vm_config")?,
                krun_set_kernel: sym(handle, "krun_set_kernel")?,
                krun_add_virtio_console_default: sym(handle, "krun_add_virtio_console_default")?,
                krun_add_disk2: sym(handle, "krun_add_disk2").ok(),
                krun_add_virtiofs: sym(handle, "krun_add_virtiofs").ok(),
                krun_set_gpu_options: sym(handle, "krun_set_gpu_options").ok(),
                krun_add_vsock_port2: sym(handle, "krun_add_vsock_port2")?,
                krun_add_net_unixstream: sym(handle, "krun_add_net_unixstream").ok(),
                krun_add_net_usernet: sym(handle, "krun_add_net_usernet").ok(),
                krun_start_enter: sym(handle, "krun_start_enter")?,
                krun_vm_pause: sym(handle, "krun_vm_pause").ok(),
                krun_vm_resume: sym(handle, "krun_vm_resume").ok(),
                krun_vm_save: sym(handle, "krun_vm_save").ok(),
                krun_set_restore: sym(handle, "krun_set_restore").ok(),
            })
        }
    });
    res.as_ref()
        .map_err(|e| Error::BackendUnavailable(BACKEND, e.clone()))
}

pub struct KrunBackend;

impl KrunBackend {
    pub fn new() -> Self {
        KrunBackend
    }
}

impl Default for KrunBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl VmmBackend for KrunBackend {
    fn name(&self) -> &'static str {
        BACKEND
    }

    fn is_available(&self) -> Result<()> {
        load_api().map(|_| ())
    }

    fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>> {
        spec.validate()?;
        self.is_available()?;
        Ok(Box::new(KrunVm {
            spec: spec.clone(),
            child: Mutex::new(None),
        }))
    }
}

pub struct KrunVm {
    spec: VmSpec,
    /// Worker subprocess. Mutex so `state(&self)` can reap (try_wait needs &mut).
    child: Mutex<Option<Child>>,
}

impl VmHandle for KrunVm {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn state(&self) -> VmState {
        let mut guard = self.child.lock().unwrap();
        match guard.as_mut() {
            None => VmState::Created,
            Some(child) => match child.try_wait() {
                Ok(None) => VmState::Running,
                Ok(Some(status)) if status.success() => VmState::Stopped,
                Ok(Some(_)) => VmState::Failed,
                Err(_) => VmState::Failed,
            },
        }
    }

    fn start(&mut self) -> Result<()> {
        if self.child.lock().unwrap().is_some() {
            return Err(Error::backend(BACKEND, "VM already started"));
        }
        let worker = std::env::var("NEBULA_WORKER_EXE")
            .map(PathBuf::from)
            .or_else(|_| std::env::current_exe())?;
        let spec_json = serde_json::to_string(&self.spec)
            .map_err(|e| Error::backend(BACKEND, format!("spec serialize: {e}")))?;
        // Debug aid: the worker's stderr is often lost to detached spawn
        // chains; persist the spec so a failing worker can be re-run by hand.
        if std::env::var_os("NEBULA_DEBUG").is_some() {
            if let ConsoleSpec::File(path) = &self.spec.console {
                let _ = std::fs::write(path.with_extension("spec.json"), &spec_json);
            }
        }
        // libkrun routes some guest console traffic to the worker's stdio, so
        // aim the worker's stdout at the console file as well.
        let stdout: std::process::Stdio = match &self.spec.console {
            ConsoleSpec::File(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::File::create(path)?.into()
            }
            ConsoleSpec::None => std::process::Stdio::null(),
        };
        // Worker stderr to a file next to the console log: inherit chains
        // get severed by detached daemon spawns (notably on Windows), and a
        // silent worker death is undebuggable without this.
        let stderr: std::process::Stdio = match &self.spec.console {
            ConsoleSpec::File(path) => {
                std::fs::File::create(path.with_extension("worker-stderr.log"))?.into()
            }
            ConsoleSpec::None => std::process::Stdio::inherit(),
        };
        let child = Command::new(worker)
            .arg("krun-worker")
            .arg("--spec")
            .arg(spec_json)
            .stdin(std::process::Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()?;
        *self.child.lock().unwrap() = Some(child);
        Ok(())
    }

    fn stop(&mut self, force: bool) -> Result<()> {
        let mut guard = self.child.lock().unwrap();
        let Some(child) = guard.as_mut() else {
            return Ok(());
        };
        #[cfg(unix)]
        {
            let pid = child.id() as i32;
            let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
            unsafe { libc::kill(pid, sig) };
        }
        #[cfg(windows)]
        {
            // No SIGTERM equivalent for a console-less worker; terminate.
            let _ = force;
            let _ = child.kill();
        }
        let _ = child.wait();
        Ok(())
    }

    /// libkrun maps guest vsock ports to host unix sockets (vsock_ports), so
    /// "connect to guest port N" = connect to the unix socket mapped to N.
    /// This is what lets nebulad's agent/socket proxies run unchanged on the
    /// krun backend (the Linux engine path).
    fn vsock_connect(&self, port: u32) -> Result<super::VsockStream> {
        let map = self
            .spec
            .vsock_ports
            .iter()
            .find(|m| m.port == port)
            .ok_or_else(|| {
                Error::backend(
                    BACKEND,
                    format!("vsock:{port}: no host socket mapped for this port"),
                )
            })?;
        #[cfg(unix)]
        return std::os::unix::net::UnixStream::connect(&map.host_path).map_err(|e| {
            Error::backend(
                BACKEND,
                format!("vsock:{port} via {}: {e}", map.host_path.display()),
            )
        });
        // Windows: the fork's vsock backend binds 127.0.0.1:0 and publishes
        // the port number in a file at host_path (nebula ipc convention).
        #[cfg(windows)]
        {
            let text = std::fs::read_to_string(&map.host_path).map_err(|e| {
                Error::backend(
                    BACKEND,
                    format!("vsock:{port} port file {}: {e}", map.host_path.display()),
                )
            })?;
            let tcp_port: u16 = text.trim().parse().map_err(|_| {
                Error::backend(
                    BACKEND,
                    format!("vsock:{port} bad port file {}", map.host_path.display()),
                )
            })?;
            std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, tcp_port)).map_err(|e| {
                Error::backend(
                    BACKEND,
                    format!("vsock:{port} via 127.0.0.1:{tcp_port}: {e}"),
                )
            })
        }
    }

    fn balloon_set_guest_mib(&mut self, _target_mib: u64) -> Result<()> {
        // Sidecars are ephemeral; balloon support comes with the fork if needed (4.1).
        Err(Error::backend(
            BACKEND,
            "balloon not supported on krun sidecars yet",
        ))
    }
}

impl Drop for KrunVm {
    fn drop(&mut self) {
        if matches!(self.state(), VmState::Running) {
            let _ = self.stop(true);
        }
    }
}

/// Entry point for the worker subprocess. Configures the microVM from the JSON
/// spec and enters it; never returns (the process becomes the VM).
/// Find passt for NetSpec::Nat NICs. Env override, PATH, then ~/.local/bin
/// (the sudo-free dpkg-extract location).
#[cfg(unix)]
fn passt_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("NEBULA_PASST_PATH") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let candidate = PathBuf::from(dir).join("passt");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let local = PathBuf::from(home).join(".local/bin/passt");
    local.is_file().then_some(local)
}

fn parse_mac(mac: Option<&str>) -> Option<[u8; 6]> {
    let mac = mac?;
    let mut out = [0u8; 6];
    let mut n = 0;
    for part in mac.split(':') {
        if n == 6 {
            return None;
        }
        out[n] = u8::from_str_radix(part, 16).ok()?;
        n += 1;
    }
    (n == 6).then_some(out)
}

pub fn run_worker(spec_json: &str) -> Result<std::convert::Infallible> {
    let spec: VmSpec = serde_json::from_str(spec_json)
        .map_err(|e| Error::backend(BACKEND, format!("bad spec: {e}")))?;
    let api = load_api()?;

    let cstr = |s: &str| CString::new(s).expect("no NUL in spec strings");
    let debug = std::env::var_os("NEBULA_DEBUG").is_some();
    let check = move |what: &str, code: i32| -> Result<()> {
        if debug {
            eprintln!("krun-worker: {what} -> {code}");
        }
        if code < 0 {
            Err(Error::backend(BACKEND, format!("{what} failed: {code}")))
        } else {
            Ok(())
        }
    };

    if debug {
        eprintln!("krun-worker: api loaded, calling krun_set_log_level");
    }
    unsafe {
        (api.krun_set_log_level)(if debug { 3 } else { 0 });
        if debug {
            eprintln!("krun-worker: krun_set_log_level ok, creating ctx");
        }
        let ctx = (api.krun_create_ctx)();
        check("krun_create_ctx", ctx)?;
        let ctx = ctx as u32;

        check(
            "krun_set_vm_config",
            (api.krun_set_vm_config)(ctx, spec.cpus.min(255) as u8, spec.mem_mib as u32),
        )?;

        let BootSpec::Kernel {
            kernel,
            initramfs,
            cmdline,
        } = &spec.boot;
        let kernel_c = cstr(&kernel.to_string_lossy());
        let initrd_c = initramfs.as_ref().map(|p| cstr(&p.to_string_lossy()));
        let cmdline_c = cstr(cmdline);
        check(
            "krun_set_kernel",
            (api.krun_set_kernel)(
                ctx,
                kernel_c.as_ptr(),
                KRUN_KERNEL_FORMAT,
                initrd_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                cmdline_c.as_ptr(),
            ),
        )?;

        if let ConsoleSpec::File(path) = &spec.console {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            #[cfg(unix)]
            {
                use std::os::fd::IntoRawFd;
                let out = std::fs::File::create(path)?.into_raw_fd();
                let err = libc::dup(out);
                let devnull = std::fs::File::open("/dev/null")?.into_raw_fd();
                // fds are intentionally leaked into the VMM (it owns them now);
                // err gets its own fd — libkrun's epoll shim rejects duplicates.
                check(
                    "krun_add_virtio_console_default",
                    (api.krun_add_virtio_console_default)(ctx, devnull, out, err),
                )?;
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::IntoRawHandle;
                let out_file = std::fs::File::create(path)?;
                let err_file = out_file.try_clone()?;
                let devnull = std::fs::File::open("NUL")?;
                // handles are intentionally leaked into the VMM (it owns them).
                check(
                    "krun_add_virtio_console_default",
                    (api.krun_add_virtio_console_default)(
                        ctx,
                        devnull.into_raw_handle(),
                        out_file.into_raw_handle(),
                        err_file.into_raw_handle(),
                    ),
                )?;
            }
        }

        // Networking: with no net device libkrun uses TSI (transparent
        // outbound, used by sidecars). NetSpec::Nat attaches a passt-backed
        // virtio-net NIC instead — the guest sees eth0 + DHCP, same shape as
        // VZ NAT (this is the Linux engine's outbound path: TSI's guest-side
        // hijack doesn't apply to our disk-boot/own-init flow).
        if matches!(spec.net, crate::spec::NetSpec::Nat) {
            let mac: [u8; 6] =
                parse_mac(spec.mac.as_deref()).unwrap_or([0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee]);
            // Default: the fork's in-process NAT (zero host deps; same path
            // Windows/WHP will use). NEBULA_NET=passt forces the external
            // proxy for debugging/comparison.
            let force_passt = std::env::var("NEBULA_NET").as_deref() == Ok("passt");
            if !force_passt {
                if let Some(add_usernet) = api.krun_add_net_usernet {
                    check("krun_add_net_usernet", add_usernet(ctx, mac.as_ptr()))?;
                }
            }
            #[cfg(windows)]
            if force_passt || api.krun_add_net_usernet.is_none() {
                return Err(Error::backend(
                    BACKEND,
                    "NetSpec::Nat on Windows needs a krun.dll built with NET=1 (usernet)",
                ));
            }
            #[cfg(unix)]
            if force_passt || api.krun_add_net_usernet.is_none() {
                // Fallback/debug path: external passt proxy.
                let add_net = api.krun_add_net_unixstream.ok_or_else(|| {
                    Error::backend(BACKEND, "NetSpec::Nat needs a libkrun built with NET=1")
                })?;
                let passt = passt_path().ok_or_else(|| {
                    Error::backend(
                        BACKEND,
                        "passt not found (apt/brew install passt, or set NEBULA_PASST_PATH)",
                    )
                })?;
                let sock =
                    std::env::temp_dir().join(format!("nebula-passt-{}.sock", std::process::id()));
                let _ = std::fs::remove_file(&sock);
                // --one-off: passt exits when the VM closes the connection.
                Command::new(&passt)
                    .arg("--quiet")
                    .arg("--one-off")
                    .arg("--socket")
                    .arg(&sock)
                    .spawn()
                    .map_err(|e| {
                        Error::backend(BACKEND, format!("spawn {}: {e}", passt.display()))
                    })?;
                for _ in 0..50 {
                    if sock.exists() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let sock_c = cstr(&sock.to_string_lossy());
                check(
                    "krun_add_net_unixstream",
                    add_net(ctx, sock_c.as_ptr(), -1, mac.as_ptr(), 0, 0),
                )?;
            }
        }

        if !spec.disks.is_empty() {
            let add_disk2 = api.krun_add_disk2.ok_or_else(|| {
                Error::backend(
                    BACKEND,
                    "this libkrun build lacks block devices (rebuild with BLK=1)",
                )
            })?;
            for (i, d) in spec.disks.iter().enumerate() {
                let id = cstr(&format!("disk{i}"));
                let path_c = cstr(&d.path.to_string_lossy());
                check(
                    "krun_add_disk2",
                    add_disk2(
                        ctx,
                        id.as_ptr(),
                        path_c.as_ptr(),
                        0, /* RAW */
                        d.read_only,
                    ),
                )?;
            }
        }

        for s in &spec.shares {
            let add_virtiofs = api.krun_add_virtiofs.ok_or_else(|| {
                Error::backend(BACKEND, "this libkrun build lacks virtiofs (unix-only)")
            })?;
            let tag = cstr(&s.tag);
            let path_c = cstr(&s.host_path.to_string_lossy());
            check(
                "krun_add_virtiofs",
                add_virtiofs(ctx, tag.as_ptr(), path_c.as_ptr()),
            )?;
        }

        for m in &spec.vsock_ports {
            // Stale sockets make the worker fail to bind on restart.
            let _ = std::fs::remove_file(&m.host_path);
            let path_c = cstr(&m.host_path.to_string_lossy());
            check(
                "krun_add_vsock_port2",
                (api.krun_add_vsock_port2)(ctx, m.port, path_c.as_ptr(), true),
            )?;
        }

        if spec.gpu {
            let set_gpu = api.krun_set_gpu_options.ok_or_else(|| {
                Error::backend(
                    BACKEND,
                    "this libkrun build lacks GPU support (rebuild with GPU=1)",
                )
            })?;
            check(
                "krun_set_gpu_options",
                set_gpu(
                    ctx,
                    VIRGLRENDERER_VENUS
                        | VIRGLRENDERER_NO_VIRGL
                        | VIRGLRENDERER_THREAD_SYNC
                        | VIRGLRENDERER_USE_EXTERNAL_BLOB,
                ),
            )?;
        }

        // Restore from a snapshot directory instead of cold-booting. The
        // device config above must match what the VM was saved with — the
        // VMM re-creates the devices from it, then applies their saved state.
        if let Some(restore_dir) = &spec.restore_path {
            let set_restore = api.krun_set_restore.ok_or_else(|| {
                Error::backend(
                    BACKEND,
                    "this libkrun build lacks snapshot restore (krun_set_restore)",
                )
            })?;
            let dir_c = cstr(&restore_dir.to_string_lossy());
            check("krun_set_restore", set_restore(ctx, dir_c.as_ptr()))?;
        }

        // Worker control channel: serves pause/save/resume for snapshots
        // (same JSON protocol as the vz-worker). Runs for the process
        // lifetime; krun_start_enter never returns.
        if let Some(ctl_path) = spec.control_path.clone() {
            let pause = api.krun_vm_pause;
            let resume = api.krun_vm_resume;
            let save = api.krun_vm_save;
            let _ = std::fs::remove_file(&ctl_path);
            match crate::ipc::listen(&ctl_path) {
                Ok(listener) => {
                    std::thread::Builder::new()
                        .name("vmm-ctl".into())
                        .spawn(move || serve_worker_control(listener, ctx, pause, resume, save))
                        .ok();
                }
                Err(e) => eprintln!("krun-worker: control listener {}: {e}", ctl_path.display()),
            }
        }

        if debug {
            eprintln!("krun-worker: configured, entering krun_start_enter");
        }
        // Takes over the process; exits when the guest powers off.
        let rc = (api.krun_start_enter)(ctx);
        eprintln!("krun-worker: krun_start_enter returned {rc}");
        // Normally unreachable — krun_start_enter exits the process itself.
        std::process::exit(if rc < 0 { 1 } else { 0 });
    }
}

/// Serve the snapshot control protocol (one JSON line per message) until the
/// process exits. Mirrors the vz-worker's control loop.
fn serve_worker_control(
    listener: crate::ipc::IpcListener,
    ctx: u32,
    pause: Option<unsafe extern "C" fn(u32) -> i32>,
    resume: Option<unsafe extern "C" fn(u32) -> i32>,
    save: Option<unsafe extern "C" fn(u32, *const c_char) -> i32>,
) {
    use crate::backend::{WorkerControl, WorkerReply};
    use std::io::{BufRead, Write};

    let reply = |stream: &mut dyn Write, ok: bool, error: Option<String>, state: Option<String>| {
        let r = WorkerReply { ok, error, state };
        if let Ok(json) = serde_json::to_string(&r) {
            let _ = writeln!(stream, "{json}");
        }
    };

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let mut writer = match stream.try_clone() {
            Ok(w) => w,
            Err(_) => continue,
        };
        let reader = std::io::BufReader::new(stream);
        let mut paused = false;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let ctl: WorkerControl = match serde_json::from_str(&line) {
                Ok(c) => c,
                Err(e) => {
                    reply(
                        &mut writer,
                        false,
                        Some(format!("bad control msg: {e}")),
                        None,
                    );
                    continue;
                }
            };
            match ctl {
                WorkerControl::Pause => match pause {
                    Some(f) => {
                        let rc = unsafe { f(ctx) };
                        paused = rc == 0;
                        reply(
                            &mut writer,
                            rc == 0,
                            (rc != 0).then(|| format!("krun_vm_pause failed: {rc}")),
                            None,
                        );
                    }
                    None => reply(
                        &mut writer,
                        false,
                        Some("this libkrun build has no pause support".into()),
                        None,
                    ),
                },
                WorkerControl::Resume => match resume {
                    Some(f) => {
                        let rc = unsafe { f(ctx) };
                        if rc == 0 {
                            paused = false;
                        }
                        reply(
                            &mut writer,
                            rc == 0,
                            (rc != 0).then(|| format!("krun_vm_resume failed: {rc}")),
                            None,
                        );
                    }
                    None => reply(
                        &mut writer,
                        false,
                        Some("this libkrun build has no resume support".into()),
                        None,
                    ),
                },
                WorkerControl::Save { path } => match save {
                    Some(f) => {
                        let c_path = CString::new(path.to_string_lossy().as_bytes()).unwrap();
                        let rc = unsafe { f(ctx, c_path.as_ptr()) };
                        reply(
                            &mut writer,
                            rc == 0,
                            (rc != 0).then(|| format!("krun_vm_save failed: {rc}")),
                            None,
                        );
                    }
                    None => reply(
                        &mut writer,
                        false,
                        Some("this libkrun build has no snapshot-save support".into()),
                        None,
                    ),
                },
                WorkerControl::State => reply(
                    &mut writer,
                    true,
                    None,
                    Some(if paused { "paused" } else { "running" }.into()),
                ),
            }
        }
    }
}
