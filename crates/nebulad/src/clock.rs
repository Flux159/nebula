//! Guest clock health, written to the log from the host's side.
//!
//! Everything a server in the guest paces -- a game's movement ticks, a
//! database's timeouts -- runs off the guest kernel's clocksource and its
//! timer interrupts. When those go wrong nothing fails: the guest keeps
//! running, a little slow and in bursts, and the only report is "it lags" from
//! a machine that is idle. The reason is in the guest kernel's choices at
//! boot and in how fast its clock actually runs, and neither reaches any log
//! unless something asks. This asks.
//!
//! Once, after boot: the clocksource and clock event devices the guest
//! settled on, and what its kernel said about calibrating them. Then every
//! few minutes: how fast the guest clock ran against the host's, how long a
//! one-second sleep took, and how often each timer interrupt fired.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nebula_core::proto::{AgentRequest, AgentResponse};

use crate::vessel::Vessel;

/// Late enough that the guest has booted, early enough that a short session
/// still gets a report.
const FIRST_SAMPLE: Duration = Duration::from_secs(30);
const SECOND_SAMPLE: Duration = Duration::from_secs(60);
const INTERVAL: Duration = Duration::from_secs(300);

const BOOT_SCRIPT: &str = r#"
cs=/sys/devices/system/clocksource/clocksource0
echo "clocksource $(cat $cs/current_clocksource 2>/dev/null) (available: $(cat $cs/available_clocksource 2>/dev/null))"
for d in /sys/devices/system/clockevents/clockevent*; do
  echo "clockevent ${d##*/}: $(cat $d/current_device 2>/dev/null)"
done
echo "broadcast: $(cat /sys/devices/system/clockevents/broadcast/current_device 2>/dev/null)"
dmesg 2>/dev/null | grep -iE 'clocksource|tsc|apic timer|calibrat|verification|unstable|hz' | head -n 40
"#;

/// Uptime first, before anything slow, so the host's send time stands in for
/// the moment it was read.
const SAMPLE_SCRIPT: &str = r#"
cat /proc/uptime
grep -E '^ *(0|LOC):|arch_timer' /proc/interrupts
a=$(cut -d' ' -f1 /proc/uptime); sleep 1; b=$(cut -d' ' -f1 /proc/uptime)
echo "sleep $a $b"
"#;

pub fn start(vessel: Arc<Vessel>) {
    if std::env::var_os("NEBULA_NO_CLOCK_REPORT").is_some() {
        tracing::warn!("guest clock report disabled (NEBULA_NO_CLOCK_REPORT)");
        return;
    }
    std::thread::spawn(move || run(&vessel));
}

fn run(vessel: &Vessel) {
    std::thread::sleep(FIRST_SAMPLE);
    let mut boot_reported = false;
    let mut windows = 0u32;
    let mut last: Option<Sample> = None;
    loop {
        if !boot_reported {
            boot_reported = report_boot(vessel);
        }
        if let Some(sample) = take_sample(vessel) {
            if let Some(prev) = &last {
                log_window(&Window::between(prev, &sample));
                windows += 1;
            }
            last = Some(sample);
        }
        // The first window is short, so a brief session still gets a report.
        std::thread::sleep(match (&last, windows) {
            (None, _) => FIRST_SAMPLE,
            (Some(_), 0) => SECOND_SAMPLE,
            _ => INTERVAL,
        });
    }
}

fn report_boot(vessel: &Vessel) -> bool {
    let Some(out) = exec(vessel, BOOT_SCRIPT, Duration::from_secs(10)) else {
        return false;
    };
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        tracing::info!("guest timers: {line}");
    }
    true
}

fn take_sample(vessel: &Vessel) -> Option<Sample> {
    let host = Instant::now();
    let out = exec(vessel, SAMPLE_SCRIPT, Duration::from_secs(10))?;
    Sample::parse(host, &out)
}

fn exec(vessel: &Vessel, script: &str, timeout: Duration) -> Option<String> {
    let request = AgentRequest::Exec {
        cmd: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        timeout_ms: timeout.as_millis() as u64,
    };
    match vessel.agent_request_long(&request, timeout + Duration::from_secs(5)) {
        Ok(AgentResponse::Exec(r)) if !r.timed_out => Some(r.stdout),
        Ok(other) => {
            tracing::debug!("guest clock probe: unexpected response {other:?}");
            None
        }
        Err(e) => {
            tracing::debug!("guest clock probe failed: {e:#}");
            None
        }
    }
}

#[derive(Debug, Clone)]
struct Sample {
    host: Instant,
    uptime_secs: f64,
    /// IRQ 0 on x86: the PIT. Silent once the local APIC timer has taken
    /// over; busy when the guest could not use it.
    pit_irqs: Option<u64>,
    /// Local timer interrupts (x86 `LOC`, arm64 `arch_timer`), all CPUs.
    local_irqs: Option<u64>,
    /// How long `sleep 1` took by the guest's own clock, 10 ms resolution.
    sleep_secs: Option<f64>,
}

impl Sample {
    fn parse(host: Instant, out: &str) -> Option<Self> {
        let mut lines = out.lines();
        let uptime_secs = lines.next()?.split_whitespace().next()?.parse().ok()?;
        let mut sample = Sample {
            host,
            uptime_secs,
            pit_irqs: None,
            local_irqs: None,
            sleep_secs: None,
        };
        for line in lines {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("sleep ") {
                let mut t = rest.split_whitespace().map(str::parse::<f64>);
                if let (Some(Ok(a)), Some(Ok(b))) = (t.next(), t.next()) {
                    sample.sleep_secs = Some(b - a);
                }
            } else if let Some(rest) = line.strip_prefix("0:") {
                if line.contains("timer") {
                    sample.pit_irqs = Some(sum_counts(rest));
                }
            } else if let Some(rest) = line.strip_prefix("LOC:") {
                sample.local_irqs = Some(sum_counts(rest));
            } else if line.ends_with("arch_timer") {
                if let Some((_, rest)) = line.split_once(':') {
                    *sample.local_irqs.get_or_insert(0) += sum_counts(rest);
                }
            }
        }
        Some(sample)
    }
}

