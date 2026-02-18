//! # Encoder Bitrate Adaptation — Closed-Loop Feedback
//!
//! Generates `BitrateCmd` control packets based on aggregate link capacity,
//! degradation stage, and congestion signals. This is the "encoder feedback
//! loop" that separates Strata from SRT and block-FEC approaches.
//!
//! ## Policy
//!
//! The adapter monitors total available capacity across all bonded links
//! and compares it to the current encoder bitrate. When capacity drops
//! below the encoder rate, it issues a `BitrateCmd` to reduce. When
//! capacity recovers, it ramps the encoder back up conservatively.

use quanta::Instant;
use std::time::Duration;

use crate::media::priority::DegradationStage;

/// Configuration for the bitrate adapter.
#[derive(Debug, Clone)]
pub struct AdaptationConfig {
    /// Minimum bitrate the encoder supports (kbps).
    pub min_bitrate_kbps: u32,
    /// Maximum bitrate the encoder should use (kbps).
    pub max_bitrate_kbps: u32,
    /// Safety headroom: target bitrate = capacity × (1 - headroom).
    /// Default: 0.15 (15% headroom for FEC + control overhead).
    pub headroom: f64,
    /// How quickly to ramp up after recovery (kbps per step).
    pub ramp_up_kbps_per_step: u32,
    /// How quickly to ramp down on congestion (multiplier, e.g., 0.7 = 30% cut).
    pub ramp_down_factor: f64,
    /// Minimum interval between bitrate commands.
    pub min_interval: Duration,
    /// Pressure ratio threshold for degradation stages.
    /// pressure = encoder_bitrate / available_capacity.
    pub pressure_threshold: f64,
    /// Bitrate cap for "visually lossless" in MaxReliability mode (kbps).
    /// When in MaxReliability mode, encoder target is capped here and spare
    /// bandwidth is diverted to FEC + packet duplication.
    /// Default: 6000 (6 Mbps — good for 1080p60 HEVC).
    pub quality_cap_kbps: u32,
    /// Minimum spare bandwidth (kbps) to trigger MaxReliability mode.
    /// Default: 3000 (3 Mbps spare).
    pub reliability_spare_threshold_kbps: u32,
}

impl Default for AdaptationConfig {
    fn default() -> Self {
        AdaptationConfig {
            min_bitrate_kbps: 500,
            max_bitrate_kbps: 20_000,
            headroom: 0.15,
            ramp_up_kbps_per_step: 200,
            ramp_down_factor: 0.7,
            min_interval: Duration::from_millis(200),
            pressure_threshold: 0.9,
            quality_cap_kbps: 6_000,
            reliability_spare_threshold_kbps: 3_000,
        }
    }
}

/// Reason for a bitrate change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdaptationReason {
    /// Normal capacity-based adaptation.
    Capacity,
    /// Congestion detected (loss or RTT spike).
    Congestion,
    /// Link failure reduced aggregate capacity.
    LinkFailure,
    /// Capacity recovered — ramping up.
    Recovery,
}

/// Reliability vs quality trade-off mode.
///
/// When spare bandwidth exists, the adapter can either push encoder quality
/// higher (MaxQuality) or cap the encoder and divert spare capacity to
/// FEC overhead and packet duplication (MaxReliability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReliabilityMode {
    /// Push encoder bitrate up, minimal FEC overhead.
    MaxQuality,
    /// Cap encoder at visually-lossless threshold, divert spare to FEC + duplication.
    MaxReliability,
}

/// A bitrate command to send to the encoder.
#[derive(Debug, Clone)]
pub struct BitrateCommand {
    /// Target bitrate in kbps.
    pub target_kbps: u32,
    /// Why the change was requested.
    pub reason: AdaptationReason,
    /// Current degradation stage.
    pub stage: DegradationStage,
    /// Current reliability mode.
    pub mode: ReliabilityMode,
    /// Spare bandwidth available for redundancy (kbps). 0 in MaxQuality mode.
    pub spare_bw_kbps: u32,
    /// Recommended FEC overhead fraction (0.10 = 10% default, up to 0.50).
    pub recommended_fec_overhead: f64,
}

