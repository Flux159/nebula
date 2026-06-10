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
    /// Ticks to sit out after any resize: /proc/meminfo is skewed while the
    /// guest moves balloon pages, and acting on those samples causes churn.
    pub cooldown_ticks: u32,
    /// Ignore target changes smaller than this (MiB).
    pub deadband_mib: u64,
}

impl Config {
    pub fn for_max(max_mib: u64) -> Self {
        Config {
            max_mib,
            min_mib: 1024.min(max_mib),
            // Generous slack: tight headroom turns routine page-cache and
            // k3s churn into pressure events and the balloon thrashes
            // (observed: settle -> avail dips -> deflate-to-max -> reshrink,
            // every couple of minutes). ~6% of max, at least 1.5 GiB.
            headroom_mib: (max_mib / 16).max(1536),
            low_water_mib: 384,
            psi_deflate_threshold: 10.0,
            inflate_after_ticks: 45,
            cooldown_ticks: 5,
            deadband_mib: 512,
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
    cooldown: u32,
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
            cooldown: 0,
        }
    }

    fn set_target(&mut self, target: u64) -> Action {
        self.surplus_ticks = 0;
        self.cooldown = self.cfg.cooldown_ticks;
        self.target_mib = target;
        Action::SetTarget(target)
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
            // Graduated emergency deflate: double the allowance each tick
            // while pressure persists (1s ticks reach max within ~5s) instead
            // of slamming to max — a brief cache burst no longer swings the
            // guest by 30 GiB. DEFLATE_ON_OOM backstops the worst case.
            // Pressure overrides the cooldown: safety beats quiet.
            if self.target_mib < self.cfg.max_mib {
                let t = (self.target_mib * 2).min(self.cfg.max_mib);
                return self.set_target(t);
            }
            self.surplus_ticks = 0;
            return Action::Hold;
        }

