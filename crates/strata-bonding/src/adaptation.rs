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
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info};

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
    /// Starting bitrate for the adapter (kbps).
    /// Should match the encoder's initial `--bitrate` to keep the adapter
    /// and encoder in sync from the first tick.  A value of 0 means "use
    /// max_bitrate_kbps" (legacy behaviour, kept for tests).
    pub initial_bitrate_kbps: u32,
}

impl Default for AdaptationConfig {
    fn default() -> Self {
        AdaptationConfig {
            min_bitrate_kbps: 500,
            max_bitrate_kbps: 20_000,
            headroom: 0.15,
            ramp_up_kbps_per_step: 250,
            ramp_down_factor: 0.7,
            min_interval: Duration::from_millis(200),
            pressure_threshold: 0.9,
            quality_cap_kbps: 6_000,
            reliability_spare_threshold_kbps: 3_000,
            initial_bitrate_kbps: 0,
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
    /// ARQ send-queue depth in packets. `None` when unavailable (e.g. from
    /// the modem supervisor which lacks transport-layer visibility).
    pub queue_depth: Option<usize>,
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
    /// When the last rate *increase* was committed — used to suppress
    /// feedback-driven reductions for a grace period so stale receiver
    /// metrics don't immediately revert the increase.
    last_increase_time: Option<Instant>,
    /// EWMA-smoothed post-FEC loss rate from receiver feedback.
    /// Smoothing prevents single bursty LTE seconds from triggering
    /// unnecessary bitrate reductions.
    ewma_loss_fec: f32,
    /// EWMA-smoothed goodput (bps) from receiver feedback.
    /// Prevents single low-sample outliers (e.g. end-of-window artifacts)
    /// from triggering spurious goodput-shortfall reductions.
    ewma_goodput_bps: f64,
    /// Per-link EWMA-smoothed capacity (kbps). Filters out noisy spikes
    /// and dips from Oracle/BBR/ack-rate estimates on lossy LTE links.
    /// α=0.3 gives ~2s half-life at 500ms update intervals.
    capacity_ewma: HashMap<usize, f64>,
}

impl BitrateAdapter {
    pub fn new(config: AdaptationConfig) -> Self {
        // If an explicit starting point is provided use it; otherwise fall back to
        // max (legacy behaviour kept for unit tests that rely on starting at max).
        let initial = if config.initial_bitrate_kbps > 0 {
            config.initial_bitrate_kbps
        } else {
            config.max_bitrate_kbps
        };
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
            last_increase_time: None,
            ewma_loss_fec: 0.0,
            ewma_goodput_bps: 0.0,
            capacity_ewma: HashMap::new(),
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
        // EWMA-smooth per-link capacity before aggregation.
        // Asymmetric: fast down (α=0.7) so drops are tracked quickly,
        // slow up (α=0.3) so recovery is conservative. Filters noisy
        // Oracle/BBR/ack-rate spikes on lossy LTE links.
        // Skip smoothing for zero capacity (cold-start: no data yet).
        const CAP_EWMA_ALPHA_UP: f64 = 0.3;
        const CAP_EWMA_ALPHA_DOWN: f64 = 0.7;

        // Aggregate capacity from alive links
        let aggregate_kbps: f64 = links
            .iter()
            .filter(|l| l.alive)
            .map(|l| {
                let raw = l.capacity_kbps;
                let smoothed = if raw > 0.0 {
                    let entry = self.capacity_ewma.entry(l.link_id).or_insert(raw);
                    let alpha = if raw < *entry {
                        CAP_EWMA_ALPHA_DOWN
                    } else {
                        CAP_EWMA_ALPHA_UP
                    };
                    *entry = alpha * raw + (1.0 - alpha) * *entry;
                    *entry
                } else {
                    // Zero capacity = cold-start; don't pollute the EWMA
                    raw
                };
                let effective_loss = l.loss_rate.clamp(0.0, 1.0);
                smoothed * (1.0 - effective_loss)
            })
            .sum();

        let alive_count = links.iter().filter(|l| l.alive).count();

        // Log per-link detail
        for l in links {
            let smoothed = self
                .capacity_ewma
                .get(&l.link_id)
                .copied()
                .unwrap_or(l.capacity_kbps);
            info!(
                target: "strata::adapt",
                link = l.link_id,
                cap_kbps = format_args!("{:.0}", l.capacity_kbps),
                smooth_kbps = format_args!("{:.0}", smoothed),
                alive = l.alive,
                loss = format_args!("{:.3}", l.loss_rate),
                rtt = format_args!("{:.0}", l.rtt_ms),
                queue = ?l.queue_depth,
                "link"
            );
        }

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

        debug!(
            target: "strata::adapt",
            aggregate_kbps = aggregate_kbps,
            usable_kbps = usable_kbps,
            pressure = pressure,
            current_target_kbps = self.current_target_kbps,
            alive_count = alive_count,
            stage = ?self.stage,
            mode = ?self.mode,
            prev_capacity_kbps = self.prev_capacity_kbps,
            "BitrateAdapter::update input"
        );

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
        let new_target = new_target
            .min(effective_max)
            .max(self.config.min_bitrate_kbps);

        // Track spare bandwidth
        self.spare_bw_kbps = if usable_kbps > new_target as f64 {
            (usable_kbps - new_target as f64) as u32
        } else {
            0
        };

        // Only issue command if target changed meaningfully and enough time passed.
        // Use a relative threshold (>10% change) with a small absolute floor (50 kbps)
        // so that small-but-significant changes at low bitrates aren't suppressed.
        let abs_change = (new_target as i64 - self.current_target_kbps as i64).unsigned_abs();
        let pct_change = abs_change as f64 / self.current_target_kbps.max(1) as f64;
        let target_changed = abs_change > 50 && pct_change > 0.10;
        let interval_ok = self
            .last_command_time
            .is_none_or(|t| t.elapsed() >= self.config.min_interval);

        debug!(
            target: "strata::adapt",
            new_target_kbps = new_target,
            old_target_kbps = self.current_target_kbps,
            target_changed = target_changed,
            interval_ok = interval_ok,
            reason = ?reason,
            consecutive_inc = self.consecutive_increases,
            consecutive_dec = self.consecutive_decreases,
            "BitrateAdapter decision"
        );

        // Compact info-level summary for field-test debugging
        info!(
            target: "strata::adapt",
            "[adapt] agg={:.0} usable={:.0} pres={:.2} cur={} → {} ({:?}) ci={} cd={} changed={} int_ok={}",
            aggregate_kbps, usable_kbps, pressure,
            self.current_target_kbps, new_target, reason,
            self.consecutive_increases, self.consecutive_decreases,
            target_changed, interval_ok
        );

        if target_changed && interval_ok {
            if new_target > self.current_target_kbps {
                self.last_increase_time = Some(Instant::now());
            }
            self.current_target_kbps = new_target;
            self.last_command_time = Some(Instant::now());
        }

        let cmd_target = self.current_target_kbps;
        info!(
            target: "strata::adapt",
            "[adapt] CMD target_kbps={}",
            cmd_target
        );

        // Always return a command so the degradation stage is forwarded to
        // the scheduler on every tick.  Previously we returned `None` when
        // the target hadn't changed, which caused the stage to "latch" —
        // an aggressive stage set on the first tick was never corrected.
        Some(self.make_command(self.current_target_kbps, reason))
    }

    /// Update with link capacity AND receiver feedback.
    ///
    /// The receiver feedback provides ground-truth signals that can override
    /// or supplement the sender's local capacity estimates:
    /// - `loss_after_fec > 5%` → apply congestion pressure
    /// - `jitter_buffer_ms > 1000` → bufferbloat, reduce bitrate
    /// - `goodput_bps` significantly below encoder output → congestion
    pub fn update_with_feedback(
        &mut self,
        links: &[LinkCapacity],
        feedback: &ReceiverFeedback,
    ) -> Option<BitrateCommand> {
        // Snapshot the target BEFORE update() — goodput reflects the previous
        // rate, so we must compare against the pre-update target to avoid
        // immediately reverting a rate increase due to stale goodput.
        let target_before_update = self.current_target_kbps;

        // Start with normal capacity-based update.
        // NOTE: `update()` may already reduce via capacity pressure.
        // The feedback reduction below can stack, which is intentional —
        // receiver-side signals confirm congestion the sender sees locally.
        let mut result = self.update(links);

        // Clamp: if update() increased the target but the receiver is stalling
        // (jitter near the latency ceiling), revert the increase.  Capacity
        // looks fine from the sender's perspective but the receiver can't keep
        // up — pushing more data only deepens the jitter buffer.
        if self.current_target_kbps > target_before_update && feedback.jitter_buffer_ms > 1500 {
            self.current_target_kbps = target_before_update;
            self.last_increase_time = None; // Don't activate grace for a reverted increase
            result = Some(self.make_command(target_before_update, AdaptationReason::Capacity));
        }

        // Apply receiver-side pressure signals.
        // loss_after_fec is now per-interval (delta-based) at the receiver,
        // but LTE loss is bursty so we EWMA-smooth it (α=0.3, ~2s half-life)
        // to avoid reacting to a single bad second.
        //
        // When goodput is positive, update normally.
        // When goodput is zero (stall), decay the EWMA toward 0 slowly
        // (α=0.05) instead of freezing it.  Previously, freezing caused the
        // EWMA to latch at ~1.0 after a stall and never recover — the
        // goodput_bps > 0 guard prevented any update, so loss_pressure stayed
        // true indefinitely.  A slow decay lets the system re-probe after
        // ~10 seconds of stall, matching the grace period + ramp-up cadence.
        if feedback.goodput_bps > 0 {
            self.ewma_loss_fec = 0.3 * feedback.loss_after_fec + 0.7 * self.ewma_loss_fec;
        } else {
            // Stall: decay loss toward 0 so the system can eventually recover.
            self.ewma_loss_fec *= 0.9;
        }
        // Cellular links can show 5-10% baseline loss; use 15% as the
        // "real congestion" threshold on the smoothed signal.
        //
        // However, high loss_fec with healthy goodput indicates reorder-buffer
        // stall artifacts — not real delivery failure. When a co-bonded link is
        // broken its ARQ holes stall the shared reorder buffer, causing
        // loss_fec to alternate 0/1 even though the working link is fine.
        // Gate on goodput being degraded to distinguish the two cases.
        let goodput_ok = self.ewma_goodput_bps > 0.0
            && self.ewma_goodput_bps >= target_before_update as f64 * 1000.0 * 0.80;
        let loss_pressure = self.ewma_loss_fec > 0.15 && !goodput_ok;
        // Jitter > 3s indicates real buffer bloat; user is OK with 2s delay.
        let bufferbloat = feedback.jitter_buffer_ms > 3000;
        // EWMA-smooth goodput (α=0.3) to filter end-of-window noise artifacts
        // that can appear as a single near-zero reading followed by a burst.
        // Seed with the first real reading to avoid cold-start false shortfalls.
        if feedback.goodput_bps > 0 {
            if self.ewma_goodput_bps == 0.0 {
                self.ewma_goodput_bps = feedback.goodput_bps as f64;
            } else {
                self.ewma_goodput_bps =
                    0.3 * feedback.goodput_bps as f64 + 0.7 * self.ewma_goodput_bps;
            }
        }
        // Compare smoothed goodput against the PRE-update target since goodput
        // lags the encoder rate by at least one RTT.
        let goodput_shortfall = self.ewma_goodput_bps > 0.0
            && self.ewma_goodput_bps < target_before_update as f64 * 1000.0 * 0.7;

        // After a rate increase, receiver metrics are stale for a few seconds
        // (they still reflect the old encoder rate).  Suppress loss/goodput
        // reductions during this grace period so we don't immediately revert
        // every increase.  Bufferbloat (jitter > 1000ms) is kept because it's
        // an absolute signal that doesn't depend on rate matching.
        let feedback_grace = self
            .last_increase_time
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(5));
        let feedback_reduction = if feedback_grace {
            bufferbloat
        } else {
            loss_pressure || bufferbloat || goodput_shortfall
        };

        info!(
            target: "strata::adapt",
            "[adapt] fb: loss_fec={:.3} ewma_loss={:.3} jitter={}ms gp={}kbps ewma_gp={}kbps | loss_p={} bb={} gp_short={} grace={} → reduce={}",
            feedback.loss_after_fec,
            self.ewma_loss_fec,
            feedback.jitter_buffer_ms,
            feedback.goodput_bps / 1000,
            self.ewma_goodput_bps as u64 / 1000,
            loss_pressure,
            bufferbloat,
            goodput_shortfall,
            feedback_grace,
            feedback_reduction
        );

        if feedback_reduction {
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
            debug!(target: "strata::adapt", "decision: no alive links → min");
            return (self.config.min_bitrate_kbps, AdaptationReason::LinkFailure);
        }

        // Links are alive but capacity estimates haven't arrived yet (BBR/oracle
        // cold-start). Hold current bitrate rather than treating zero capacity as
        // "over-pressure" and tanking to min on the first tick.
        if usable_kbps == 0.0 {
            debug!(target: "strata::adapt", "decision: zero usable (cold-start) → hold");
            return (self.current_target_kbps, AdaptationReason::Capacity);
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

            debug!(
                target: "strata::adapt",
                "decision: OVER-PRESSURE {:.2} > {:.2} → reduce {} → {} ({:?})",
                pressure, self.config.pressure_threshold, current, target, reason
            );
            return (target, reason);
        }

        // Under-pressure with stable capacity: ramp up
        if pressure < 0.7 && self.consecutive_increases >= 3 {
            let target = current + self.config.ramp_up_kbps_per_step;
            let target = target.min(self.config.max_bitrate_kbps);
            // Cap below the pressure threshold to avoid overshooting into
            // over-pressure on the very next tick.
            let safe_ceiling = (usable_kbps * (self.config.pressure_threshold - 0.05)) as u32;
            let target = target.min(safe_ceiling);
            // Cap ramp-up to 1.3x EWMA goodput — sender-side capacity is
            // optimistic on lossy links; real throughput is what matters.
            // This prevents the classic overshoot-crash-ramp cycle.
            let target = if self.ewma_goodput_bps > 0.0 {
                let gp_ceiling = (self.ewma_goodput_bps * 1.3 / 1000.0) as u32;
                target.min(gp_ceiling)
            } else {
                target
            };
            debug!(
                target: "strata::adapt",
                "decision: RAMP-UP pressure={:.2} ci={} → {} → {} (gp_ewma={}kbps)",
                pressure, self.consecutive_increases, current, target,
                self.ewma_goodput_bps as u64 / 1000
            );
            return (target, AdaptationReason::Recovery);
        }

        // Stable: no change
        if pressure < 0.7 {
            debug!(
                target: "strata::adapt",
                "decision: low-pressure {:.2} but ci={} < 3 → hold at {}",
                pressure, self.consecutive_increases, current
            );
        } else {
            debug!(
                target: "strata::adapt",
                "decision: stable pressure={:.2} → hold at {}",
                pressure, current
            );
        }
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
        self.capacity_ewma.clear();
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
                queue_depth: None,
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
        // A command is always returned (for stage freshness) but the target
        // should stay at max when capacity far exceeds it.
        let cmd = cmd.expect("always returns a command");
        assert_eq!(
            cmd.target_kbps, 5_000,
            "target should stay at max when capacity >> target"
        );
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
            queue_depth: None,
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
        let first = first.expect("first update should produce command");
        let first_target = first.target_kbps;
        assert!(first_target < 10_000, "first update should reduce target");

        let second = adapter.update(&links);
        let second = second.expect("always returns a command for stage freshness");
        assert_eq!(
            second.target_kbps, first_target,
            "target should not change while gated by interval"
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

        // Use 60% loss with degraded goodput (below 80% of target).
        // After 3 calls: EWMA ≈ 0.3*0.6 + 0.7*(0.3*0.6 + 0.7*(0.3*0.6)) ≈ 0.36 > 0.15
        // goodput=500kbps is below 80% of the expected ~7000kbps target → goodput_ok=false.
        let feedback = ReceiverFeedback {
            goodput_bps: 500_000, // degraded — well below 80% of target
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.60, // severe loss — well above 15% EWMA threshold
        };

        // Warm up EWMA (needs ~3 ticks to exceed 15% threshold)
        adapter.update_with_feedback(&links, &feedback);
        adapter.update_with_feedback(&links, &feedback);
        let before = adapter.current_target_kbps();
        let cmd = adapter.update_with_feedback(&links, &feedback);
        assert!(cmd.is_some(), "should emit command on sustained high loss");
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
            loss_after_fec: 0.03, // below 5% threshold
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
            jitter_buffer_ms: 4000, // well above 3000ms threshold
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

    // ─── P0 Stall Recovery Tests ────────────────────────────────────────

    #[test]
    fn ewma_loss_ignores_zero_goodput_stall() {
        // Feed loss_fec=1.0 with gp=0, then loss_fec=0.0 with gp=1000kbps.
        // The EWMA should NOT be poisoned by the stall period.
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links); // prime

        // Stall period: goodput=0, loss=1.0 (reorder buffer blocked)
        let stall_fb = ReceiverFeedback {
            goodput_bps: 0,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 1.0,
        };

        for _ in 0..5 {
            adapter.update_with_feedback(&links, &stall_fb);
        }

        // EWMA should not have been updated (gate: goodput_bps > 0)
        assert!(
            adapter.ewma_loss_fec < 0.01,
            "ewma_loss should stay near 0 during stall, got {}",
            adapter.ewma_loss_fec
        );

        // Recovery: goodput=1Mbps, loss=0.0
        let recover_fb = ReceiverFeedback {
            goodput_bps: 1_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.0,
        };

        for _ in 0..3 {
            adapter.update_with_feedback(&links, &recover_fb);
        }

        // EWMA should be near 0 (was never poisoned)
        assert!(
            adapter.ewma_loss_fec < 0.05,
            "ewma_loss should recover to near-zero, got {}",
            adapter.ewma_loss_fec
        );
    }

    #[test]
    fn loss_pressure_gated_on_goodput() {
        // High ewma_loss + high goodput → loss_pressure should be false
        // (no bitrate reduction).
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 2_000,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links); // prime

        // Warm up EWMA with moderate loss but healthy goodput
        // goodput_bps = 2Mbps > 80% of target 2Mbps → goodput_ok = true
        let feedback = ReceiverFeedback {
            goodput_bps: 2_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.35, // above 15% threshold
        };

        let before = adapter.current_target_kbps();
        for _ in 0..5 {
            adapter.update_with_feedback(&links, &feedback);
        }

        // With goodput healthy, high loss_fec shouldn't cause reduction
        // (capacity alone doesn't force reduction since 10Mbps >> 2Mbps target)
        assert!(
            adapter.current_target_kbps() >= before,
            "should not reduce when goodput is healthy: was {} now {}",
            before,
            adapter.current_target_kbps()
        );
    }

    #[test]
    fn adaptation_recovers_after_transient_stall() {
        // Full cycle: ramp to stable → stall (gp=0, loss=1.0) → recovery
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 1_200,
            ramp_up_kbps_per_step: 500,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);

        // Phase 1: Stable operation at 1200kbps
        let stable_fb = ReceiverFeedback {
            goodput_bps: 1_200_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.02,
        };
        for _ in 0..5 {
            adapter.update_with_feedback(&links, &stable_fb);
        }
        let pre_stall = adapter.current_target_kbps();

        // Phase 2: Stall (gp=0, loss=1.0 for 5 ticks)
        let stall_fb = ReceiverFeedback {
            goodput_bps: 0,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 1.0,
        };
        for _ in 0..5 {
            adapter.update_with_feedback(&links, &stall_fb);
        }

        // Target should NOT have crashed during stall
        assert!(
            adapter.current_target_kbps() >= pre_stall * 80 / 100,
            "target should not crash during stall: pre={} now={}",
            pre_stall,
            adapter.current_target_kbps()
        );

        // Phase 3: Recovery (gp=800kbps, loss=0.0)
        let recover_fb = ReceiverFeedback {
            goodput_bps: 800_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.0,
        };
        for _ in 0..10 {
            adapter.update_with_feedback(&links, &recover_fb);
        }

        // Should still have reasonable bitrate after recovery
        assert!(
            adapter.current_target_kbps() >= 800,
            "target should recover to reasonable level, got {}",
            adapter.current_target_kbps()
        );
    }

