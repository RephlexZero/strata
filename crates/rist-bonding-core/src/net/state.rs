use crate::config::LinkLifecycleConfig;
use crate::net::interface::LinkPhase;
use crate::scheduler::ewma::Ewma;
use std::sync::atomic::{AtomicI32, AtomicU64};
use std::sync::Mutex;
use std::time::Instant;

pub struct EwmaStats {
    pub rtt: Ewma,
    pub bandwidth: Ewma,
    pub loss: Ewma,
    pub last_sent: u64,
    pub last_lost: u64,
    pub last_rex: u64,
}

impl Default for EwmaStats {
    fn default() -> Self {
        Self {
            rtt: Ewma::new(0.125),
            bandwidth: Ewma::new(0.125),
            loss: Ewma::new(0.125),
            last_sent: 0,
            last_lost: 0,
            last_rex: 0,
        }
    }
}

pub struct LinkStats {
    pub rtt: AtomicU64,
    pub bandwidth: AtomicU64,
    pub retransmitted: AtomicU64,
    pub sent: AtomicU64,
    pub lost: AtomicU64,
    pub smoothed_rtt_us: AtomicU64,
    pub smoothed_bw_bps: AtomicU64,
    pub smoothed_loss_permille: AtomicU64, // Stored as * 1000. 1000 = 100% loss.
    pub last_stats_ms: AtomicU64,
    pub os_up_i32: AtomicI32, // -1 unknown, 0 down, 1 up
    pub mtu_i32: AtomicI32,   // -1 unknown
    pub os_last_poll_ms: AtomicU64,
    pub ewma_state: Mutex<EwmaStats>,
    pub lifecycle: Mutex<LinkLifecycle>,
}

impl LinkStats {
    pub fn new(lifecycle_config: LinkLifecycleConfig) -> Self {
        Self {
            rtt: AtomicU64::new(0),
            bandwidth: AtomicU64::new(0),
            retransmitted: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            smoothed_rtt_us: AtomicU64::new(0),
            smoothed_bw_bps: AtomicU64::new(0),
            smoothed_loss_permille: AtomicU64::new(0),
            last_stats_ms: AtomicU64::new(0),
            os_up_i32: AtomicI32::new(-1),
            mtu_i32: AtomicI32::new(-1),
            os_last_poll_ms: AtomicU64::new(0),
            ewma_state: Mutex::new(EwmaStats::default()),
            lifecycle: Mutex::new(LinkLifecycle::new(lifecycle_config)),
        }
    }
}

impl Default for LinkStats {
    fn default() -> Self {
        Self::new(LinkLifecycleConfig::default())
    }
}

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

        if self.phase != LinkPhase::Cooldown && self.phase != LinkPhase::Reset {
            if self.phase != LinkPhase::Init {
                self.last_transition = now;
            }
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
}