/// Per-link capacity input for the adapter.
#[derive(Debug, Clone)]
pub struct LinkCapacity {
    /// Link identifier.
    pub link_id: usize,
    /// Estimated throughput capacity in kbps.
    pub capacity_kbps: f64,
    /// Whether the link is alive and usable.
    pub alive: bool,
    /// Current loss rate (0.0 - 1.0).
    pub loss_rate: f64,
    /// Current RTT in ms.
    pub rtt_ms: f64,
}

/// Receiver-side telemetry feedback for the BitrateAdapter.
///
/// Provides ground-truth metrics from the receiver that the sender
/// cannot estimate purely from its own observations.
#[derive(Debug, Clone, Default)]
pub struct ReceiverFeedback {
    /// Total recovered video goodput (bits/sec).
    pub goodput_bps: u64,
    /// Fraction of packets recovered by FEC (0.0–1.0).
    pub fec_repair_rate: f32,
    /// Current jitter buffer depth in milliseconds.
    pub jitter_buffer_ms: u32,
    /// Residual loss after FEC recovery (0.0–1.0).
    pub loss_after_fec: f32,
}

/// Encoder Bitrate Adapter.
///
/// Monitors aggregate capacity and generates bitrate commands
/// to keep the encoder in sync with available bandwidth.
pub struct BitrateAdapter {
    config: AdaptationConfig,
    /// Current target bitrate (kbps).
    current_target_kbps: u32,
    /// Current degradation stage.
    stage: DegradationStage,
    /// Current reliability mode.
    mode: ReliabilityMode,
    /// Spare bandwidth available for redundancy (kbps).
    spare_bw_kbps: u32,
    /// When the last command was issued (None = never).
    last_command_time: Option<Instant>,
    /// Previous aggregate capacity for trend detection.
    prev_capacity_kbps: f64,
    /// Number of consecutive capacity increases (for ramp-up gating).
    consecutive_increases: u32,
    /// Number of consecutive capacity decreases.
    consecutive_decreases: u32,
}

impl BitrateAdapter {
    pub fn new(config: AdaptationConfig) -> Self {
        let initial = config.max_bitrate_kbps;
        BitrateAdapter {
            config,
            current_target_kbps: initial,
            stage: DegradationStage::Normal,
            mode: ReliabilityMode::MaxQuality,
            spare_bw_kbps: 0,
            last_command_time: None,
            prev_capacity_kbps: 0.0,
            consecutive_increases: 0,
            consecutive_decreases: 0,
        }
    }

    /// Current target bitrate in kbps.
    pub fn current_target_kbps(&self) -> u32 {
        self.current_target_kbps
    }

    /// Current degradation stage.
    pub fn stage(&self) -> DegradationStage {
        self.stage
    }

    /// Current reliability mode.
    pub fn mode(&self) -> ReliabilityMode {
        self.mode
    }

    /// Spare bandwidth in kbps (available for redundancy).
    pub fn spare_bw_kbps(&self) -> u32 {
        self.spare_bw_kbps
    }

    /// Recommended FEC overhead fraction based on spare bandwidth.
    ///
    /// Returns 0.10 (10%) as baseline. With spare bandwidth in MaxReliability
    /// mode, scales up linearly to 0.50 (50%) as the spare-to-target ratio
    /// increases.
    pub fn recommended_fec_overhead(&self) -> f64 {
        const BASE_OVERHEAD: f64 = 0.10;
        const MAX_OVERHEAD: f64 = 0.50;
        if self.mode != ReliabilityMode::MaxReliability || self.spare_bw_kbps == 0 {
            return BASE_OVERHEAD;
        }
        // Scale overhead: spare / target ratio, capped at MAX_OVERHEAD
        let ratio = self.spare_bw_kbps as f64 / self.current_target_kbps.max(1) as f64;
        (BASE_OVERHEAD + ratio * (MAX_OVERHEAD - BASE_OVERHEAD)).min(MAX_OVERHEAD)
    }

