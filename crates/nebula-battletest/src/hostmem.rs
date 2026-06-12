//! Host-side memory probes + run metadata. Shell-outs, not syscalls — this is
//! a test harness; portability beats elegance. The *guest/VM* numbers come
//! from nebulad's stats API, which already does per-platform accounting.

use crate::util::run_cmd;
use std::path::Path;
use std::time::Duration;

const T: Duration = Duration::from_secs(10);

/// Reclaimable host RAM in MiB. Used only as a safety floor (stop sweeps
/// before wedging the machine), so "roughly right" is fine.
pub fn host_free_mib() -> Option<u64> {
    imp::host_free_mib()
}

pub fn host_total_mib() -> Option<u64> {
    imp::host_total_mib()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    pub fn host_free_mib() -> Option<u64> {
        let out = run_cmd(Path::new("/usr/bin/vm_stat"), &[] as &[&str], &[], T).ok()?;
        if !out.ok() {
            return None;
        }
        let page_size: u64 = out
            .stdout
            .lines()
            .next()?
            .split("page size of ")
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        let mut pages = 0u64;
        for line in out.stdout.lines() {
            for key in [
                "Pages free:",
                "Pages inactive:",
                "Pages speculative:",
                "Pages purgeable:",
            ] {
                if let Some(v) = line.strip_prefix(key) {
                    pages += v.trim().trim_end_matches('.').parse::<u64>().unwrap_or(0);
                }
            }
        }
        Some(pages * page_size / (1024 * 1024))
    }

    pub fn host_total_mib() -> Option<u64> {
        let out = run_cmd(Path::new("/usr/sbin/sysctl"), &["-n", "hw.memsize"], &[], T).ok()?;
        out.stdout
            .trim()
            .parse::<u64>()
            .ok()
            .map(|b| b / (1024 * 1024))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn host_free_mib() -> Option<u64> {
        meminfo("MemAvailable:")
    }

    pub fn host_total_mib() -> Option<u64> {
        meminfo("MemTotal:")
    }

    fn meminfo(key: &str) -> Option<u64> {
        let m = std::fs::read_to_string("/proc/meminfo").ok()?;
        m.lines().find_map(|l| {
            l.strip_prefix(key)?
                .trim()
                .trim_end_matches(" kB")
                .parse::<u64>()
                .ok()
                .map(|kib| kib / 1024)
        })
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    pub fn host_free_mib() -> Option<u64> {
        ps_kib("(Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory")
    }

    pub fn host_total_mib() -> Option<u64> {
        ps_kib("(Get-CimInstance Win32_OperatingSystem).TotalVisibleMemorySize")
    }

    fn ps_kib(expr: &str) -> Option<u64> {
        let out = run_cmd(
            Path::new("powershell"),
            &["-NoProfile", "-Command", expr],
            &[],
            T,
        )
        .ok()?;
        out.stdout.trim().parse::<u64>().ok().map(|kib| kib / 1024)
    }
}

/// Metadata stamped into every run's meta.json so results are comparable.
pub fn host_meta() -> serde_json::Value {
    let sh = |bin: &str, args: &[&str]| -> Option<String> {
        run_cmd(Path::new(bin), args, &[], T)
            .ok()
            .filter(|o| o.ok())
            .map(|o| o.stdout.trim().to_string())
    };
    let hostname = sh("hostname", &["-s"]).or_else(|| std::env::var("COMPUTERNAME").ok());
    #[cfg(target_os = "macos")]
    let (chip, os) = (
        sh("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"]),
        sh("/usr/bin/sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}")),
    );
    #[cfg(target_os = "linux")]
    let (chip, os) = (
        std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|c| {
            c.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        }),
        sh("uname", &["-sr"]),
    );
    #[cfg(windows)]
    let (chip, os) = (
        sh(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "(Get-CimInstance Win32_Processor).Name",
            ],
        ),
        sh(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "[System.Environment]::OSVersion.VersionString",
            ],
        ),
    );
    let git_sha = sh("git", &["rev-parse", "--short", "HEAD"]);
    serde_json::json!({
        "hostname": hostname,
        "chip": chip,
        "os": os,
        "arch": std::env::consts::ARCH,
        "host_total_mib": host_total_mib(),
        "nebula_git_sha": git_sha,
    })
}
