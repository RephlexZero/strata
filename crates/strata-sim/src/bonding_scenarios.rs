//! # Bonding-Specific Network Scenarios
//!
//! Pre-built scenario templates that exercise bonding edge cases:
//! - **Link failure / recovery** — sudden loss of one link
//! - **Handover simulation** — gradual SINR degradation + recovery
//! - **Correlated fading** — all links degrade simultaneously
//! - **Asymmetric capacity** — one link much faster than others
//!
//! These produce `Vec<ScenarioFrame>` compatible with the existing
//! impairment infrastructure.

use crate::impairment::ImpairmentConfig;
use crate::scenario::ScenarioFrame;
use std::time::Duration;

/// A link failure event: link drops, then recovers after a duration.
#[derive(Debug, Clone)]
pub struct LinkFailureScenario {
    /// Total scenario duration.
    pub duration: Duration,
    /// Time step between frames.
    pub step: Duration,
    /// Number of links (the last one will fail).
    pub num_links: usize,
    /// When the failure starts.
    pub failure_start: Duration,
    /// How long the failure lasts.
    pub failure_duration: Duration,
    /// Normal link capacity (kbit/s) for healthy links.
    pub normal_rate_kbit: u64,
    /// Normal delay (ms).
    pub normal_delay_ms: u32,
}

impl Default for LinkFailureScenario {
    fn default() -> Self {
        LinkFailureScenario {
            duration: Duration::from_secs(30),
            step: Duration::from_secs(1),
            num_links: 3,
            failure_start: Duration::from_secs(10),
            failure_duration: Duration::from_secs(5),
            normal_rate_kbit: 5_000,
            normal_delay_ms: 20,
        }
    }
}

impl LinkFailureScenario {
    /// Generate frames. The last link goes to 0 kbit during the failure window.
    pub fn frames(&self) -> Vec<ScenarioFrame> {
        let total_steps = (self.duration.as_secs_f64() / self.step.as_secs_f64()).ceil() as u64;
        let fail_end = self.failure_start + self.failure_duration;

        (0..=total_steps)
            .map(|i| {
                let t = self.step.mul_f64(i as f64);
                let in_failure = t >= self.failure_start && t < fail_end;

                let configs: Vec<ImpairmentConfig> = (0..self.num_links)
                    .map(|link_idx| {
                        let is_failing = link_idx == self.num_links - 1 && in_failure;
                        if is_failing {
                            ImpairmentConfig {
                                rate_kbit: Some(1),   // near-zero
                                delay_ms: Some(2000), // massive delay
                                loss_percent: Some(100.0),
                                ..Default::default()
                            }
                        } else {
                            ImpairmentConfig {
                                rate_kbit: Some(self.normal_rate_kbit),
                                delay_ms: Some(self.normal_delay_ms),
                                loss_percent: Some(0.0),
                                ..Default::default()
                            }
                        }
                    })
                    .collect();

                ScenarioFrame { t, configs }
            })
            .collect()
    }
}

/// Simulates a cellular handover: gradual SINR degradation causing
/// increased delay and loss, followed by recovery on a "new cell".
#[derive(Debug, Clone)]
pub struct HandoverScenario {
    /// Total scenario duration.
    pub duration: Duration,
    /// Time step.
    pub step: Duration,
    /// Number of links (link 0 will undergo handover).
    pub num_links: usize,
    /// When degradation begins.
    pub degradation_start: Duration,
    /// Duration of the degradation ramp.
    pub degradation_ramp: Duration,
    /// Duration of the blackout (handover gap).
    pub blackout_duration: Duration,
    /// Duration of the recovery ramp.
    pub recovery_ramp: Duration,
    /// Normal capacity.
    pub normal_rate_kbit: u64,
    /// Normal delay.
    pub normal_delay_ms: u32,
}

impl Default for HandoverScenario {
    fn default() -> Self {
        HandoverScenario {
            duration: Duration::from_secs(30),
            step: Duration::from_millis(500),
            num_links: 2,
            degradation_start: Duration::from_secs(8),
            degradation_ramp: Duration::from_secs(3),
            blackout_duration: Duration::from_millis(500),
            recovery_ramp: Duration::from_secs(2),
            normal_rate_kbit: 10_000,
            normal_delay_ms: 15,
        }
    }
}