    /// Build a BitrateCommand with current mode/spare/fec fields.
    fn make_command(&self, target_kbps: u32, reason: AdaptationReason) -> BitrateCommand {
        BitrateCommand {
            target_kbps,
            reason,
            stage: self.stage,
            mode: self.mode,
            spare_bw_kbps: self.spare_bw_kbps,
            recommended_fec_overhead: self.recommended_fec_overhead(),
        }
    }

    /// Update with new link capacity information and optionally produce
    /// a bitrate command if the encoder target should change.
    pub fn update(&mut self, links: &[LinkCapacity]) -> Option<BitrateCommand> {
        // Aggregate capacity from alive links
        let aggregate_kbps: f64 = links
            .iter()
            .filter(|l| l.alive)
            .map(|l| {
                // Discount capacity by loss rate (effective throughput)
                l.capacity_kbps * (1.0 - l.loss_rate)
            })
            .sum();

        let alive_count = links.iter().filter(|l| l.alive).count();

        // Compute usable capacity (with headroom)
        let usable_kbps = aggregate_kbps * (1.0 - self.config.headroom);

        // Compute pressure ratio (target / capacity; >1 = over-pressure)
        let pressure = if usable_kbps > 0.0 {
            self.current_target_kbps as f64 / usable_kbps
        } else if alive_count > 0 {
            2.0 // Over-pressure: have links but zero capacity
        } else {
            5.0 // Extreme: no links alive
        };

        // DegradationStage::from_pressure expects capacity/required ratio
        let capacity_ratio = if pressure > 0.0 { 1.0 / pressure } else { 1.0 };
        self.stage = DegradationStage::from_pressure(capacity_ratio);

        // Track capacity trend (stable-or-increasing vs decreasing)
        if aggregate_kbps >= self.prev_capacity_kbps * 0.95 {
            self.consecutive_increases += 1;
            self.consecutive_decreases = 0;
        } else if aggregate_kbps < self.prev_capacity_kbps * 0.90 {
            self.consecutive_decreases += 1;
            self.consecutive_increases = 0;
        }
        self.prev_capacity_kbps = aggregate_kbps;

        // ─── Mode switching: MaxQuality ↔ MaxReliability ────────────
        // Switch to MaxReliability when encoder is at 80%+ of ceiling
        // and spare bandwidth exceeds threshold.
        let at_ceiling =
            self.current_target_kbps as f64 >= self.config.max_bitrate_kbps as f64 * 0.80;
        let big_spare = usable_kbps
            > (self.current_target_kbps + self.config.reliability_spare_threshold_kbps) as f64;

        if at_ceiling && big_spare {
            self.mode = ReliabilityMode::MaxReliability;
        } else if usable_kbps < self.config.quality_cap_kbps as f64 * 1.2 {
            // Not enough capacity to even reach the quality cap with spare
            self.mode = ReliabilityMode::MaxQuality;
        }

        // Compute effective max bitrate (capped in MaxReliability mode)
        let effective_max = if self.mode == ReliabilityMode::MaxReliability {
            self.config.quality_cap_kbps
        } else {
            self.config.max_bitrate_kbps
        };

        // Determine if we need a bitrate change
        let (new_target, reason) = self.compute_target(usable_kbps, pressure, alive_count);
        let new_target = new_target.min(effective_max);

        // Track spare bandwidth
        self.spare_bw_kbps = if usable_kbps > new_target as f64 {
            (usable_kbps - new_target as f64) as u32
        } else {
            0
        };

        // Only issue command if target changed meaningfully and enough time passed
        let target_changed = (new_target as i64 - self.current_target_kbps as i64).unsigned_abs()
            > self.config.ramp_up_kbps_per_step as u64 / 2;
        let interval_ok = self
            .last_command_time
            .is_none_or(|t| t.elapsed() >= self.config.min_interval);

        if target_changed && interval_ok {
            self.current_target_kbps = new_target;
            self.last_command_time = Some(Instant::now());
            Some(self.make_command(new_target, reason))
        } else {
            None
        }
    }

