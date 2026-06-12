//! Process running with timeouts, tiny stats helpers, dependency-free dates.

use anyhow::Context;
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct CmdOut {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub elapsed: Duration,
}

impl CmdOut {
    pub fn ok(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
    pub fn brief_err(&self) -> String {
        if self.timed_out {
            return format!("timed out after {:.0?}", self.elapsed);
        }
        let tail: String = self
            .stderr
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        format!("exit {:?}: {}", self.code, tail)
    }
}

/// Run to completion with a hard timeout (kills the child). Stdout/stderr are
/// drained on threads so a chatty child can't deadlock on a full pipe.
pub fn run_cmd<S: AsRef<OsStr>>(
    bin: &Path,
    args: &[S],
    envs: &[(&str, String)],
    timeout: Duration,
) -> anyhow::Result<CmdOut> {
    let started = Instant::now();
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    let mut so = child.stdout.take().expect("piped");
    let mut se = child.stderr.take().expect("piped");
    let th_o = std::thread::spawn(move || {
        let mut b = Vec::new();
        so.read_to_end(&mut b).ok();
        b
    });
    let th_e = std::thread::spawn(move || {
        let mut b = Vec::new();
        se.read_to_end(&mut b).ok();
        b
    });
    let mut timed_out = false;
    let code = loop {
        if let Some(st) = child.try_wait()? {
            break st.code();
        }
        if started.elapsed() > timeout {
            timed_out = true;
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(CmdOut {
        code,
        stdout: String::from_utf8_lossy(&th_o.join().unwrap_or_default()).into_owned(),
        stderr: String::from_utf8_lossy(&th_e.join().unwrap_or_default()).into_owned(),
        timed_out,
        elapsed: started.elapsed(),
    })
}

pub fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let m = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[m - 1] + v[m]) / 2.0
    } else {
        v[m]
    }
}

/// Least-squares slope of y over index 0..n. Used for drift detection.
pub fn slope(ys: &[f64]) -> f64 {
    let n = ys.len() as f64;
    if ys.len() < 2 {
        return 0.0;
    }
    let mx = (n - 1.0) / 2.0;
    let my = ys.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (i, y) in ys.iter().enumerate() {
        let dx = i as f64 - mx;
        num += dx * (y - my);
        den += dx * dx;
    }
    if den == 0.0 {
        0.0
    } else {
        num / den
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// UTC `YYYYMMDD-HHMMSS` without a chrono dependency (Hinnant civil-from-days).
pub fn utc_stamp() -> String {
    let secs = unix_now() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_shape() {
        let s = utc_stamp();
        assert_eq!(s.len(), 15);
        assert!(s.starts_with("20"));
    }

    #[test]
    fn median_and_slope() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert!((slope(&[0.0, 1.0, 2.0, 3.0]) - 1.0).abs() < 1e-9);
        assert!(slope(&[5.0, 5.0, 5.0]).abs() < 1e-9);
    }
}
