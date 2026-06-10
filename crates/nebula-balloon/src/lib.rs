//! Elastic memory policy: the guest keeps only what its workloads need.
//!
//! VZ traditional-balloon semantics: `target` is the memory the guest may
//! keep; lowering it inflates the balloon (pages return to the host),
//! raising it deflates. The controller's contract:
//!
//! - **Deflate fast.** Guest pressure (low available or PSI) immediately
//!   raises the target — workloads must never OOM because of the balloon.
//! - **Inflate slow.** Only reclaim after a sustained surplus, in bounded
//!   steps, with hysteresis — no thrash on sawtooth workloads.
//!
//! Pure state machine: time and measurements come in, a new target (maybe)
//! comes out. All policy is unit-testable without a VM.

#[derive(Debug, Clone)]
pub struct Config {
    /// Hard ceiling: the VM's configured memory (MiB).
    pub max_mib: u64,
    /// Floor: kernel + system daemons need this much (MiB).
    pub min_mib: u64,
    /// Headroom kept above measured workload use (MiB).
    pub headroom_mib: u64,
    /// Available-memory floor that triggers an emergency deflate (MiB).
    pub low_water_mib: u64,
    /// PSI some avg10 (%) above which we deflate.
    pub psi_deflate_threshold: f64,
    /// Ticks of sustained surplus before we inflate (reclaim).
    pub inflate_after_ticks: u32,
    /// Largest single reclaim step, as a fraction of the current target.
    pub max_inflate_step: f64,
    /// Ignore target changes smaller than this (MiB).
    pub deadband_mib: u64,
}

impl Config {
    pub fn for_max(max_mib: u64) -> Self {
        Config {
            max_mib,
            min_mib: 1024.min(max_mib),
            headroom_mib: 768,
            low_water_mib: 256,
            psi_deflate_threshold: 5.0,
            inflate_after_ticks: 8,
            max_inflate_step: 0.25,
            deadband_mib: 256,
        }
    }
}

/// One tick's worth of guest measurements.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub total_mib: u64,
    pub available_mib: u64,
    pub psi_some_avg10: Option<f64>,
}