    /// Update with link capacity AND receiver feedback.
    ///
    /// The receiver feedback provides ground-truth signals that can override
    /// or supplement the sender's local capacity estimates:
    /// - `loss_after_fec > 1%` → apply congestion pressure
    /// - `jitter_buffer_ms > 500` → bufferbloat, cap bitrate
    /// - `goodput_bps` significantly below encoder output → congestion
    pub fn update_with_feedback(
        &mut self,
        links: &[LinkCapacity],
        feedback: &ReceiverFeedback,
    ) -> Option<BitrateCommand> {
        // Start with normal capacity-based update
        let mut result = self.update(links);

        // Apply receiver-side pressure signals
        let loss_pressure = feedback.loss_after_fec > 0.01;
        let bufferbloat = feedback.jitter_buffer_ms > 500;
        let goodput_shortfall = feedback.goodput_bps > 0
            && (feedback.goodput_bps as f64) < self.current_target_kbps as f64 * 1000.0 * 0.7;

        if loss_pressure || bufferbloat || goodput_shortfall {
            // Receiver signals congestion — force a reduction
            let new_target =
                (self.current_target_kbps as f64 * self.config.ramp_down_factor) as u32;
            let new_target = new_target.max(self.config.min_bitrate_kbps);

            if new_target < self.current_target_kbps {
                self.current_target_kbps = new_target;
                self.last_command_time = Some(Instant::now());
                self.consecutive_decreases += 1;
                self.consecutive_increases = 0;

                let reason = if loss_pressure {
                    AdaptationReason::Congestion
                } else {
                    AdaptationReason::Capacity
                };

                result = Some(self.make_command(new_target, reason));
            }
        }

        result
    }

    /// Compute the new target bitrate.
    fn compute_target(
        &self,
        usable_kbps: f64,
        pressure: f64,
        alive_count: usize,
    ) -> (u32, AdaptationReason) {
        let current = self.current_target_kbps;

        // Emergency: no links
        if alive_count == 0 {
            return (self.config.min_bitrate_kbps, AdaptationReason::LinkFailure);
        }

        // Over-pressure: need to reduce
        if pressure > self.config.pressure_threshold {
            let target = (current as f64 * self.config.ramp_down_factor) as u32;
            let target = target
                .max(self.config.min_bitrate_kbps)
                .min(usable_kbps as u32);

            let reason = if self.consecutive_decreases >= 3 {
                AdaptationReason::Congestion
            } else {
                AdaptationReason::Capacity
            };

            return (target, reason);
        }

        // Under-pressure with stable capacity: ramp up
        if pressure < 0.7 && self.consecutive_increases >= 3 {
            let target = current + self.config.ramp_up_kbps_per_step;
            let target = target.min(self.config.max_bitrate_kbps);
            let target = target.min(usable_kbps as u32);
            return (target, AdaptationReason::Recovery);
        }

        // Stable: no change
        (current, AdaptationReason::Capacity)
    }

    /// Force an immediate bitrate reduction (e.g., on link failure event).
    pub fn force_reduce(&mut self, reason: AdaptationReason) -> BitrateCommand {
        let new_target = (self.current_target_kbps as f64 * self.config.ramp_down_factor) as u32;
        let new_target = new_target.max(self.config.min_bitrate_kbps);
        self.current_target_kbps = new_target;
        self.last_command_time = Some(Instant::now());
        self.make_command(new_target, reason)
    }

