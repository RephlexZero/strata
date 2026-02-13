use crate::config::LinkLifecycleConfig;
use crate::net::interface::LinkPhase;
use crate::scheduler::ewma::Ewma;
use std::sync::atomic::{AtomicI32, AtomicU64};
use std::sync::Mutex;
use std::time::Instant;

/// Smoothed statistics state for a single link's stats callback.
///
/// Updated by the librist stats callback at each interval (~100ms),
/// holding the EWMA filters for RTT, bandwidth, and loss, plus
/// counters for delta computation.
pub struct EwmaStats {
    pub rtt: Ewma,
    pub bandwidth: Ewma,
    pub loss: Ewma,
    pub last_sent: u64,
    pub last_lost: u64,
    pub last_rex: u64,
    pub last_stats_ms: u64,
}

impl EwmaStats {
    pub fn with_alpha(alpha: f64) -> Self {
        Self {
            rtt: Ewma::new(alpha),
            bandwidth: Ewma::new(alpha),
            loss: Ewma::new(alpha),
            last_sent: 0,
            last_lost: 0,
            last_rex: 0,
            last_stats_ms: 0,
        }
    }
}

impl Default for EwmaStats {
    fn default() -> Self {
        Self::with_alpha(0.125)
    }
}

/// Shared atomic state for a single network link.
///
/// Written by the librist stats callback (via `Arc`) and read by
/// [`LinkSender::get_metrics()`](crate::net::interface::LinkSender::get_metrics)
/// on the scheduler thread. Atomic fields avoid lock contention on the hot
/// path; the `ewma_state` mutex is taken only in the stats callback.
pub struct LinkStats {
    pub rtt: AtomicU64,
    pub bandwidth: AtomicU64,
    pub retransmitted: AtomicU64,
    pub sent: AtomicU64,
    pub lost: AtomicU64,
    pub smoothed_rtt_us: AtomicU64,
    pub smoothed_bw_bps: AtomicU64,
    pub smoothed_loss_permille: AtomicU64, // Stored as * 1000. 1000 = 100% loss.
    pub bytes_written: AtomicU64,
    pub last_stats_ms: AtomicU64,
    pub os_up_i32: AtomicI32, // -1 unknown, 0 down, 1 up
    pub mtu_i32: AtomicI32,   // -1 unknown
    pub os_last_poll_ms: AtomicU64,
    pub ewma_state: Mutex<EwmaStats>,
    pub lifecycle: Mutex<LinkLifecycle>,
}

impl LinkStats {
    pub fn new(lifecycle_config: LinkLifecycleConfig) -> Self {
        Self::with_ewma_alpha(lifecycle_config, 0.125)
    }

    pub fn with_ewma_alpha(lifecycle_config: LinkLifecycleConfig, ewma_alpha: f64) -> Self {
        Self {
            rtt: AtomicU64::new(0),
            bandwidth: AtomicU64::new(0),
            retransmitted: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            smoothed_rtt_us: AtomicU64::new(0),
            smoothed_bw_bps: AtomicU64::new(0),
            smoothed_loss_permille: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            last_stats_ms: AtomicU64::new(0),
            os_up_i32: AtomicI32::new(-1),
            mtu_i32: AtomicI32::new(-1),
            os_last_poll_ms: AtomicU64::new(0),
            ewma_state: Mutex::new(EwmaStats::with_alpha(ewma_alpha)),
            lifecycle: Mutex::new(LinkLifecycle::new(lifecycle_config)),
        }
    }
}

impl Default for LinkStats {
    fn default() -> Self {
        Self::new(LinkLifecycleConfig::default())
    }
}

/// Link lifecycle state machine.
///
/// Tracks phase transitions based on consecutive good/bad observations
/// and stats freshness. See [`LinkPhase`]
/// for the phase diagram.
pub struct LinkLifecycle {
    pub phase: LinkPhase,
    pub last_transition: Instant,
    pub last_good: Instant,
    pub last_bad: Instant,
    pub consecutive_good: u32,
    pub consecutive_bad: u32,
    pub config: LinkLifecycleConfig,
}

