//! Background stats sampler: every scenario gets a full timeseries trace of
//! balloon/footprint/pressure for free, labeled with the current phase.

use crate::{api, hostmem};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Sampler {
    stop: Arc<AtomicBool>,
    label: Arc<Mutex<String>>,
    handle: Option<std::thread::JoinHandle<Summary>>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Summary {
    pub samples: u64,
    pub peak_host_fp_mib: u64,
    pub min_balloon_held_mib: u64,
}

pub const CSV_HEADER: &str = "ts_unix,elapsed_s,phase,guest_total_mib,guest_used_mib,guest_avail_mib,balloon_target_mib,balloon_held_mib,max_mib,host_fp_mib,psi_some,psi_full,host_free_mib";

impl Sampler {
    pub fn start(api_port: u16, csv_path: &Path, interval: Duration) -> anyhow::Result<Self> {
        let mut f = std::fs::File::create(csv_path)?;
        writeln!(f, "{CSV_HEADER}")?;
        let stop = Arc::new(AtomicBool::new(false));
        let label = Arc::new(Mutex::new(String::from("start")));
        let (stop2, label2) = (stop.clone(), label.clone());
        let handle = std::thread::Builder::new()
            .name("bt-sampler".into())
            .spawn(move || {
                let t0 = Instant::now();
                let mut sum = Summary {
                    min_balloon_held_mib: u64::MAX,
                    ..Default::default()
                };
                while !stop2.load(Ordering::Relaxed) {
                    if let Ok(s) = api::get_stats(api_port) {
                        let phase = label2.lock().map(|g| g.clone()).unwrap_or_default();
                        let host_free = hostmem::host_free_mib().unwrap_or(0);
                        let row = format!(
                            "{},{:.1},{},{},{},{},{},{},{},{},{:.2},{:.2},{}",
                            crate::util::unix_now(),
                            t0.elapsed().as_secs_f64(),
                            phase,
                            s.guest.as_ref().map(|g| g.total_kib / 1024).unwrap_or(0),
                            s.guest_used_mib(),
                            s.guest_avail_mib(),
                            s.balloon_target_mib,
                            s.held_mib(),
                            s.max_mib,
                            s.host_footprint_mib,
                            s.psi_some(),
                            s.guest
                                .as_ref()
                                .and_then(|g| g.psi_full_avg10)
                                .unwrap_or(0.0),
                            host_free,
                        );
                        writeln!(f, "{row}").ok();
                        f.flush().ok();
                        sum.samples += 1;
                        sum.peak_host_fp_mib = sum.peak_host_fp_mib.max(s.host_footprint_mib);
                        sum.min_balloon_held_mib = sum.min_balloon_held_mib.min(s.held_mib());
                    }
                    // Engine restarts mid-scenario are normal; failed polls
                    // are simply skipped rows.
                    std::thread::sleep(interval);
                }
                if sum.min_balloon_held_mib == u64::MAX {
                    sum.min_balloon_held_mib = 0;
                }
                sum
            })?;
        Ok(Self {
            stop,
            label,
            handle: Some(handle),
        })
    }

    /// Tag subsequent rows (e.g. "maxram=8192/nginx/batch=12").
    pub fn set_phase(&self, phase: &str) {
        if let Ok(mut g) = self.label.lock() {
            *g = phase.to_string();
        }
    }

    pub fn stop(mut self) -> Summary {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}