/// The per-CPU counts that open a `/proc/interrupts` row, summed.
fn sum_counts(row: &str) -> u64 {
    row.split_whitespace()
        .map_while(|t| t.parse::<u64>().ok())
        .sum()
}

#[derive(Debug, PartialEq)]
struct Window {
    host_secs: f64,
    /// Guest seconds per host second. 1.0 is right.
    rate: f64,
    pit_per_sec: Option<f64>,
    local_per_sec: Option<f64>,
    sleep_ms: Option<f64>,
}

impl Window {
    fn between(a: &Sample, b: &Sample) -> Self {
        let host_secs = b
            .host
            .duration_since(a.host)
            .as_secs_f64()
            .max(f64::EPSILON);
        let per_sec = |x: Option<u64>, y: Option<u64>| match (x, y) {
            (Some(x), Some(y)) => Some(y.saturating_sub(x) as f64 / host_secs),
            _ => None,
        };
        Window {
            host_secs,
            rate: (b.uptime_secs - a.uptime_secs) / host_secs,
            pit_per_sec: per_sec(a.pit_irqs, b.pit_irqs),
            local_per_sec: per_sec(a.local_irqs, b.local_irqs),
            sleep_ms: b.sleep_secs.map(|s| s * 1000.0),
        }
    }

    /// What is wrong, if anything a game server would feel.
    fn problems(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if (self.rate - 1.0).abs() > 0.01 {
            out.push("guest clock does not keep time with the host");
        }
        if self.sleep_ms.is_some_and(|ms| ms > 1_100.0) {
            out.push("guest timers fire late");
        }
        // Once booted, Linux only leaves the PIT running when it could not
        // use the local APIC timer, and then every timer runs off it.
        if self.pit_per_sec.is_some_and(|hz| hz > 20.0) {
            out.push("guest timers run off the emulated PIT");
        }
        out
    }
}

fn log_window(w: &Window) {
    let fmt = |v: Option<f64>| v.map_or_else(|| "-".to_string(), |v| format!("{v:.0}"));
    let summary = format!(
        "guest clock: {:.4}x host over {:.0}s, sleep(1s) took {}ms, timer irqs/s pit={} local={}",
        w.rate,
        w.host_secs,
        fmt(w.sleep_ms),
        fmt(w.pit_per_sec),
        fmt(w.local_per_sec),
    );
    let problems = w.problems();
    if problems.is_empty() {
        tracing::info!("{summary}");
    } else {
        tracing::warn!("{summary}: {}", problems.join("; "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const X86: &str = "\
120.50 400.00
  0:        117          0          0          0   IO-APIC   2-edge      timer
LOC:      52011      48870      50102      47999   Local timer interrupts
sleep 120.51 121.52
";

    #[test]
    fn parses_an_x86_sample() {
        let s = Sample::parse(Instant::now(), X86).unwrap();
        assert_eq!(s.uptime_secs, 120.50);
        assert_eq!(s.pit_irqs, Some(117));
        assert_eq!(s.local_irqs, Some(52011 + 48870 + 50102 + 47999));
        assert!((s.sleep_secs.unwrap() - 1.01).abs() < 1e-9);
    }

    #[test]
    fn parses_an_arm64_sample() {
        let out = "\
33.10 120.00
 11:       900        800     GICv3  27 Level     arch_timer
sleep 33.11 34.11
";
        let s = Sample::parse(Instant::now(), out).unwrap();
        assert_eq!(s.pit_irqs, None);
        assert_eq!(s.local_irqs, Some(1_700));
    }

    #[test]
    fn a_missing_uptime_is_no_sample() {
        assert!(Sample::parse(Instant::now(), "").is_none());
        assert!(Sample::parse(Instant::now(), "garbage\n").is_none());
    }

    fn sample(host: Instant, uptime: f64, pit: u64, sleep: f64) -> Sample {
        Sample {
            host,
            uptime_secs: uptime,
            pit_irqs: Some(pit),
            local_irqs: Some(0),
            sleep_secs: Some(sleep),
        }
    }

    #[test]
    fn a_healthy_guest_has_no_problems() {
        let t0 = Instant::now();
        let a = sample(t0, 100.0, 117, 1.0);
        let b = sample(t0 + Duration::from_secs(60), 160.01, 117, 1.01);
        let w = Window::between(&a, &b);
        assert!(w.problems().is_empty(), "{w:?}");
    }

    #[test]
    fn a_guest_on_pit_ticks_is_flagged() {
        let t0 = Instant::now();
        let a = sample(t0, 100.0, 1_000, 1.0);
        // 60 host seconds, 55 guest seconds, 1000 PIT interrupts a second,
        // and a one-second sleep that took a second and a half.
        let b = sample(t0 + Duration::from_secs(60), 155.0, 61_000, 1.5);
        let w = Window::between(&a, &b);
        assert_eq!(
            w.problems(),
            vec![
                "guest clock does not keep time with the host",
                "guest timers fire late",
                "guest timers run off the emulated PIT",
            ]
        );
    }
}