impl HandoverScenario {
    pub fn frames(&self) -> Vec<ScenarioFrame> {
        let total_steps = (self.duration.as_secs_f64() / self.step.as_secs_f64()).ceil() as u64;
        let deg_end = self.degradation_start + self.degradation_ramp;
        let blackout_end = deg_end + self.blackout_duration;
        let recovery_end = blackout_end + self.recovery_ramp;

        (0..=total_steps)
            .map(|i| {
                let t = self.step.mul_f64(i as f64);
                let configs: Vec<ImpairmentConfig> = (0..self.num_links)
                    .map(|link_idx| {
                        if link_idx != 0 {
                            // Non-handover links are stable
                            return ImpairmentConfig {
                                rate_kbit: Some(self.normal_rate_kbit),
                                delay_ms: Some(self.normal_delay_ms),
                                loss_percent: Some(0.0),
                                ..Default::default()
                            };
                        }

                        // Link 0: handover progression
                        if t < self.degradation_start {
                            // Normal
                            ImpairmentConfig {
                                rate_kbit: Some(self.normal_rate_kbit),
                                delay_ms: Some(self.normal_delay_ms),
                                loss_percent: Some(0.0),
                                ..Default::default()
                            }
                        } else if t < deg_end {
                            // Degrading: linear ramp
                            let progress = (t - self.degradation_start).as_secs_f64()
                                / self.degradation_ramp.as_secs_f64();
                            let rate = self.normal_rate_kbit as f64 * (1.0 - progress * 0.8);
                            let delay = self.normal_delay_ms as f64 * (1.0 + progress * 5.0);
                            let loss = progress * 15.0;
                            ImpairmentConfig {
                                rate_kbit: Some(rate.max(1.0) as u64),
                                delay_ms: Some(delay as u32),
                                loss_percent: Some(loss as f32),
                                ..Default::default()
                            }
                        } else if t < blackout_end {
                            // Blackout
                            ImpairmentConfig {
                                rate_kbit: Some(1),
                                delay_ms: Some(5000),
                                loss_percent: Some(100.0),
                                ..Default::default()
                            }
                        } else if t < recovery_end {
                            // Recovery ramp
                            let progress =
                                (t - blackout_end).as_secs_f64() / self.recovery_ramp.as_secs_f64();
                            let rate = self.normal_rate_kbit as f64 * progress;
                            let delay =
                                self.normal_delay_ms as f64 * (1.0 + (1.0 - progress) * 3.0);
                            let loss = (1.0 - progress) * 5.0;
                            ImpairmentConfig {
                                rate_kbit: Some(rate.max(1.0) as u64),
                                delay_ms: Some(delay as u32),
                                loss_percent: Some(loss as f32),
                                ..Default::default()
                            }
                        } else {
                            // Recovered
                            ImpairmentConfig {
                                rate_kbit: Some(self.normal_rate_kbit),
                                delay_ms: Some(self.normal_delay_ms),
                                loss_percent: Some(0.0),
                                ..Default::default()
                            }
                        }
                    })
                    .collect();

                ScenarioFrame { t, configs }
            })
            .collect()
    }
}

/// Correlated fading: ALL links degrade simultaneously (e.g., rain fade,
/// entering a tunnel).
#[derive(Debug, Clone)]
pub struct CorrelatedFadingScenario {
    /// Total duration.
    pub duration: Duration,
    /// Time step.
    pub step: Duration,
    /// Number of links.
    pub num_links: usize,
    /// When the fade begins.
    pub fade_start: Duration,
    /// How long the fade lasts.
    pub fade_duration: Duration,
    /// Severity: fraction of capacity lost (0.0 - 1.0).
    pub severity: f64,
    /// Normal capacity.
    pub normal_rate_kbit: u64,
    /// Normal delay.
    pub normal_delay_ms: u32,
}

impl Default for CorrelatedFadingScenario {
    fn default() -> Self {
        CorrelatedFadingScenario {
            duration: Duration::from_secs(20),
            step: Duration::from_millis(500),
            num_links: 3,
            fade_start: Duration::from_secs(5),
            fade_duration: Duration::from_secs(8),
            severity: 0.7,
            normal_rate_kbit: 8_000,
            normal_delay_ms: 20,
        }
    }
}