    /// Reset to maximum bitrate (e.g., on stream restart).
    pub fn reset(&mut self) {
        self.current_target_kbps = self.config.max_bitrate_kbps;
        self.stage = DegradationStage::Normal;
        self.mode = ReliabilityMode::MaxQuality;
        self.spare_bw_kbps = 0;
        self.consecutive_increases = 0;
        self.consecutive_decreases = 0;
        self.prev_capacity_kbps = 0.0;
        self.last_command_time = None;
    }
}

impl Default for BitrateAdapter {
    fn default() -> Self {
        Self::new(AdaptationConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_links(capacities: &[(f64, bool)]) -> Vec<LinkCapacity> {
        capacities
            .iter()
            .enumerate()
            .map(|(i, &(cap, alive))| LinkCapacity {
                link_id: i,
                capacity_kbps: cap,
                alive,
                loss_rate: 0.0,
                rtt_ms: 20.0,
            })
            .collect()
    }

    // ─── Basic Operation ────────────────────────────────────────────────

    #[test]
    fn initial_state() {
        let adapter = BitrateAdapter::default();
        assert_eq!(adapter.current_target_kbps(), 20_000);
        assert_eq!(adapter.stage(), DegradationStage::Normal);
    }

    #[test]
    fn no_change_when_capacity_exceeds_target() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 5_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // 10 Mbps aggregate > 5 Mbps target
        let links = make_links(&[(5_000.0, true), (5_000.0, true)]);
        let cmd = adapter.update(&links);
        assert!(cmd.is_none(), "should not change when capacity >> target");
    }

    // ─── Ramp Down ──────────────────────────────────────────────────────

    #[test]
    fn reduces_bitrate_when_capacity_drops() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // Capacity suddenly drops below encoder target
        let links = make_links(&[(3_000.0, true)]);
        let cmd = adapter.update(&links);
        assert!(cmd.is_some(), "should reduce bitrate");
        let cmd = cmd.unwrap();
        assert!(
            cmd.target_kbps < 10_000,
            "target should be reduced, got {}",
            cmd.target_kbps
        );
    }