#[derive(Debug)]
pub struct Controller {
    cfg: Config,
    /// What we last asked the guest to keep (MiB).
    target_mib: u64,
    surplus_ticks: u32,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    /// Set the balloon target to this many MiB.
    SetTarget(u64),
    /// Leave things alone.
    Hold,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        let target = cfg.max_mib;
        Self {
            cfg,
            target_mib: target,
            surplus_ticks: 0,
        }
    }

    pub fn target_mib(&self) -> u64 {
        self.target_mib
    }

    /// Current balloon size (memory reclaimed from the guest), MiB.
    pub fn balloon_mib(&self) -> u64 {
        self.cfg.max_mib.saturating_sub(self.target_mib)
    }

    pub fn tick(&mut self, s: Sample) -> Action {
        // Workload use, excluding pages the balloon already holds: the guest
        // reports MemTotal for the full configured size; balloon pages are
        // gone from `available`, so measured use must be corrected by them.
        let balloon = self.balloon_mib();
        let used = s
            .total_mib
            .saturating_sub(s.available_mib)
            .saturating_sub(balloon);

        let pressured = s.available_mib < self.cfg.low_water_mib
            || s.psi_some_avg10.unwrap_or(0.0) > self.cfg.psi_deflate_threshold;

        if pressured {
            // Emergency deflate: give everything back at once. Correctness
            // beats elegance here; we re-shrink later when calm returns.
            self.surplus_ticks = 0;
            if self.target_mib < self.cfg.max_mib {
                self.target_mib = self.cfg.max_mib;
                return Action::SetTarget(self.target_mib);
            }
            return Action::Hold;
        }

        let desired = (used + self.cfg.headroom_mib).clamp(self.cfg.min_mib, self.cfg.max_mib);

        if desired > self.target_mib {
            // Workload grew: deflate to fit immediately (with headroom).
            self.surplus_ticks = 0;
            self.target_mib = desired;
            return Action::SetTarget(self.target_mib);
        }

        // Surplus: only reclaim after it persists, stepwise, with deadband.
        if self.target_mib.saturating_sub(desired) < self.cfg.deadband_mib {
            self.surplus_ticks = 0;
            return Action::Hold;
        }
        self.surplus_ticks += 1;
        if self.surplus_ticks < self.cfg.inflate_after_ticks {
            return Action::Hold;
        }
        self.surplus_ticks = 0;
        let max_step = ((self.target_mib as f64) * self.cfg.max_inflate_step) as u64;
        let new_target = self.target_mib.saturating_sub(max_step).max(desired);
        self.target_mib = new_target;
        Action::SetTarget(new_target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::for_max(32 * 1024)
    }

    fn idle_sample() -> Sample {
        Sample {
            total_mib: 32 * 1024,
            available_mib: 31 * 1024,
            psi_some_avg10: Some(0.0),
        }
    }

    #[test]
    fn idle_guest_shrinks_gradually_to_floor() {
        let mut c = Controller::new(cfg());
        let mut sets = 0;
        let mut last = c.target_mib();
        for _ in 0..400 {
            // As the balloon inflates, available shrinks accordingly.
            let s = Sample {
                total_mib: 32 * 1024,
                available_mib: c.target_mib() - 700, // ~700MiB of real use
                psi_some_avg10: Some(0.0),
            };
            if let Action::SetTarget(t) = c.tick(s) {
                assert!(t < last, "shrink must be monotonic while idle");
                last = t;
                sets += 1;
            }
        }
        assert!(sets > 3, "needs multiple bounded steps");
        // 700 used + 768 headroom, bounded below by min and deadband slack.
        assert!(
            c.target_mib() <= 1024 + 768,
            "should approach floor, got {}",
            c.target_mib()
        );
    }

    #[test]
    fn no_reclaim_before_sustained_surplus() {
        let mut c = Controller::new(cfg());
        for _ in 0..7 {
            assert_eq!(c.tick(idle_sample()), Action::Hold);
        }
        assert!(matches!(c.tick(idle_sample()), Action::SetTarget(_)));
    }

    #[test]
    fn pressure_deflates_to_max_instantly() {
        let mut c = Controller::new(cfg());
        for _ in 0..50 {
            c.tick(idle_sample());
        }
        assert!(c.target_mib() < 32 * 1024);
        let s = Sample {
            total_mib: 32 * 1024,
            available_mib: 100,
            psi_some_avg10: Some(0.0),
        };
        assert_eq!(c.tick(s), Action::SetTarget(32 * 1024));
    }

    #[test]
    fn psi_alone_triggers_deflate() {
        let mut c = Controller::new(cfg());
        for _ in 0..50 {
            c.tick(idle_sample());
        }
        let s = Sample {
            total_mib: 32 * 1024,
            available_mib: 4 * 1024,
            psi_some_avg10: Some(20.0),
        };
        assert_eq!(c.tick(s), Action::SetTarget(32 * 1024));
    }

    #[test]
    fn growth_deflates_immediately_without_waiting() {
        let mut c = Controller::new(cfg());
        for _ in 0..200 {
            let s = Sample {
                total_mib: 32 * 1024,
                available_mib: c.target_mib() - 700,
                psi_some_avg10: Some(0.0),
            };
            c.tick(s);
        }
        let shrunk = c.target_mib();
        assert!(shrunk < 4096);
        // Workload jumps to 6 GiB used (still > low_water available).
        let s = Sample {
            total_mib: 32 * 1024,
            available_mib: shrunk.saturating_sub(6 * 1024).max(300),
            psi_some_avg10: Some(0.0),
        };
        match c.tick(s) {
            Action::SetTarget(t) => assert!(t > shrunk, "must grow, got {t}"),
            Action::Hold => panic!("must react to growth"),
        }
    }

    #[test]
    fn small_fluctuations_hold_steady() {
        let mut c = Controller::new(Config {
            deadband_mib: 512,
            ..cfg()
        });
        for _ in 0..200 {
            let s = Sample {
                total_mib: 32 * 1024,
                available_mib: c.target_mib() - 700,
                psi_some_avg10: Some(0.0),
            };
            c.tick(s);
        }
        let settled = c.target_mib();
        // +-100 MiB wiggle inside the deadband: no churn.
        for i in 0..50 {
            let wiggle = if i % 2 == 0 { 100 } else { 0 };
            let s = Sample {
                total_mib: 32 * 1024,
                available_mib: c.target_mib() - 700 - wiggle,
                psi_some_avg10: Some(0.0),
            };
            assert_eq!(c.tick(s), Action::Hold);
        }
        assert_eq!(c.target_mib(), settled);
    }

    #[test]
    fn never_exceeds_bounds() {
        let mut c = Controller::new(cfg());
        for i in 0..1000 {
            let s = Sample {
                total_mib: 32 * 1024,
                available_mib: (i * 37) % (30 * 1024),
                psi_some_avg10: Some(((i % 17) as f64) * 1.3),
            };
            c.tick(s);
            assert!(c.target_mib() >= 1024);
            assert!(c.target_mib() <= 32 * 1024);
        }
    }
}