impl CorrelatedFadingScenario {
    pub fn frames(&self) -> Vec<ScenarioFrame> {
        let total_steps = (self.duration.as_secs_f64() / self.step.as_secs_f64()).ceil() as u64;
        let fade_end = self.fade_start + self.fade_duration;

        (0..=total_steps)
            .map(|i| {
                let t = self.step.mul_f64(i as f64);

                // Compute fade factor (bell-shaped within fade window)
                let fade_factor = if t >= self.fade_start && t < fade_end {
                    let progress =
                        (t - self.fade_start).as_secs_f64() / self.fade_duration.as_secs_f64();
                    // Sine bell: peak at midpoint
                    let bell = (progress * std::f64::consts::PI).sin();
                    bell * self.severity
                } else {
                    0.0
                };

                let configs: Vec<ImpairmentConfig> = (0..self.num_links)
                    .map(|_| {
                        let rate = self.normal_rate_kbit as f64 * (1.0 - fade_factor);
                        let delay = self.normal_delay_ms as f64 * (1.0 + fade_factor * 3.0);
                        let loss = fade_factor * 10.0;
                        ImpairmentConfig {
                            rate_kbit: Some(rate.max(1.0) as u64),
                            delay_ms: Some(delay as u32),
                            loss_percent: Some(loss as f32),
                            ..Default::default()
                        }
                    })
                    .collect();

                ScenarioFrame { t, configs }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Link Failure ───────────────────────────────────────────────────

    #[test]
    fn link_failure_has_correct_duration() {
        let scenario = LinkFailureScenario::default();
        let frames = scenario.frames();
        let last_t = frames.last().unwrap().t;
        assert!(last_t >= scenario.duration);
    }

    #[test]
    fn link_failure_produces_failure_window() {
        let scenario = LinkFailureScenario {
            failure_start: Duration::from_secs(5),
            failure_duration: Duration::from_secs(3),
            num_links: 2,
            ..Default::default()
        };
        let frames = scenario.frames();

        // Find a frame during the failure
        let during_failure = frames
            .iter()
            .find(|f| f.t >= Duration::from_secs(6) && f.t < Duration::from_secs(8))
            .unwrap();

        // Last link should be failing
        let failing = &during_failure.configs[1];
        assert_eq!(failing.loss_percent, Some(100.0));

        // First link should be healthy
        let healthy = &during_failure.configs[0];
        assert_eq!(healthy.loss_percent, Some(0.0));
    }

    #[test]
    fn link_failure_recovers_after_window() {
        let scenario = LinkFailureScenario {
            failure_start: Duration::from_secs(5),
            failure_duration: Duration::from_secs(3),
            num_links: 2,
            ..Default::default()
        };
        let frames = scenario.frames();

        // After failure window, link should be healthy
        let after_recovery = frames
            .iter()
            .find(|f| f.t >= Duration::from_secs(9))
            .unwrap();

        let recovered = &after_recovery.configs[1];
        assert_eq!(recovered.loss_percent, Some(0.0));
    }

    // ─── Handover ───────────────────────────────────────────────────────

    #[test]
    fn handover_has_degradation_blackout_recovery() {
        let scenario = HandoverScenario::default();
        let frames = scenario.frames();

        // Before degradation: normal
        let pre = frames
            .iter()
            .find(|f| f.t < scenario.degradation_start)
            .unwrap();
        assert_eq!(pre.configs[0].loss_percent, Some(0.0));

        // During blackout: total loss
        let deg_end = scenario.degradation_start + scenario.degradation_ramp;
        let blackout_mid = deg_end + scenario.blackout_duration / 2;
        let blackout = frames
            .iter()
            .find(|f| f.t >= deg_end && f.t <= blackout_mid)
            .unwrap();
        assert_eq!(blackout.configs[0].loss_percent, Some(100.0));

        // After recovery: back to normal
        let recovery_end = deg_end + scenario.blackout_duration + scenario.recovery_ramp;
        let post = frames
            .iter()
            .find(|f| f.t >= recovery_end + Duration::from_secs(1))
            .unwrap();
        assert_eq!(post.configs[0].loss_percent, Some(0.0));
    }

    #[test]
    fn handover_non_target_links_stable() {
        let scenario = HandoverScenario {
            num_links: 3,
            ..Default::default()
        };
        let frames = scenario.frames();

        // All non-zero links should always be stable
        for frame in &frames {
            for (i, cfg) in frame.configs.iter().enumerate() {
                if i > 0 {
                    assert_eq!(cfg.loss_percent, Some(0.0));
                    assert_eq!(cfg.rate_kbit, Some(scenario.normal_rate_kbit));
                }
            }
        }
    }

    // ─── Correlated Fading ──────────────────────────────────────────────

    #[test]
    fn correlated_fading_affects_all_links() {
        let scenario = CorrelatedFadingScenario::default();
        let frames = scenario.frames();

        // Mid-fade: all links should have reduced capacity
        let mid_fade = scenario.fade_start + scenario.fade_duration / 2;
        let mid = frames
            .iter()
            .find(|f| (f.t.as_secs_f64() - mid_fade.as_secs_f64()).abs() < 0.6)
            .unwrap();

        for cfg in &mid.configs {
            let rate = cfg.rate_kbit.unwrap();
            assert!(
                rate < scenario.normal_rate_kbit,
                "all links should be degraded during fade: rate={rate}"
            );
        }
    }

    #[test]
    fn correlated_fading_recovers_fully() {
        let scenario = CorrelatedFadingScenario::default();
        let frames = scenario.frames();

        // After fade window: all links normal
        let after = frames.last().unwrap();
        for cfg in &after.configs {
            assert_eq!(cfg.rate_kbit, Some(scenario.normal_rate_kbit));
            assert_eq!(cfg.delay_ms, Some(scenario.normal_delay_ms));
        }
    }

    #[test]
    fn correlated_fading_bell_shaped() {
        let scenario = CorrelatedFadingScenario {
            num_links: 1,
            ..Default::default()
        };
        let frames = scenario.frames();
        let fade_end = scenario.fade_start + scenario.fade_duration;

        // Collect rates during fade
        let fade_rates: Vec<u64> = frames
            .iter()
            .filter(|f| f.t >= scenario.fade_start && f.t < fade_end)
            .map(|f| f.configs[0].rate_kbit.unwrap())
            .collect();

        assert!(!fade_rates.is_empty());

        // Mid-point should be the minimum (bell-shaped)
        let mid_idx = fade_rates.len() / 2;
        let mid_rate = fade_rates[mid_idx];
        let first_rate = fade_rates[0];
        let last_rate = fade_rates[fade_rates.len() - 1];

        assert!(
            mid_rate <= first_rate && mid_rate <= last_rate,
            "bell-shaped: mid={mid_rate}, first={first_rate}, last={last_rate}"
        );
    }
}
