//! Phase 0 spike: boot a throwaway microVM on the chosen backend, verify the
//! guest actually ran (boot marker on the console), and report timing.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use nebula_core::backend::{backend_by_name, VmState};
use nebula_core::initramfs::InitramfsBuilder;
use nebula_core::{BootSpec, ConsoleSpec, NetSpec, VmSpec};

/// Alpine netboot kernel used as the spike guest kernel (replaced by our own
/// kernel build in Phase 1).
const ALPINE_KERNEL_URL: &str =
    "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/aarch64/netboot/vmlinuz-virt";

/// Guest binaries match the HOST arch (the microVM runs the same ISA).
const MUSL_TARGET: &str = if cfg!(target_arch = "aarch64") {
    "aarch64-unknown-linux-musl"
} else {
    "x86_64-unknown-linux-musl"
};

pub fn run(backend_name: &str, cpus: u32, mem: u64) -> anyhow::Result<()> {
    let home = dirs_home().context("cannot determine $HOME")?;
    let cache = home.join(".nebula/cache");
    let run_dir = home.join(".nebula/run");
    std::fs::create_dir_all(&cache)?;
    std::fs::create_dir_all(&run_dir)?;

    let kernel = ensure_kernel(&cache)?;
    let initramfs = build_spike_initramfs(&cache)?;
    let console_log = run_dir.join(format!("spike-{backend_name}-console.log"));

    // libkrun's aarch64 layout places the initramfs near the top of a window
    // that needs >= ~1 GiB of guest RAM; smaller values fail to build the VM.
    let mem = if backend_name == "krun" {
        mem.max(1024)
    } else {
        mem
    };

    let spec = VmSpec {
        name: format!("spike-{backend_name}"),
        cpus,
        mem_mib: mem,
        boot: BootSpec::Kernel {
            kernel,
            initramfs: Some(initramfs),
            cmdline: "console=hvc0 reboot=k panic=-1".into(),
        },
        disks: vec![],
        shares: vec![],
        net: NetSpec::None,
        vsock: false,
        console: ConsoleSpec::File(console_log.clone()),
        balloon: backend_name == "vz",
        rng: true,
        rosetta: false,
        gpu: false,
        control_path: None,
        restore_path: None,
        vsock_ports: vec![],
        backend: None,
        mac: None,
        machine_id: None,
    };

    let backend = backend_by_name(backend_name)?;
    backend.is_available()?;
    eprintln!(
        "[spike] backend={} cpus={} mem={}MiB",
        backend.name(),
        cpus,
        mem
    );

    let mut vm = backend.create(&spec)?;
    let t0 = Instant::now();
    vm.start()
        .context("VM start failed (is the binary signed? see scripts/sign-dev.sh)")?;
    eprintln!(
        "[spike] started in {:?}, waiting for guest power-off…",
        t0.elapsed()
    );

    vm.wait_for(VmState::Stopped, Duration::from_secs(60))?;
    let total = t0.elapsed();

    let console = std::fs::read_to_string(&console_log).unwrap_or_default();
    let marker = console.lines().find(|l| l.contains("NEBULA_SPIKE_OK"));
    match marker {
        Some(line) => {
            println!("SPIKE PASS [{}] boot→poweroff {:?}", backend.name(), total);
            println!("  guest: {}", line.trim());
            println!("  console log: {}", console_log.display());
            Ok(())
        }
        None => {
            eprintln!("--- console output ({} bytes) ---", console.len());
            for line in console
                .lines()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .iter()
                .rev()
            {
                eprintln!("  {line}");
            }
            bail!(
                "guest booted but no NEBULA_SPIKE_OK marker found (console: {})",
                console_log.display()
            );
        }
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Download (once) and decompress the spike kernel. Returns the uncompressed
/// arm64 `Image` path, which both VZLinuxBootLoader and krun_set_kernel accept.
fn ensure_kernel(cache: &std::path::Path) -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("NEBULA_SPIKE_KERNEL") {
        let p = PathBuf::from(p);
        anyhow::ensure!(p.is_file(), "NEBULA_SPIKE_KERNEL={} not found", p.display());
        return Ok(p);
    }
    let gz = cache.join("spike-vmlinuz-virt");
    let image = cache.join("spike-kernel-Image");
    if image.is_file() {
        return Ok(image);
    }
    if !gz.is_file() {
        eprintln!("[spike] downloading Alpine kernel…");
        let status = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&gz)
            .arg(ALPINE_KERNEL_URL)
            .status()
            .context("running curl")?;
        if !status.success() {
            bail!("kernel download failed: {ALPINE_KERNEL_URL}");
        }
    }
    let raw = extract_arm64_image(&std::fs::read(&gz)?)?;
    // Sanity: uncompressed arm64 Image has "ARM\x64" magic at offset 0x38.
    if raw.len() < 0x40 || &raw[0x38..0x3c] != b"ARM\x64" {
        bail!("extracted kernel is not an arm64 Image (bad magic)");
    }
    std::fs::write(&image, raw)?;
    Ok(image)
}