impl LinkLifecycle {
    pub fn new(config: LinkLifecycleConfig) -> Self {
        let now = Instant::now();
        Self {
            phase: LinkPhase::Init,
            last_transition: now,
            last_good: now,
            last_bad: now,
            consecutive_good: 0,
            consecutive_bad: 0,
            config,
        }
    }

    pub fn update(
        &mut self,
        now: Instant,
        rtt_ms: f64,
        loss_rate: f64,
        capacity_bps: f64,
        stats_age: std::time::Duration,
    ) -> LinkPhase {
        let fresh = stats_age < std::time::Duration::from_millis(self.config.stats_fresh_ms);
        let good = fresh
            && rtt_ms >= self.config.good_rtt_ms_min
            && loss_rate <= self.config.good_loss_rate_max
            && capacity_bps >= self.config.good_capacity_bps_min;

        if good {
            self.consecutive_good += 1;
            self.consecutive_bad = 0;
            self.last_good = now;
        } else {
            self.consecutive_bad += 1;
            self.consecutive_good = 0;
            self.last_bad = now;
        }

        let stats_stale = stats_age > std::time::Duration::from_millis(self.config.stats_stale_ms);

        let old_phase = self.phase;
        self.phase = match self.phase {
            LinkPhase::Init => {
                if fresh {
                    LinkPhase::Probe
                } else {
                    LinkPhase::Init
                }
            }
            LinkPhase::Probe => {
                if stats_stale {
                    LinkPhase::Reset
                } else if self.consecutive_good >= self.config.probe_to_warm_good {
                    LinkPhase::Warm
                } else {
                    LinkPhase::Probe
                }
            }
            LinkPhase::Warm => {
                if self.consecutive_good >= self.config.warm_to_live_good {
                    LinkPhase::Live
                } else if self.consecutive_bad >= self.config.warm_to_degrade_bad {
                    LinkPhase::Degrade
                } else {
                    LinkPhase::Warm
                }
            }
            LinkPhase::Live => {
                if stats_stale {
                    LinkPhase::Reset
                } else if self.consecutive_bad >= self.config.live_to_degrade_bad {
                    LinkPhase::Degrade
                } else {
                    LinkPhase::Live
                }
            }
            LinkPhase::Degrade => {
                if self.consecutive_good >= self.config.degrade_to_warm_good {
                    LinkPhase::Warm
                } else if self.consecutive_bad >= self.config.degrade_to_cooldown_bad {
                    LinkPhase::Cooldown
                } else {
                    LinkPhase::Degrade
                }
            }
            LinkPhase::Cooldown => {
                if now.duration_since(self.last_transition)
                    > std::time::Duration::from_millis(self.config.cooldown_ms)
                {
                    LinkPhase::Probe
                } else {
                    LinkPhase::Cooldown
                }
            }
            LinkPhase::Reset => {
                if fresh {
                    LinkPhase::Probe
                } else {
                    LinkPhase::Reset
                }
            }
        };

        // Record last_transition for all phases that carry traffic or
        // transition outward.  Cooldown and Reset use last_transition as
        // their entry timestamp (timer start), so we must NOT overwrite it
        // while *staying* in those phases, but we DO set it on the tick
        // that first enters them.
        let entered_cooldown_or_reset = (self.phase == LinkPhase::Cooldown
            || self.phase == LinkPhase::Reset)
            && old_phase != self.phase;

        if self.phase != LinkPhase::Cooldown
            && self.phase != LinkPhase::Reset
            && self.phase != LinkPhase::Init
        {
            self.last_transition = now;
        } else if entered_cooldown_or_reset {
            // First tick entering Cooldown/Reset — record entry time so the
            // cooldown timer measures from the correct instant.
            self.last_transition = now;
        }

        self.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn lifecycle_reaches_live_on_good_stats() {
        let mut lc = LinkLifecycle::new(LinkLifecycleConfig::default());
        let start = Instant::now();

        for i in 0..12 {
            let now = start + Duration::from_millis(i * 100);
            let phase = lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
            if i >= 10 {
                assert_eq!(phase, LinkPhase::Live);
            }
        }
    }

    #[test]
    fn lifecycle_degrades_on_bad_stats() {
        let mut lc = LinkLifecycle::new(LinkLifecycleConfig::default());
        let start = Instant::now();

        for i in 0..12 {
            let now = start + Duration::from_millis(i * 100);
            lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        }

        let mut phase = LinkPhase::Live;
        for i in 0..3 {
            let now = start + Duration::from_secs(2) + Duration::from_millis(i * 50);
            phase = lc.update(now, 200.0, 0.9, 100_000.0, Duration::from_millis(200));
        }
        assert_eq!(phase, LinkPhase::Degrade);
    }

    #[test]
    fn lifecycle_resets_on_stale_stats() {
        let mut lc = LinkLifecycle::new(LinkLifecycleConfig::default());
        let start = Instant::now();

        for i in 0..12 {
            let now = start + Duration::from_millis(i * 100);
            lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        }

        let now = start + Duration::from_secs(5);
        let phase = lc.update(now, 0.0, 1.0, 0.0, Duration::from_secs(5));
        assert_eq!(phase, LinkPhase::Reset);
    }

    #[test]
    fn lifecycle_init_to_probe_on_fresh_stats() {
        let mut lc = LinkLifecycle::new(LinkLifecycleConfig::default());
        let start = Instant::now();
        let phase = lc.update(start, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        assert_eq!(phase, LinkPhase::Probe);
    }

    #[test]
    fn lifecycle_init_stays_without_fresh_stats() {
        let mut lc = LinkLifecycle::new(LinkLifecycleConfig::default());
        let start = Instant::now();
        let phase = lc.update(start, 10.0, 0.01, 1_000_000.0, Duration::from_secs(5));
        assert_eq!(phase, LinkPhase::Init);
    }

    #[test]
    fn lifecycle_probe_to_warm() {
        let config = LinkLifecycleConfig {
            probe_to_warm_good: 3,
            ..LinkLifecycleConfig::default()
        };
        let mut lc = LinkLifecycle::new(config);
        let start = Instant::now();

        lc.update(start, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        assert_eq!(lc.phase, LinkPhase::Probe);

        for i in 1..=3 {
            let now = start + Duration::from_millis(i * 100);
            lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        }
        assert_eq!(lc.phase, LinkPhase::Warm);
    }

    #[test]
    fn lifecycle_probe_to_reset_on_stale() {
        let config = LinkLifecycleConfig {
            stats_stale_ms: 3000,
            ..LinkLifecycleConfig::default()
        };
        let mut lc = LinkLifecycle::new(config);
        let start = Instant::now();

        lc.update(start, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        assert_eq!(lc.phase, LinkPhase::Probe);

        let phase = lc.update(
            start + Duration::from_secs(1),
            0.0,
            1.0,
            0.0,
            Duration::from_secs(5),
        );
        assert_eq!(phase, LinkPhase::Reset);
    }

    #[test]
    fn lifecycle_degrade_to_warm_on_recovery() {
        let config = LinkLifecycleConfig {
            degrade_to_warm_good: 5,
            ..LinkLifecycleConfig::default()
        };
        let mut lc = LinkLifecycle::new(config);
        let start = Instant::now();

        for i in 0..12 {
            let now = start + Duration::from_millis(i * 100);
            lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        }
        assert_eq!(lc.phase, LinkPhase::Live);

        for i in 0..3 {
            let now = start + Duration::from_secs(2) + Duration::from_millis(i * 50);
            lc.update(now, 200.0, 0.9, 100_000.0, Duration::from_millis(200));
        }
        assert_eq!(lc.phase, LinkPhase::Degrade);

        for i in 0..5 {
            let now = start + Duration::from_secs(3) + Duration::from_millis(i * 100);
            lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        }
        assert_eq!(lc.phase, LinkPhase::Warm);
    }

    #[test]
    fn lifecycle_degrade_to_cooldown_on_persistent_bad() {
        let config = LinkLifecycleConfig {
            degrade_to_cooldown_bad: 10,
            ..LinkLifecycleConfig::default()
        };
        let mut lc = LinkLifecycle::new(config);
        let start = Instant::now();

        for i in 0..12 {
            let now = start + Duration::from_millis(i * 100);
            lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        }
        assert_eq!(lc.phase, LinkPhase::Live);

        for i in 0..3 {
            let now = start + Duration::from_secs(2) + Duration::from_millis(i * 50);
            lc.update(now, 200.0, 0.9, 100_000.0, Duration::from_millis(200));
        }
        assert_eq!(lc.phase, LinkPhase::Degrade);

        for i in 0..10 {
            let now = start + Duration::from_secs(3) + Duration::from_millis(i * 50);
            lc.update(now, 200.0, 0.9, 100_000.0, Duration::from_millis(200));
        }
        assert_eq!(lc.phase, LinkPhase::Cooldown);
    }

    #[test]
    fn lifecycle_cooldown_to_probe_after_timeout() {
        let config = LinkLifecycleConfig {
            degrade_to_cooldown_bad: 10,
            cooldown_ms: 2000,
            ..LinkLifecycleConfig::default()
        };
        let mut lc = LinkLifecycle::new(config);
        let start = Instant::now();

        // Drive to Cooldown
        for i in 0..12 {
            let now = start + Duration::from_millis(i * 100);
            lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        }
        for i in 0..3 {
            let now = start + Duration::from_secs(2) + Duration::from_millis(i * 50);
            lc.update(now, 200.0, 0.9, 100_000.0, Duration::from_millis(200));
        }
        for i in 0..10 {
            let now = start + Duration::from_secs(3) + Duration::from_millis(i * 50);
            lc.update(now, 200.0, 0.9, 100_000.0, Duration::from_millis(200));
        }
        assert_eq!(lc.phase, LinkPhase::Cooldown);

        // Wait less than cooldown -> stays Cooldown
        let now = start + Duration::from_secs(4);
        lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        assert_eq!(lc.phase, LinkPhase::Cooldown);

        // Wait past cooldown -> Probe
        let now = start + Duration::from_secs(10);
        lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        assert_eq!(lc.phase, LinkPhase::Probe);
    }

    #[test]
    fn lifecycle_reset_to_probe_on_fresh() {
        let mut lc = LinkLifecycle::new(LinkLifecycleConfig::default());
        let start = Instant::now();

        for i in 0..12 {
            let now = start + Duration::from_millis(i * 100);
            lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        }
        let now = start + Duration::from_secs(5);
        lc.update(now, 0.0, 1.0, 0.0, Duration::from_secs(5));
        assert_eq!(lc.phase, LinkPhase::Reset);

        let now = start + Duration::from_secs(6);
        let phase = lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        assert_eq!(phase, LinkPhase::Probe);
    }

    #[test]
    fn lifecycle_reset_stays_without_fresh_stats() {
        let mut lc = LinkLifecycle::new(LinkLifecycleConfig::default());
        let start = Instant::now();

        for i in 0..12 {
            let now = start + Duration::from_millis(i * 100);
            lc.update(now, 10.0, 0.01, 1_000_000.0, Duration::from_millis(200));
        }
        lc.update(
            start + Duration::from_secs(5),
            0.0,
            1.0,
            0.0,
            Duration::from_secs(5),
        );
        assert_eq!(lc.phase, LinkPhase::Reset);

        let now = start + Duration::from_secs(8);
        let phase = lc.update(now, 0.0, 1.0, 0.0, Duration::from_secs(10));
        assert_eq!(phase, LinkPhase::Reset);
    }

    #[test]
    fn ewma_stats_default_alpha() {
        let s = EwmaStats::default();
        assert!((s.rtt.value() - 0.0).abs() < f64::EPSILON);
        assert_eq!(s.last_sent, 0);
    }

    #[test]
    fn link_stats_default_values() {
        use std::sync::atomic::Ordering;
        let ls = LinkStats::default();
        assert_eq!(ls.rtt.load(Ordering::Relaxed), 0);
        assert_eq!(ls.bandwidth.load(Ordering::Relaxed), 0);
        assert_eq!(ls.os_up_i32.load(Ordering::Relaxed), -1);
        assert_eq!(ls.mtu_i32.load(Ordering::Relaxed), -1);
        let lc = ls.lifecycle.lock().unwrap();
        assert_eq!(lc.phase, LinkPhase::Init);
    }
}