        // Post-resize cooldown: meminfo lags while the guest moves balloon
        // pages; deciding on those samples causes spurious follow-ups.
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return Action::Hold;
        }

        let desired = (used + self.cfg.headroom_mib).clamp(self.cfg.min_mib, self.cfg.max_mib);

        if desired > self.target_mib + self.cfg.deadband_mib {
            // Workload grew materially: deflate to fit immediately (with
            // headroom). Sub-deadband wiggle is absorbed by the headroom —
            // resizing the guest for every 100 MiB breathing is thrash.
            return self.set_target(desired);
        }
        if desired > self.target_mib {
            // Within the deadband: tolerated, but don't count it as surplus.
            self.surplus_ticks = 0;
            return Action::Hold;
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
        // One jump straight to the steady-state target — a single resize per
        // workload change instead of a multi-minute staircase of steps.
        self.set_target(desired)
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

    /// Run until the controller settles (bounded), returning resize count.
    fn settle(c: &mut Controller, used_mib: u64, max_ticks: u32) -> u32 {
        let mut sets = 0;
        for _ in 0..max_ticks {
            let s = Sample {
                total_mib: 32 * 1024,
                available_mib: c.target_mib().saturating_sub(used_mib).max(600),
                psi_some_avg10: Some(0.0),
            };
            if let Action::SetTarget(_) = c.tick(s) {
                sets += 1;
            }
        }
        sets
    }

    #[test]
    fn idle_guest_reclaims_in_a_single_jump() {
        let mut c = Controller::new(cfg());
        let sets = settle(&mut c, 700, 120);
        assert_eq!(sets, 1, "one resize per workload change, not a staircase");
        // 700 used + 2048 headroom (min floor 1024).
        assert_eq!(c.target_mib(), 700 + 2048);
    }

    #[test]
    fn no_reclaim_before_sustained_surplus() {
        let mut c = Controller::new(cfg());
        for _ in 0..44 {
            assert_eq!(c.tick(idle_sample()), Action::Hold);
        }
        assert!(matches!(c.tick(idle_sample()), Action::SetTarget(_)));
    }

    #[test]
    fn settled_guest_stays_quiet() {
        let mut c = Controller::new(cfg());
        settle(&mut c, 700, 120);
        // 10 minutes of idle ticks with sub-deadband wiggle: zero resizes.
        for i in 0..600u64 {
            let wiggle = if i % 2 == 0 { 100 } else { 0 };
            let s = Sample {
                total_mib: 32 * 1024,
                available_mib: c.target_mib() - 700 - wiggle,
                psi_some_avg10: Some(0.0),
            };
            assert_eq!(c.tick(s), Action::Hold, "tick {i} must hold");
        }
    }

    #[test]
    fn sustained_pressure_escalates_to_max_quickly() {
        let mut c = Controller::new(cfg());
        settle(&mut c, 700, 120);
        let mut last = c.target_mib();
        assert!(last < 32 * 1024);
        for tick in 1..=6 {
            let s = Sample {
                total_mib: 32 * 1024,
                available_mib: 100,
                psi_some_avg10: Some(0.0),
            };
            match c.tick(s) {
                Action::SetTarget(t) => {
                    assert!(t > last, "tick {tick}: must grow");
                    last = t;
                }
                Action::Hold => break,
            }
        }
        assert_eq!(c.target_mib(), 32 * 1024);
    }

    #[test]
    fn brief_pressure_blip_does_not_slam_to_max() {
        let mut c = Controller::new(cfg());
        settle(&mut c, 700, 120);
        let shrunk = c.target_mib();
        let s = Sample {
            total_mib: 32 * 1024,
            available_mib: 100,
            psi_some_avg10: Some(0.0),
        };
        assert_eq!(c.tick(s), Action::SetTarget((shrunk * 2).min(32 * 1024)));
        assert!(c.target_mib() < 32 * 1024 / 2);
    }

    #[test]
    fn psi_alone_triggers_deflate() {
        let mut c = Controller::new(cfg());
        settle(&mut c, 700, 120);
        let before = c.target_mib();
        let s = Sample {
            total_mib: 32 * 1024,
            available_mib: 4 * 1024,
            psi_some_avg10: Some(20.0),
        };
        match c.tick(s) {
            Action::SetTarget(t) => assert!(t > before),
            Action::Hold => panic!("PSI pressure must act"),
        }
    }

    #[test]
    fn growth_deflates_immediately_without_waiting() {
        let mut c = Controller::new(cfg());
        settle(&mut c, 700, 120);
        let shrunk = c.target_mib();
        // 6 GiB of new use, still above the pressure floor.
        let s = Sample {
            total_mib: 32 * 1024,
            available_mib: shrunk.saturating_sub(6 * 1024).max(500),
            psi_some_avg10: Some(0.0),
        };
        match c.tick(s) {
            Action::SetTarget(t) => assert!(t > shrunk),
            Action::Hold => panic!("must react to growth"),
        }
    }

    #[test]
    fn cooldown_swallows_transition_skew() {
        let mut c = Controller::new(cfg());
        settle(&mut c, 700, 120);
        // Material growth triggers a resize…
        let shrunk = c.target_mib();
        let s = Sample {
            total_mib: 32 * 1024,
            available_mib: shrunk.saturating_sub(6 * 1024).max(500),
            psi_some_avg10: Some(0.0),
        };
        assert!(matches!(c.tick(s), Action::SetTarget(_)));
        // …then transition-skewed samples (absurd readings) hold for the
        // cooldown window instead of triggering follow-up resizes.
        for _ in 0..5 {
            let skewed = Sample {
                total_mib: 32 * 1024,
                available_mib: 31 * 1024, // looks suddenly empty
                psi_some_avg10: Some(0.0),
            };
            assert_eq!(c.tick(skewed), Action::Hold);
        }
    }

    #[test]
    fn never_exceeds_bounds() {
        let mut c = Controller::new(cfg());
        for i in 0..1000u64 {
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