/// Unwrap a Linux arm64 kernel into the raw `Image` format VZ/libkrun expect.
/// Handles: raw Image (passthrough), plain gzip, and the EFI zboot container
/// (`MZ` + "zimg" header with an embedded gzip payload) that Alpine ships.
fn extract_arm64_image(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    fn gunzip(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        // Feed via temp file: piping both stdin and stdout deadlocks once the
        // decompressed output outgrows the pipe buffer.
        let tmp = std::env::temp_dir().join(format!("nebula-gunzip-{}.gz", std::process::id()));
        std::fs::write(&tmp, data)?;
        let out = Command::new("gzip")
            .arg("-dc")
            .stdin(std::fs::File::open(&tmp)?)
            .stderr(std::process::Stdio::null())
            .output();
        let _ = std::fs::remove_file(&tmp);
        let out = out?;
        if !out.status.success() || out.stdout.is_empty() {
            bail!("gunzip failed");
        }
        Ok(out.stdout)
    }

    if bytes.starts_with(&[0x1f, 0x8b]) {
        return gunzip(bytes);
    }
    if bytes.len() > 0x1c && &bytes[0..2] == b"MZ" && &bytes[4..8] == b"zimg" {
        let off = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let size = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let comp = &bytes[0x18..0x1c];
        if comp != b"gzip" {
            bail!(
                "zboot kernel uses unsupported compression {:?}",
                String::from_utf8_lossy(comp)
            );
        }
        if off + size > bytes.len() {
            bail!("zboot payload out of bounds");
        }
        return gunzip(&bytes[off..off + size]);
    }
    Ok(bytes.to_vec())
}

/// Assemble the spike initramfs: our static vessel-init as /init, the
/// /dev/console node the kernel needs to give PID 1 a stdio, and the
/// virtio_mmio module (stock kernels build it =m; libkrun devices are MMIO).
fn build_spike_initramfs(cache: &std::path::Path) -> anyhow::Result<PathBuf> {
    let init_bin = find_vessel_init()?;
    let mut b = InitramfsBuilder::new()
        .dir("/dev", 0o755)
        .char_dev("/dev/console", 5, 1, 0o600)
        .dir("/proc", 0o755)
        .dir("/sys", 0o755)
        .file("/init", std::fs::read(&init_bin)?, 0o755);
    if let Some(ko) = fetch_virtio_mmio_ko(cache)? {
        b = b.file("/virtio_mmio.ko", ko, 0o644);
    }
    let path = cache.join("spike-initramfs.cpio");
    std::fs::write(&path, b.build())?;
    Ok(path)
}

/// Pull virtio_mmio.ko out of Alpine's netboot initramfs (cached). Best-effort:
/// the spike still passes on VZ without it.
fn fetch_virtio_mmio_ko(cache: &std::path::Path) -> anyhow::Result<Option<Vec<u8>>> {
    let ko_path = cache.join("spike-virtio_mmio.ko");
    if ko_path.is_file() {
        return Ok(Some(std::fs::read(&ko_path)?));
    }
    let initramfs = cache.join("spike-alpine-initramfs-virt");
    if !initramfs.is_file() {
        let url = ALPINE_KERNEL_URL.replace("vmlinuz-virt", "initramfs-virt");
        let status = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&initramfs)
            .arg(&url)
            .status()?;
        if !status.success() {
            eprintln!("[spike] warn: could not download {url}; krun console will stay dark");
            return Ok(None);
        }
    }
    // bsdtar reads (compressed) cpio archives transparently.
    let out = Command::new("tar")
        .arg("-xOf")
        .arg(&initramfs)
        .arg("--include=*virtio_mmio.ko")
        .output()?;
    if !out.status.success() || out.stdout.is_empty() {
        eprintln!("[spike] warn: virtio_mmio.ko not found in Alpine initramfs");
        return Ok(None);
    }
    std::fs::write(&ko_path, &out.stdout)?;
    Ok(Some(out.stdout))
}

fn find_vessel_init() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("NEBULA_VESSEL_INIT") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        bail!("NEBULA_VESSEL_INIT={} does not exist", p.display());
    }
    // Walk up from the current exe (target/debug/nebula) to the target dir.
    let exe = std::env::current_exe()?;
    let target = exe
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "target"))
        .map(PathBuf::from);
    if let Some(target) = target {
        for profile in ["release", "debug"] {
            let p = target.join(MUSL_TARGET).join(profile).join("vessel-init");
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    bail!(
        "vessel-init (linux musl build) not found — run:\n  \
         cargo build -p vessel-init --release --target {MUSL_TARGET}"
    );
}
