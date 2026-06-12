//! Driver around the `nebula` CLI + REST API. Everything the scenarios do to
//! the engine goes through here, with timeouts on every call.

use crate::api;
use crate::util::{run_cmd, CmdOut};
use anyhow::{bail, Context};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct Nebula {
    pub bin: PathBuf,
    pub api_port: u16,
}

const SHORT: Duration = Duration::from_secs(30);
const LONG: Duration = Duration::from_secs(180);

impl Nebula {
    pub fn locate(flag: Option<PathBuf>, api_port: u16) -> anyhow::Result<Self> {
        let bin = if let Some(p) = flag {
            if !p.exists() {
                bail!("--nebula {} does not exist", p.display());
            }
            p
        } else {
            // Dev default: the `nebula` binary built beside us in target/debug.
            let sibling = std::env::current_exe().ok().and_then(|exe| {
                let s = exe.with_file_name(format!("nebula{}", std::env::consts::EXE_SUFFIX));
                s.exists().then_some(s)
            });
            sibling.unwrap_or_else(|| PathBuf::from("nebula")) // PATH fallback
        };
        Ok(Self { bin, api_port })
    }

    pub fn run<S: AsRef<std::ffi::OsStr>>(
        &self,
        args: &[S],
        timeout: Duration,
    ) -> anyhow::Result<CmdOut> {
        run_cmd(&self.bin, args, &[], timeout)
    }

    /// down --force; up; wait for agent-healthy. Fresh boot = deterministic
    /// numbers (same contract as the phase test scripts).
    pub fn fresh_up(&self) -> anyhow::Result<()> {
        self.run(&["down", "--force"], SHORT).ok();
        std::thread::sleep(Duration::from_secs(1));
        let up = self.run(&["up"], LONG)?;
        if !up.ok() {
            bail!("nebula up failed: {}", up.brief_err());
        }
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(60) {
            if self.healthy() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        bail!("engine not healthy 60s after `nebula up` returned")
    }

    pub fn stats(&self) -> anyhow::Result<api::Stats> {
        api::get_stats(self.api_port)
    }

    /// Agent healthy = status API answers with a running VM and agent block,
    /// and a guest exec round-trips.
    pub fn healthy(&self) -> bool {
        let api_ok = api::get_status(self.api_port)
            .map(|s| {
                s.get("vmState").and_then(|v| v.as_str()) == Some("Running")
                    && s.get("agent").map(|a| !a.is_null()).unwrap_or(false)
            })
            .unwrap_or(false);
        api_ok && self.exec_ok("true")
    }

    pub fn exec_ok(&self, cmd: &str) -> bool {
        self.run(&["exec", "sh", "-c", cmd], SHORT)
            .map(|o| o.ok())
            .unwrap_or(false)
    }

    pub fn exec_out(&self, cmd: &str) -> anyhow::Result<String> {
        let o = self.run(&["exec", "sh", "-c", cmd], SHORT)?;
        if o.timed_out {
            bail!("exec timed out: {cmd}");
        }
        Ok(o.stdout)
    }

    /// Agent-healthy ≠ engine-ready: dockerd/slimd come up seconds after the
    /// agent. Poll `docker version` like the phase scripts do.
    pub fn wait_docker(&self, timeout: Duration) -> anyhow::Result<()> {
        let t0 = Instant::now();
        loop {
            if self
                .docker(&["version"], SHORT)
                .map(|o| o.ok())
                .unwrap_or(false)
            {
                return Ok(());
            }
            if t0.elapsed() > timeout {
                bail!("container engine not answering {:?} after up", timeout);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// One-off docker command via `nebula docker` (env overrides, never
    /// touches the user's contexts; works against full dockerd and slimd).
    pub fn docker<S: AsRef<std::ffi::OsStr>>(
        &self,
        args: &[S],
        timeout: Duration,
    ) -> anyhow::Result<CmdOut> {
        let mut full: Vec<std::ffi::OsString> = vec!["docker".into()];
        full.extend(args.iter().map(|a| a.as_ref().to_owned()));
        self.run(&full, timeout)
    }

    /// Cumulative guest OOM-kill count from dmesg. Scenarios diff this across
    /// a phase instead of parsing timestamps.
    pub fn oom_count(&self) -> u64 {
        self.exec_out("dmesg 2>/dev/null | grep -ci 'out of memory\\|oom-kill' ; true")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Names of running bt- containers (our naming convention; never touches
    /// anything we didn't create).
    pub fn bt_containers(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let o = self.docker(
            &[
                "ps",
                "--filter",
                &format!("name={prefix}"),
                "--format",
                "{{.Names}}",
            ],
            SHORT,
        )?;
        if !o.ok() {
            bail!("docker ps failed: {}", o.brief_err());
        }
        Ok(o.stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// Remove all bt-prefixed containers, in chunks (arg-list limits).
    pub fn cleanup_containers(&self, prefix: &str) {
        let all = self
            .docker(
                &[
                    "ps",
                    "-a",
                    "--filter",
                    &format!("name={prefix}"),
                    "--format",
                    "{{.Names}}",
                ],
                SHORT,
            )
            .map(|o| {
                o.stdout
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for chunk in all.chunks(50) {
            let mut args: Vec<String> = vec!["rm".into(), "-f".into()];
            args.extend(chunk.iter().cloned());
            self.docker(&args, Duration::from_secs(120)).ok();
        }
    }

    pub fn vessels<S: AsRef<std::ffi::OsStr>>(
        &self,
        args: &[S],
        timeout: Duration,
    ) -> anyhow::Result<CmdOut> {
        let mut full: Vec<std::ffi::OsString> = vec!["vessels".into()];
        full.extend(args.iter().map(|a| a.as_ref().to_owned()));
        self.run(&full, timeout)
    }

    /// bt-v* vessel names from `vessels ls` (skip header + engine vessel).
    pub fn bt_vessels(&self) -> Vec<String> {
        self.vessels(&["ls"], SHORT)
            .map(|o| {
                o.stdout
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_whitespace().next())
                    .filter(|n| n.starts_with("bt-v"))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn cleanup_vessels(&self) {
        for name in self.bt_vessels() {
            self.vessels(&["rm", "--force", &name], Duration::from_secs(60))
                .ok();
        }
    }

    /// Pull images once per engine so pull time never pollutes start latency.
    pub fn pre_pull(&self, images: &[&str]) -> anyhow::Result<()> {
        for img in images {
            let o = self.docker(&["pull", img], Duration::from_secs(300))?;
            if !o.ok() {
                bail!("pre-pull {img} failed: {}", o.brief_err());
            }
        }
        Ok(())
    }
}

/// Where results land: `<out_root>/<stamp>-<host>-<scenario>/`.
pub fn run_dir(out_root: &Path, scenario: &str) -> anyhow::Result<PathBuf> {
    let host = crate::hostmem::host_meta()
        .get("hostname")
        .and_then(|h| h.as_str())
        .unwrap_or("unknown")
        .to_string();
    let dir = out_root.join(format!("{}-{host}-{scenario}", crate::util::utc_stamp()));
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    Ok(dir)
}
