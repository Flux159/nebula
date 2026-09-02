//! `<NEBULA_HOME>/run/instance.json` — what this daemon is, and how the last
//! one ended.
//!
//! `nebulad starting` used to be the only line the daemon ever wrote about its
//! own lifecycle: a clean `nebula down`, a signal, and a crash all looked like
//! a gap followed by another `starting` (issue #23). The record here closes
//! that: the running daemon publishes its identity and effective ports, and
//! every exit path stamps a reason into it, so the *next* start can say
//! whether the previous one ended cleanly — the half that survives even a
//! `kill -9`, which by definition cannot log anything itself.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::ports::PortPlan;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceRecord {
    pub pid: u32,
    pub version: String,
    pub home: PathBuf,
    /// Unix seconds; the log has timestamps but this file is read by tools.
    pub started_at: u64,
    pub ports: Ports,
    /// `None` while running — which is exactly what makes an unclean exit
    /// detectable on the next start.
    #[serde(default)]
    pub exit: Option<ExitRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ports {
    pub api_host: String,
    pub api_port: u16,
    pub dns_port: u16,
    pub k8s_port: u16,
    pub dns_zone: String,
}

impl From<&PortPlan> for Ports {
    fn from(p: &PortPlan) -> Self {
        Self {
            api_host: p.api_host.clone(),
            api_port: p.api_port,
            dns_port: p.dns_port,
            k8s_port: p.k8s_port,
            dns_zone: p.dns_zone.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitRecord {
    /// Stable, greppable: `down`, `signal`, `vessel-died`, `startup-error`,
    /// `fatal`, `listener-closed`.
    pub reason: String,
    #[serde(default)]
    pub detail: Option<String>,
    pub at: u64,
    pub uptime_secs: u64,
    #[serde(default)]
    pub containers: Option<usize>,
    #[serde(default)]
    pub vessels_running: Option<usize>,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn path(paths: &Paths) -> PathBuf {
    paths.run_dir().join("instance.json")
}

pub fn read(file: &Path) -> Option<InstanceRecord> {
    serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()
}

/// Publish this run's identity. Overwrites any previous record — it has
/// already been reported by [`report_previous_run`].
pub fn write_running(paths: &Paths, plan: &PortPlan) -> anyhow::Result<()> {
    let rec = InstanceRecord {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        home: paths.root.clone(),
        started_at: now_unix(),
        ports: plan.into(),
        exit: None,
    };
    write(&path(paths), &rec)
}

/// Stamp the exit reason into the record. Best effort by design: a daemon on
/// its way out must not fail to exit because it could not write a file.
pub fn record_exit(paths: &Paths, exit: ExitRecord) {
    let file = path(paths);
    let Some(mut rec) = read(&file) else { return };
    rec.exit = Some(exit);
    let _ = write(&file, &rec);
}

fn write(file: &Path, rec: &InstanceRecord) -> anyhow::Result<()> {
    // Write-rename: a half-written record reads as "no record", which is a
    // worse message but never a wrong one.
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(rec)?)?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

/// Log how the previous run ended, before this one takes the file over.
///
/// This is the line that makes a mystery restart legible: an INFO when the
/// last exit was deliberate, a WARN naming the pid and start time when it was
/// not. A hard kill leaves no exit record, so "no record" *is* the finding.
pub fn report_previous_run(paths: &Paths) {
    let Some(prev) = read(&path(paths)) else {
        return;
    };
    match &prev.exit {
        Some(e) => tracing::info!(
            previous_pid = prev.pid,
            previous_version = %prev.version,
            reason = %e.reason,
            detail = e.detail.as_deref().unwrap_or(""),
            uptime_secs = e.uptime_secs,
            "previous run exited cleanly"
        ),
        None => tracing::warn!(
            previous_pid = prev.pid,
            previous_version = %prev.version,
            started_at = prev.started_at,
            ran_secs = now_unix().saturating_sub(prev.started_at),
            "previous run did not shut down cleanly (no exit record) — it was killed, \
             crashed, or the host lost power"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_paths() -> (tempdir::Dir, Paths) {
        let dir = tempdir::Dir::new();
        let paths = Paths {
            root: dir.path().to_path_buf(),
        };
        paths.ensure_dirs().unwrap();
        (dir, paths)
    }

    fn plan() -> PortPlan {
        PortPlan {
            api_host: "127.0.0.1".into(),
            api_port: 7440,
            dns_port: 42053,
            k8s_port: 6443,
            dns_zone: "nebula.local".into(),
        }
    }

    #[test]
    fn round_trips_a_running_record() {
        let (_d, paths) = tmp_paths();
        write_running(&paths, &plan()).unwrap();
        let rec = read(&path(&paths)).unwrap();
        assert_eq!(rec.pid, std::process::id());
        assert_eq!(rec.ports.api_port, 7440);
        assert!(rec.exit.is_none(), "a running record has no exit stamp");
    }

    #[test]
    fn records_an_exit_reason() {
        let (_d, paths) = tmp_paths();
        write_running(&paths, &plan()).unwrap();
        record_exit(
            &paths,
            ExitRecord {
                reason: "signal".into(),
                detail: Some("SIGTERM".into()),
                at: now_unix(),
                uptime_secs: 12,
                containers: Some(3),
                vessels_running: Some(1),
            },
        );
        let e = read(&path(&paths)).unwrap().exit.unwrap();
        assert_eq!(e.reason, "signal");
        assert_eq!(e.detail.as_deref(), Some("SIGTERM"));
        assert_eq!(e.uptime_secs, 12);
        assert_eq!(e.containers, Some(3));
    }

    #[test]
    fn missing_record_is_not_an_error() {
        let (_d, paths) = tmp_paths();
        assert!(read(&path(&paths)).is_none());
        // Nothing to stamp; must not panic.
        record_exit(
            &paths,
            ExitRecord {
                reason: "down".into(),
                detail: None,
                at: now_unix(),
                uptime_secs: 0,
                containers: None,
                vessels_running: None,
            },
        );
        report_previous_run(&paths);
    }

    /// Minimal scratch directory (no dev-dependency for three tests).
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "nebulad-instance-test-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