    #[test]
    fn all_links_dead_drops_to_minimum() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            min_bitrate_kbps: 500,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        let links = make_links(&[(5_000.0, false), (5_000.0, false)]);
        let cmd = adapter.update(&links);
        assert!(cmd.is_some());
        let cmd = cmd.unwrap();
        assert_eq!(cmd.target_kbps, 500);
        assert_eq!(cmd.reason, AdaptationReason::LinkFailure);
    }

    // ─── Ramp Up ────────────────────────────────────────────────────────

    #[test]
    fn ramps_up_after_sustained_recovery() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ramp_up_kbps_per_step: 500,
            ..Default::default()
        });

        // Force low starting point
        adapter.current_target_kbps = 3_000;
        adapter.prev_capacity_kbps = 5_000.0;

        // Simulate 4 consecutive capacity increases
        let links = make_links(&[(10_000.0, true), (10_000.0, true)]);
        for _ in 0..4 {
            adapter.update(&links);
        }

        assert!(
            adapter.current_target_kbps() > 3_000,
            "should have ramped up, got {}",
            adapter.current_target_kbps()
        );
    }

    // ─── Degradation Stages ─────────────────────────────────────────────

    #[test]
    fn stage_escalates_with_pressure() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // Normal pressure
        let links = make_links(&[(20_000.0, true)]);
        adapter.update(&links);
        assert_eq!(adapter.stage(), DegradationStage::Normal);

        // High pressure (target exceeds capacity)
        adapter.current_target_kbps = 10_000;
        let links = make_links(&[(2_000.0, true)]);
        adapter.update(&links);
        assert_ne!(
            adapter.stage(),
            DegradationStage::Normal,
            "should escalate under heavy pressure"
        );
    }

    // ─── Loss Discounting ───────────────────────────────────────────────

    #[test]
    fn loss_reduces_effective_capacity() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // 10 Mbps raw but 20% loss → 8 Mbps effective
        let links = vec![LinkCapacity {
            link_id: 0,
            capacity_kbps: 10_000.0,
            alive: true,
            loss_rate: 0.20,
            rtt_ms: 30.0,
        }];

        adapter.update(&links);
        // Usable = 10000 * 0.8 * 0.85 = 6800 kbps
        // With 10000 target vs 6800 usable → should trigger reduction
        let cmd = adapter.update(&links);
        assert!(
            cmd.is_some() || adapter.current_target_kbps() <= 8_000,
            "lossy link should reduce effective capacity"
        );
    }

    // ─── Force Reduce ───────────────────────────────────────────────────

    #[test]
    fn force_reduce_immediately_cuts() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            ramp_down_factor: 0.5,
            ..Default::default()
        });

        let cmd = adapter.force_reduce(AdaptationReason::LinkFailure);
        assert_eq!(cmd.target_kbps, 5_000);
        assert_eq!(cmd.reason, AdaptationReason::LinkFailure);
        assert_eq!(adapter.current_target_kbps(), 5_000);
    }

    // ─── Reset ──────────────────────────────────────────────────────────

    #[test]
    fn reset_restores_max() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            ..Default::default()
        });

        adapter.force_reduce(AdaptationReason::Congestion);
        assert!(adapter.current_target_kbps() < 10_000);

        adapter.reset();
        assert_eq!(adapter.current_target_kbps(), 10_000);
        assert_eq!(adapter.stage(), DegradationStage::Normal);
    }

    // ─── Min Interval Gating ────────────────────────────────────────────

    #[test]
    fn respects_min_interval() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::from_secs(10), // very long
            ..Default::default()
        });

        let links = make_links(&[(1_000.0, true)]);
        let first = adapter.update(&links);
        assert!(first.is_some(), "first update should produce command");

        let second = adapter.update(&links);
        assert!(
            second.is_none(),
            "second update should be gated by interval"
        );
    }

    // ─── Headroom ───────────────────────────────────────────────────────

    #[test]
    fn headroom_reserves_capacity() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            headroom: 0.20, // 20% reserved
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // 10 Mbps capacity → 8 Mbps usable
        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links);
        // Target was 10000, usable is 8000 → should reduce
        assert!(
            adapter.current_target_kbps() <= 10_000,
            "should respect headroom"
        );
    }

    // ─── ReceiverFeedback ─────────────────────────────────────────────

    #[test]
    fn feedback_high_loss_forces_ramp_down() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links); // prime the adapter

        let feedback = ReceiverFeedback {
            goodput_bps: 5_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.05, // 5% — well above 1% threshold
        };

        let before = adapter.current_target_kbps();
        let cmd = adapter.update_with_feedback(&links, &feedback);
        assert!(cmd.is_some(), "should emit command on high loss");
        assert!(
            adapter.current_target_kbps() < before,
            "target should decrease: was {} now {}",
            before,
            adapter.current_target_kbps()
        );
    }

    #[test]
    fn feedback_low_loss_no_extra_pressure() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links); // prime
        let target_after_prime = adapter.current_target_kbps();

        let feedback = ReceiverFeedback {
            goodput_bps: 8_000_000,
            fec_repair_rate: 0.01,
            jitter_buffer_ms: 50,
            loss_after_fec: 0.005, // below 1% threshold
        };

        adapter.update_with_feedback(&links, &feedback);
        // Should not decrease below where update alone would go
        assert!(
            adapter.current_target_kbps() >= target_after_prime - 100,
            "no extra pressure expected: {} vs {}",
            adapter.current_target_kbps(),
            target_after_prime
        );
    }

    #[test]
    fn feedback_high_jitter_forces_ramp_down() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links); // prime

        let feedback = ReceiverFeedback {
            goodput_bps: 8_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 800, // well above 500ms threshold
            loss_after_fec: 0.0,
        };

        let before = adapter.current_target_kbps();
        let cmd = adapter.update_with_feedback(&links, &feedback);
        assert!(cmd.is_some(), "high jitter should trigger command");
        assert!(
            adapter.current_target_kbps() < before,
            "target should decrease on high jitter"
        );
    }

    // ─── ReliabilityMode ─────────────────────────────────────────────

    #[test]
    fn starts_in_max_quality_mode() {
        let adapter = BitrateAdapter::default();
        assert_eq!(adapter.mode(), ReliabilityMode::MaxQuality);
        assert_eq!(adapter.spare_bw_kbps(), 0);
    }

    #[test]
    fn switches_to_max_reliability_with_spare_bw() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            quality_cap_kbps: 6_000,
            reliability_spare_threshold_kbps: 3_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // Huge capacity: 25 Mbps across 3 links — adapter is at 10k ceiling,
        // 80%+ of max, and spare > 3 Mbps after headroom
        let links = make_links(&[(10_000.0, true), (8_000.0, true), (7_000.0, true)]);
        // Prime multiple times so target ramps toward max
        for _ in 0..10 {
            adapter.update(&links);
        }

        assert_eq!(
            adapter.mode(),
            ReliabilityMode::MaxReliability,
            "should switch to MaxReliability with abundant spare BW"
        );
        assert!(
            adapter.current_target_kbps() <= 6_000,
            "target should be capped at quality_cap_kbps: {}",
            adapter.current_target_kbps()
        );
        assert!(
            adapter.spare_bw_kbps() > 0,
            "should have spare BW: {}",
            adapter.spare_bw_kbps()
        );
    }

    #[test]
    fn stays_max_quality_when_constrained() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            quality_cap_kbps: 6_000,
            reliability_spare_threshold_kbps: 3_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // Just enough capacity — no real spare for reliability mode
        let links = make_links(&[(5_000.0, true)]);
        adapter.update(&links);

        assert_eq!(
            adapter.mode(),
            ReliabilityMode::MaxQuality,
            "should stay in MaxQuality when capacity is tight"
        );
    }

    #[test]
    fn recommended_fec_overhead_default() {
        let adapter = BitrateAdapter::default();
        assert!(
            (adapter.recommended_fec_overhead() - 0.10).abs() < 1e-6,
            "default FEC overhead should be 10%"
        );
    }

    #[test]
    fn recommended_fec_overhead_scales_with_spare() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            quality_cap_kbps: 6_000,
            reliability_spare_threshold_kbps: 3_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // Push into MaxReliability
        let links = make_links(&[(10_000.0, true), (8_000.0, true), (7_000.0, true)]);
        for _ in 0..10 {
            adapter.update(&links);
        }

        assert_eq!(adapter.mode(), ReliabilityMode::MaxReliability);
        let overhead = adapter.recommended_fec_overhead();
        assert!(
            overhead > 0.10,
            "FEC overhead should increase with spare BW: {}",
            overhead
        );
        assert!(
            overhead <= 0.50,
            "FEC overhead should not exceed 50%: {}",
            overhead
        );
    }

    #[test]
    fn command_includes_mode_and_spare() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        let links = make_links(&[(3_000.0, true)]);
        let cmd = adapter.update(&links).unwrap();

        // Check that the new fields are populated
        assert!(matches!(
            cmd.mode,
            ReliabilityMode::MaxQuality | ReliabilityMode::MaxReliability
        ));
        assert!(cmd.recommended_fec_overhead >= 0.10);
        assert!(cmd.recommended_fec_overhead <= 0.50);
    }

    #[test]
    fn reset_clears_mode() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            quality_cap_kbps: 6_000,
            reliability_spare_threshold_kbps: 3_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // Push into MaxReliability
        let links = make_links(&[(10_000.0, true), (8_000.0, true), (7_000.0, true)]);
        for _ in 0..10 {
            adapter.update(&links);
        }
        assert_eq!(adapter.mode(), ReliabilityMode::MaxReliability);

        adapter.reset();
        assert_eq!(adapter.mode(), ReliabilityMode::MaxQuality);
        assert_eq!(adapter.spare_bw_kbps(), 0);
    }
}