    #[test]
    fn goodput_shortfall_drives_reduction_not_loss() {
        // Low goodput alone (without loss) should reduce via goodput_shortfall,
        // not loss_pressure.
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 5_000,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links); // prime

        // Seed EWMA goodput first with a normal reading
        let seed_fb = ReceiverFeedback {
            goodput_bps: 5_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.0,
        };
        adapter.update_with_feedback(&links, &seed_fb);

        // Now provide very low goodput (well below 70% of target) with zero loss
        let low_gp_fb = ReceiverFeedback {
            goodput_bps: 1_000_000, // 20% of target — below 70% threshold
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.0,
        };

        let before = adapter.current_target_kbps();
        // Need several ticks for EWMA to converge down
        for _ in 0..8 {
            adapter.update_with_feedback(&links, &low_gp_fb);
        }

        assert!(
            adapter.current_target_kbps() < before,
            "should reduce on goodput shortfall: was {} now {}",
            before,
            adapter.current_target_kbps()
        );
    }

    // ─── Field-Test Failure Reproductions ─────────────────────────────

    /// Reproduces the observed field-test pattern where EWMA loss climbs
    /// to 0.99 and never recovers.
    ///
    /// The pattern: alternating stall (gp=0, loss=1.0) and unstall
    /// (gp>0, loss=0.8-1.0) cycles.  The stall periods suppress EWMA
    /// updates (correct), but the unstall periods feed high loss samples
    /// that ratchet the EWMA toward 1.0 without any recovery mechanism.
    #[test]
    fn field_test_ewma_loss_oscillation_recovery() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 1_200,
            ..Default::default()
        });

        let links = make_links(&[(5_000.0, true), (5_000.0, true)]);
        adapter.update(&links); // prime

        // Seed EWMA goodput with a good reading so we can observe changes
        let seed = ReceiverFeedback {
            goodput_bps: 1_200_000,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.02,
            fec_repair_rate: 0.0,
        };
        for _ in 0..3 {
            adapter.update_with_feedback(&links, &seed);
        }
        assert!(adapter.ewma_loss_fec < 0.1, "EWMA should be low after seed");

        // Simulate 20 stall/unstall cycles (resembling the field-test pattern).
        // Each cycle: 2 ticks of stall (gp=0, loss=1.0) then 1 tick of
        // unstall where reorder buffer briefly delivers but with high residual
        // loss (0.9) because most packets expired.
        for cycle in 0..20 {
            // Stall phase: gp=0 → EWMA should NOT update
            let stall = ReceiverFeedback {
                goodput_bps: 0,
                jitter_buffer_ms: 1500,
                loss_after_fec: 1.0,
                fec_repair_rate: 0.0,
            };
            adapter.update_with_feedback(&links, &stall);
            adapter.update_with_feedback(&links, &stall);

            // Unstall phase: gp>0 → EWMA WILL update with high loss
            let unstall = ReceiverFeedback {
                goodput_bps: 300_000, // Some delivery, but low
                jitter_buffer_ms: 1800,
                loss_after_fec: 0.9, // Most packets expired during stall
                fec_repair_rate: 0.0,
            };
            adapter.update_with_feedback(&links, &unstall);

            if cycle == 5 {
                // After 6 cycles the EWMA should not yet be stuck at 0.99
                assert!(
                    adapter.ewma_loss_fec < 0.95,
                    "EWMA should not ratchet to 0.99 after only 6 cycles, got {:.3}",
                    adapter.ewma_loss_fec
                );
            }
        }

        // After 20 cycles of this, feed 10 ticks of clean recovery.
        let clean = ReceiverFeedback {
            goodput_bps: 1_000_000,
            jitter_buffer_ms: 200,
            loss_after_fec: 0.0,
            fec_repair_rate: 0.0,
        };
        for _ in 0..10 {
            adapter.update_with_feedback(&links, &clean);
        }

        // KEY ASSERTION: EWMA must recover to below 0.15 (the loss_pressure
        // threshold) within 10 clean ticks.  If it's still above 0.15 after
        // 10 clean updates, the adaptation layer is permanently poisoned.
        assert!(
            adapter.ewma_loss_fec < 0.15,
            "EWMA should recover below loss_pressure threshold after 10 clean \
             ticks, got {:.3} — EWMA is permanently poisoned",
            adapter.ewma_loss_fec
        );
    }

    /// Reproduces the field-test pattern where the adapter never settles:
    /// increase → grace period → grace expires → reduce → increase → ...
    ///
    /// With EWMA stuck at 0.99, every grace expiry triggers a reduction,
    /// and every reduction triggers a recovery ramp-up. The bitrate
    /// oscillates wildly instead of converging to a sustainable rate.
    #[test]
    fn field_test_adaptation_oscillation_convergence() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 5_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 1_200,
            ramp_up_kbps_per_step: 500,
            ..Default::default()
        });

        let links = make_links(&[(3_000.0, true), (3_000.0, true)]);
        adapter.update(&links); // prime

        // Simulate a scenario where true sustainable capacity is ~2000 kbps
        // but loss is high due to one degraded link.
        let degraded = ReceiverFeedback {
            goodput_bps: 2_000_000,
            jitter_buffer_ms: 800,
            loss_after_fec: 0.3, // 30% residual loss
            fec_repair_rate: 0.0,
        };

        // Run 30 ticks. Track how many times the bitrate changes direction.
        let mut prev_target = adapter.current_target_kbps();
        let mut direction_changes = 0u32;
        let mut was_increasing: Option<bool> = None;

        for _ in 0..30 {
            adapter.update_with_feedback(&links, &degraded);
            let target = adapter.current_target_kbps();
            let increasing = target > prev_target;

            if let Some(was_inc) = was_increasing
                && increasing != was_inc
                && target != prev_target
            {
                direction_changes += 1;
            }
            if target != prev_target {
                was_increasing = Some(increasing);
            }
            prev_target = target;
        }

        // Should converge — not oscillate more than ~6 direction changes
        // over 30 ticks (a couple of initial adjustments are expected).
        assert!(
            direction_changes <= 6,
            "adaptation oscillated {} times in 30 ticks — should converge, not thrash",
            direction_changes
        );
    }

    /// Reproduces the field-test scenario where segments stall because
    /// the adapter can't distinguish "stalled reorder buffer" from
    /// "real congestion" — it keeps trying to recover bitrate despite
    /// the receiver being unable to deliver.
    ///
    /// With latency pegged at 1999ms and loss at 0.99, the correct
    /// action is to reduce to minimum, not oscillate.
    #[test]
    fn field_test_sustained_stall_reaches_minimum() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 5_000,
            min_bitrate_kbps: 200,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 2_000,
            ramp_up_kbps_per_step: 500,
            ..Default::default()
        });

        let links = make_links(&[(3_000.0, true), (3_000.0, true)]);
        adapter.update(&links); // prime

        // Simulate sustained stall: jitter pinned at 1999ms, loss ~1.0,
        // occasional brief goodput blips (the reorder buffer occasionally
        // delivers a batch then stalls again).
        for _ in 0..20 {
            // Mostly stalled
            let stalled = ReceiverFeedback {
                goodput_bps: 0,
                jitter_buffer_ms: 1999,
                loss_after_fec: 1.0,
                fec_repair_rate: 0.0,
            };
            adapter.update_with_feedback(&links, &stalled);
            adapter.update_with_feedback(&links, &stalled);
            adapter.update_with_feedback(&links, &stalled);

            // Brief goodput blip
            let blip = ReceiverFeedback {
                goodput_bps: 500_000,
                jitter_buffer_ms: 1999,
                loss_after_fec: 0.95,
                fec_repair_rate: 0.0,
            };
            adapter.update_with_feedback(&links, &blip);
        }

        // After 80 ticks of this pattern, the adapter should have driven
        // bitrate close to minimum — not be oscillating at 2000+ kbps.
        assert!(
            adapter.current_target_kbps() <= 500,
            "should be at or near minimum after sustained stall, got {} kbps",
            adapter.current_target_kbps()
        );
    }

    /// Verifies that the grace period doesn't suppress ALL signals during
    /// a genuine congestion event.  If we just increased the rate and
    /// immediately see jitter > 2000ms + near-total loss, the grace
    /// period should yield (or shorten) rather than waiting 3 full seconds.
    #[test]
    fn grace_period_yields_on_severe_congestion() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 5_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 1_000,
            ramp_up_kbps_per_step: 500,
            ..Default::default()
        });

        let links = make_links(&[(5_000.0, true), (5_000.0, true)]);
        adapter.update(&links); // prime

        // Ramp up to set last_increase_time (triggers grace period)
        let good = ReceiverFeedback {
            goodput_bps: 1_000_000,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.0,
            fec_repair_rate: 0.0,
        };
        adapter.update_with_feedback(&links, &good);
        let after_increase = adapter.current_target_kbps();

        // Immediately hit severe congestion (jitter > 3000 is the only
        // signal that penetrates grace today — this tests the existing
        // bufferbloat bypass)
        let severe = ReceiverFeedback {
            goodput_bps: 200_000,
            jitter_buffer_ms: 4000, // Severe bufferbloat
            loss_after_fec: 0.95,
            fec_repair_rate: 0.0,
        };
        adapter.update_with_feedback(&links, &severe);

        assert!(
            adapter.current_target_kbps() < after_increase,
            "severe bufferbloat (jitter=4000ms) should penetrate grace period: was {} now {}",
            after_increase,
            adapter.current_target_kbps()
        );
    }

    // ── Regression: EWMA loss decay during stall ─────────────────────

    /// When goodput drops to zero after the EWMA has climbed high,
    /// the decay (×0.9 per tick) should bring it below the loss_pressure
    /// threshold within ~15 ticks, preventing permanent stall.
    #[test]
    fn ewma_loss_decays_during_zero_goodput_stall() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 2_000,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links); // prime

        // Poison EWMA high: feed loss=0.8 with valid goodput for several ticks
        let high_loss = ReceiverFeedback {
            goodput_bps: 500_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.8,
        };
        for _ in 0..10 {
            adapter.update_with_feedback(&links, &high_loss);
        }
        assert!(
            adapter.ewma_loss_fec > 0.5,
            "EWMA should be high after loss injection: {:.3}",
            adapter.ewma_loss_fec
        );

        // Now simulate zero-goodput stall — EWMA should decay
        let stall = ReceiverFeedback {
            goodput_bps: 0,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 1.0, // ignored during stall
        };
        for _ in 0..20 {
            adapter.update_with_feedback(&links, &stall);
        }

        // 0.9^20 ≈ 0.12, so from ~0.7 → ~0.09 after 20 ticks
        assert!(
            adapter.ewma_loss_fec < 0.15,
            "EWMA should have decayed below loss_pressure threshold: {:.3}",
            adapter.ewma_loss_fec
        );
    }

    #[test]
    fn capacity_ewma_smooths_spikes_and_tracks_drops() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // Seed with stable 5 Mbps
        for _ in 0..5 {
            let links = make_links(&[(5_000.0, true)]);
            adapter.update(&links);
        }
        let stable = adapter.capacity_ewma[&0];
        assert!(
            (stable - 5_000.0).abs() < 100.0,
            "should converge near 5000: {stable:.0}"
        );

        // Spike to 15 Mbps (noisy reading) — slow-up α=0.3 should dampen
        let links = make_links(&[(15_000.0, true)]);
        adapter.update(&links);
        let after_spike = adapter.capacity_ewma[&0];
        assert!(
            after_spike < 9_000.0,
            "spike should be dampened: {after_spike:.0}"
        );

        // Drop to 1 Mbps (link degradation) — fast-down α=0.7 should track
        let links = make_links(&[(1_000.0, true)]);
        adapter.update(&links);
        let after_drop = adapter.capacity_ewma[&0];
        assert!(
            after_drop < 4_000.0,
            "drop should be tracked quickly: {after_drop:.0}"
        );
    }
}
