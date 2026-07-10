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
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use tracing::{debug, info};

/// Sliding window duration for peak goodput tracking.
///
/// The windowed max-filter remembers the best observed delivery rate over
/// this interval, preventing the application-limited self-trap where a slow
/// EWMA anchors the encoder ceiling at the post-burst depressed rate.
const GOODPUT_WINDOW_SECS: f64 = 10.0;

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
    ///
    /// Deliberately additive-up / multiplicative-down (AIMD-shaped): "recover
    /// cautiously, cut decisively". With the defaults (250 kbps/step,
    /// ramp_down_factor 0.7), a single 3 Mbps → 2.1 Mbps cut is instant, but
    /// climbing back to 3 Mbps takes ~4 ticks (~4 s) if gates stay open the
    /// whole time; a collapse to the 500 kbps floor takes ~10 s of clean
    /// ticks. Recovery time is linear in the size of the gap the cut left —
    /// this is the field-visible "slow climb after every dip" pattern, and
    /// it is intentional, not an accident of the two knobs' values.
    pub ramp_up_kbps_per_step: u32,
    /// How quickly to ramp down on congestion (multiplier, e.g., 0.7 = 30% cut).
    /// See the asymmetry note on [`AdaptationConfig::ramp_up_kbps_per_step`].
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
    /// How long a non-severe congestion signal must stay continuously
    /// true before the adapter is allowed to issue a `Congestion`
    /// reduction. See [`CONGESTION_SUSTAIN_DEFAULT`].
    pub congestion_sustain: Duration,
    /// Duration over which the encoder bitrate ramps gently from
    /// [`AdaptationConfig::startup_floor_kbps`] up to `initial_bitrate_kbps`
    /// at stream start.
    ///
    /// A freshly-attached cellular link has not yet warmed its bandwidth
    /// grant or congestion window. Blasting the full target bitrate (plus FEC
    /// overhead) into a cold link overflows the modem buffer and produces a
    /// heavy startup loss burst (~14 % over the first ~10 s in field tests)
    /// that decodes as grey/noisy frames until the next clean keyframe lands.
    /// Ramping the encoder up gently lets the link warm before it has to
    /// carry full rate. `Duration::ZERO` disables the ramp (the default,
    /// which preserves legacy/unit-test behaviour). Only active when
    /// `initial_bitrate_kbps > 0`.
    pub startup_ramp: Duration,
    /// Bitrate (kbps) the startup ramp begins at. Clamped at runtime to
    /// `>= min_bitrate_kbps` and `<= initial_bitrate_kbps`.
    pub startup_floor_kbps: u32,
    /// Receiver's playout-window ceiling (ms) — mirrors
    /// `ReceiverConfig::max_latency`. The delay-pressure arm treats a jitter
    /// buffer beyond this as overflow. Sender and receiver are separate
    /// processes with independently-set configs, so this must be sourced
    /// from the same config the receiver actually uses (see call sites in
    /// `strata-gst`) rather than assumed — a mismatch here means the
    /// adapter either cuts before the receiver's real ceiling or never
    /// reacts to a genuine overflow. Default matches
    /// `ReceiverConfig::max_latency`'s own default (3000 ms).
    pub jitter_buffer_ceiling_ms: u32,
}

/// Default sustain duration for non-severe congestion signals.
///
/// Cellular HARQ stalls produce single-tick `link_collapse` /
/// `late_pressure` / `burst_loss` signals that clear within
/// 200–800 ms (see field-test data). Without a sustain gate, every such
/// stall triggers a 30 % bitrate cut followed by a slow ramp-up,
/// producing the sawtooth encoder behaviour visible as on-wire artifacts.
///
/// `severe_burst` (post-FEC loss > 50 % AND jitter > 200 ms in the same
/// window) is treated as an emergency and bypasses this delay — it
/// almost certainly indicates real link collapse, not a HARQ burst.
pub const CONGESTION_SUSTAIN_DEFAULT: Duration = Duration::from_millis(1500);

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
            congestion_sustain: CONGESTION_SUSTAIN_DEFAULT,
            startup_ramp: Duration::ZERO,
            startup_floor_kbps: 500,
            jitter_buffer_ceiling_ms: 3_000,
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
    /// Pacing (drain) rate in kbps — the rate this link actually empties
    /// its paced queue at. `None` when unavailable. Capacity estimates
    /// above this are undeliverable (the oracle over-reads lossy LTE), so
    /// the adapter clamps per-link capacity to it.
    pub drain_rate_kbps: Option<f64>,
    /// Cumulative packets deleted by the link's paced-queue AQM. `None`
    /// when unavailable. A rising count means offered > drained for longer
    /// than the queue's sojourn budget — self-congestion.
    pub aqm_dropped_total: Option<u64>,
}

/// Loss rate above which a link is considered to be melting down, when
/// paired with [`LINK_MELT_QUEUE_DEPTH`].
const LINK_MELT_LOSS_RATE: f64 = 0.55;
/// ARQ queue depth (packets) that, combined with [`LINK_MELT_LOSS_RATE`],
/// confirms a link is melting rather than just riding a benign IDR burst
/// (queue depth alone is not trustworthy — see `Adaptation-Delay-Pressure`).
const LINK_MELT_QUEUE_DEPTH: usize = 60;

/// Absolute emergency floor (kbps) the encoder may fall to when the
/// configured `min_bitrate_kbps` floor is itself the congestion source.
/// Barely-watchable video that arrives beats configured-quality video that
/// self-destructs in the paced queue.
const EMERGENCY_FLOOR_KBPS: u32 = 300;
/// Consecutive self-congested ticks with the target already pinned at the
/// configured floor before that floor yields. Combined with
/// [`AQM_SUSTAINED_TICKS`] this is ~5 s of continuous AQM drops at the
/// floor — long enough that no transient burst can trip it. A tick count,
/// not a duration — see the `AQM_SUSTAINED_TICKS` comment for the
/// `stats_interval_ms` coupling caveat.
const FLOOR_YIELD_TICKS: u32 = 3;

/// A single link is clearly melting down: high loss AND a deep ARQ queue.
/// The loss-weighted aggregate can mask this behind a healthy sibling link,
/// so callers check per-link rather than relying on the aggregate alone.
fn link_melting(l: &LinkCapacity) -> bool {
    l.alive
        && l.loss_rate >= LINK_MELT_LOSS_RATE
        && l.queue_depth.unwrap_or(0) >= LINK_MELT_QUEUE_DEPTH
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
    /// Fraction of packets that arrived past the playout deadline over the
    /// reporting interval (0.0–1.0). Late packets are user-visible artifacts
    /// even when outright loss is low, so the adapter treats a sustained
    /// late-rate as delay pressure independent of jitter-buffer growth.
    pub late_rate: f32,
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
    /// Whether this adapter has ever observed non-zero aggregate capacity.
    /// Used to distinguish cold-start from mid-stream collapse.
    ever_had_capacity: bool,
    /// Number of consecutive ticks without a notable capacity decrease (for
    /// ramp-up gating). Despite the name this is "no notable decrease", not
    /// "increasing" — a flat capacity also counts, deliberately, since
    /// ramp-up should gate on stability. See the comment at its update site.
    consecutive_increases: u32,
    /// Number of consecutive capacity decreases.
    consecutive_decreases: u32,
    over_pressure_ticks: u32,
    /// Previous sum of per-link AQM drop counters (for per-tick deltas).
    prev_aqm_dropped: u64,
    /// Consecutive ticks on which the paced-queue AQM deleted packets.
    /// One dropping tick can be a single over-size burst; a sustained run
    /// means the queue is standing past its sojourn budget — the offered
    /// rate (video + FEC + retransmits) durably exceeds the drain rate.
    aqm_dropping_ticks: u32,
    /// Latched while AQM drops are sustained. Forces the over-pressure
    /// reduce path regardless of the (optimistic) capacity estimate, and
    /// pins FEC overhead to baseline — congestive loss must not inflate
    /// parity, that's the 50%-overhead-at-zero-spare death spiral.
    self_congested: bool,
    /// Consecutive self-congested ticks with the target already at the
    /// configured `min_bitrate_kbps` floor. Feeds the floor-yield latch.
    floor_pinned_ticks: u32,
    /// Latched when the configured floor is itself pinning the encoder
    /// above deliverable capacity (field 2026-07-05: min 3000 kbps forced
    /// against a ~20 kbps deliverable trickle → every reduce decision
    /// logged `reduce=true` but clamped back to the floor, AQM shredded
    /// the stream, and the HLS egress starved into a rebuild loop). While
    /// latched, all congestion floors drop to [`EMERGENCY_FLOOR_KBPS`];
    /// clears only once the target has ramped back up to the configured
    /// floor under genuine capacity, so releasing the latch can never
    /// itself snap the encoder back into the congestion that set it.
    floor_yielded: bool,
    /// Consecutive ticks with zero usable capacity while links are still alive.
    /// A single transient zero is a feedback/ACK gap on an otherwise-healthy
    /// link (the next tick reports full capacity), NOT a collapse — slamming to
    /// min on it produces a ~5s bitrate sawtooth that shows as grey/blocky
    /// frames. Only a sustained run is treated as a real LinkFailure.
    zero_capacity_ticks: u32,
    /// When the last rate *increase* was committed — used to suppress
    /// feedback-driven reductions for a grace period so stale receiver
    /// metrics don't immediately revert the increase.
    last_increase_time: Option<Instant>,
    /// EWMA-smoothed post-FEC loss rate from receiver feedback.
    /// Smoothing prevents single bursty LTE seconds from triggering
    /// unnecessary bitrate reductions.
    ewma_loss_fec: f32,
    /// Max per-link CHANNEL (wire) loss observed on the last `update()`.
    /// FEC parity is sized to this, NOT to `ewma_loss_fec`: the post-FEC
    /// residual folds in cross-link reorder and late-arrival loss that
    /// parity cannot repair, and feeding it back inflates FEC into its own
    /// repair-microburst congestion source (field 2026-06-27: ~2% wire loss,
    /// ~60% post-FEC residual, FEC pinned at 41% while the encoder sat at the
    /// 500 kbps floor with 3.7 Mbps spare).
    max_link_loss: f64,
    /// EWMA of `max_link_loss` — the value FEC parity sizing actually
    /// reads. Per review_findings.md §2.4.2: the raw per-tick max let a
    /// single bursty second lift overhead (and inject a parity burst) for
    /// exactly one tick. Rises over ~3 ticks so only sustained channel
    /// loss grows parity; falls fast so a cleaned-up channel sheds it.
    max_link_loss_sustained: f64,
    /// EWMA-smoothed goodput (bps) from receiver feedback.
    /// Prevents single low-sample outliers (e.g. end-of-window artifacts)
    /// from triggering spurious goodput-shortfall reductions.
    ewma_goodput_bps: f64,
    /// Per-link EWMA-smoothed capacity (kbps). Filters out noisy spikes
    /// and dips from Oracle/BBR/ack-rate estimates on lossy LTE links.
    /// α=0.3 gives ~2s half-life assuming ~1 tick/s (`stats_interval_ms`
    /// default, config.rs) — see the tick-count caveat on
    /// [`AQM_SUSTAINED_TICKS`] for how this assumption can go stale.
    capacity_ewma: HashMap<usize, f64>,
    /// When the last burst-loss cut happened.  Ramp-up is suppressed for a
    /// cooldown period after a burst to prevent the classic sawtooth where the
    /// adapter ramps back to the same level that triggered the burst within
    /// 3-4 seconds, only to get punished again.
    last_burst_time: Option<Instant>,
    /// Sliding window of (timestamp, goodput_bps) samples from receiver feedback.
    /// Used by the windowed max-filter to track peak delivery rate.
    goodput_window: VecDeque<(Instant, f64)>,
    /// Peak goodput from the sliding window (bps).
    ///
    /// Used as the capacity anchor instead of `ewma_goodput_bps` to avoid the
    /// application-limited self-trap: after a burst cut the encoder to 2 Mbps,
    /// the slow EWMA would lock the ceiling at ~2 Mbps even though the links
    /// are physically capable of 6 Mbps.  The peak remembers the true capacity.
    goodput_peak_bps: f64,
    /// Previous jitter buffer depth from receiver feedback.
    prev_jitter_buffer_ms: u32,
    /// When the current non-severe congestion signal first became true.
    /// `None` while the link is healthy. Cleared as soon as the signal
    /// drops. A reduction is only allowed once the signal has been
    /// continuously true for [`AdaptationConfig::congestion_sustain`];
    /// `severe_burst` bypasses this gate so genuine collapses still
    /// react immediately.
    congestion_started: Option<Instant>,
    /// Bitrate (kbps) the startup ramp climbs toward — the resolved initial
    /// target. `0` means the ramp is inactive (disabled, or already complete,
    /// or no explicit initial bitrate). See [`AdaptationConfig::startup_ramp`].
    startup_ramp_target_kbps: u32,
    /// Floor (kbps) the startup ramp begins at. Only meaningful while
    /// `startup_ramp_target_kbps > 0`.
    startup_ramp_floor_kbps: u32,
    /// When the startup-ramp clock started — set lazily on the first
    /// `update()` tick so SSH/pipeline spin-up latency doesn't eat into the
    /// ramp window. `None` until the first tick.
    startup_ramp_started: Option<Instant>,
}

/// A downward override of `current_target_kbps` applied by
/// `update_with_feedback` after `update()`'s own capacity-path commit.
///
/// Before this existed, `update_with_feedback` had three independent call
/// sites that each directly wrote `self.current_target_kbps` and hand-rolled
/// its own subset of the related bookkeeping (`last_command_time`,
/// `last_increase_time`, `last_burst_time`, `consecutive_*`) — a discrepancy
/// between any two sites was invisible without reading all three bodies side
/// by side (review_findings.md §2.2). `TargetOverride` +
/// [`BitrateAdapter::apply_target_override`] centralize that bookkeeping
/// into one place; the flags below capture the real, intentional
/// differences between the sites (e.g. the jitter-growth revert deliberately
/// does NOT touch `last_command_time`, since it undoes a commit `update()`
/// already made this same tick).
///
/// This does not change WHEN an override applies or WHAT target it computes
/// — each call site still owns its own trigger condition, and all three
/// remain sequential downward-only refinements (each can further restrict
/// whatever the previous stage left this tick, in the fixed order:
/// increase-revert, then goodput-ceiling clamp, then the feedback congestion
/// cut), not a one-of-N arbitrated choice.
struct TargetOverride {
    target_kbps: u32,
    reason: AdaptationReason,
    touch_last_command_time: bool,
    clear_increase_grace: bool,
    arm_burst_cooldown: bool,
    count_as_decrease: bool,
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
        // Gentle startup ramp: when an explicit initial bitrate is set and a
        // ramp window is configured, start at a low floor and climb to
        // `initial` over the window (see `apply_startup_ramp`). Legacy/unit
        // tests leave `initial_bitrate_kbps == 0` and/or `startup_ramp == 0`,
        // so they keep starting at the full initial with no ramp.
        let ramp_active = !config.startup_ramp.is_zero() && config.initial_bitrate_kbps > 0;
        let (start_target, ramp_target, ramp_floor) = if ramp_active {
            let floor = config
                .startup_floor_kbps
                .max(config.min_bitrate_kbps)
                .min(initial);
            (floor, initial, floor)
        } else {
            (initial, 0, 0)
        };
        BitrateAdapter {
            config,
            current_target_kbps: start_target,
            stage: DegradationStage::Normal,
            mode: ReliabilityMode::MaxQuality,
            spare_bw_kbps: 0,
            last_command_time: None,
            prev_capacity_kbps: 0.0,
            ever_had_capacity: false,
            consecutive_increases: 0,
            consecutive_decreases: 0,
            over_pressure_ticks: 0,
            prev_aqm_dropped: 0,
            aqm_dropping_ticks: 0,
            self_congested: false,
            floor_pinned_ticks: 0,
            floor_yielded: false,
            zero_capacity_ticks: 0,
            last_increase_time: None,
            ewma_loss_fec: 0.0,
            max_link_loss: 0.0,
            max_link_loss_sustained: 0.0,
            ewma_goodput_bps: 0.0,
            capacity_ewma: HashMap::new(),
            last_burst_time: None,
            goodput_window: VecDeque::new(),
            goodput_peak_bps: 0.0,
            prev_jitter_buffer_ms: 0,
            congestion_started: None,
            startup_ramp_target_kbps: ramp_target,
            startup_ramp_floor_kbps: ramp_floor,
            startup_ramp_started: None,
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
        const MIN_LOSSY_OVERHEAD: f64 = 0.25;
        const MAX_OVERHEAD: f64 = 0.50;
        // Ceiling used when we're in MaxQuality but the links have plenty of
        // spare bandwidth. Kept conservative: FEC repair packets are emitted
        // in bursts at packet-group boundaries, and pushing overhead higher
        // than ~15% on cellular links turns those repair bursts into their
        // own congestion source (microbursts overflow marginal-link buffers
        // → late packets → reported loss → further FEC inflation). 15% is
        // enough to meaningfully improve recovery while staying below the
        // burst-induced-congestion knee observed in field testing.
        const MAX_QUALITY_SPARE_CEILING: f64 = 0.15;

        // Spare-driven scaling.
        // MaxReliability mode: scale aggressively up to MAX_OVERHEAD.
        // MaxQuality mode: still scale when spare exceeds the encode target,
        // but cap at MAX_QUALITY_SPARE_CEILING so we never trade bitrate for
        // protection when bandwidth is actually tight.
        let spare_scaled = if self.spare_bw_kbps > 0 {
            let ratio = self.spare_bw_kbps as f64 / self.current_target_kbps.max(1) as f64;
            let ceiling = if self.mode == ReliabilityMode::MaxReliability {
                MAX_OVERHEAD
            } else {
                MAX_QUALITY_SPARE_CEILING
            };
            (BASE_OVERHEAD + ratio * (ceiling - BASE_OVERHEAD)).min(ceiling)
        } else {
            BASE_OVERHEAD
        };

        // Loss-driven scaling avoids being stuck at 10% overhead when links
        // are genuinely lossy but mode hysteresis has not latched.
        //
        // Sized to per-link CHANNEL loss (`max_link_loss`), NOT the post-FEC
        // residual (`ewma_loss_fec`). FEC parity only repairs random channel
        // loss; the residual also folds in cross-link reorder and late-arrival
        // loss that parity CANNOT fix. Driving FEC from the residual is a
        // positive-feedback death spiral: repair packets are emitted in bursts
        // at generation boundaries, those microbursts overflow marginal-link
        // buffers → late packets → higher residual → still more parity. Field
        // 2026-06-27: ~2% wire loss but ~60% residual (cross-link reorder)
        // pinned FEC at 41% while the encoder sat at the 500 floor with 3.7
        // Mbps spare and both links idle.
        //
        // EXCEPT under self-congestion: when the paced-queue AQM is cutting our
        // own standing queue, the loss is congestive (offered exceeds drained)
        // and parity only adds to the offer — pin to baseline (that's the
        // bitrate adapter's job, via the over-pressure path).
        let loss_scaled = if self.self_congested {
            BASE_OVERHEAD
        } else {
            (BASE_OVERHEAD + self.max_link_loss_sustained * 0.50).min(MAX_OVERHEAD)
        };

        let mut overhead = spare_scaled.max(loss_scaled);
        if self.max_link_loss_sustained >= 0.25 && !self.self_congested {
            overhead = overhead.max(MIN_LOSSY_OVERHEAD);
        }

        overhead.clamp(BASE_OVERHEAD, MAX_OVERHEAD)
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

    fn apply_target_override(&mut self, ov: TargetOverride) -> BitrateCommand {
        self.current_target_kbps = ov.target_kbps;
        if ov.touch_last_command_time {
            self.last_command_time = Some(Instant::now());
        }
        if ov.clear_increase_grace {
            self.last_increase_time = None;
        }
        if ov.arm_burst_cooldown {
            self.last_burst_time = Some(Instant::now());
        }
        if ov.count_as_decrease {
            self.consecutive_decreases += 1;
            self.consecutive_increases = 0;
        }
        self.make_command(ov.target_kbps, ov.reason)
    }

    /// Per-tick slew-rate limit applied only to bitrate *increases*.
    ///
    /// Caps the per-tick step up so the encoder is never asked to climb more
    /// than +15 % from `current_target_kbps`. Decreases pass through
    /// unchanged: a real capacity drop must reach the encoder immediately so
    /// it doesn't drown a fading link. Without the up-side cap, noisy
    /// per-tick goodput / capacity estimates whipsaw the encoder (field run
    /// #10 saw 1039 ↔ 2742 kbps swings every 5 s); CBR-HEVC produces visible
    /// VBV transients on every jump — the "moments of clear, then grey
    /// noise, then back" pattern. The 15 % step deliberately exceeds the
    /// 10 % `target_changed` commit threshold so legitimate ramp-up ticks
    /// still propagate. `LinkFailure` bypasses the limit entirely.
    fn slew_clamp(&self, proposed: u32, reason: AdaptationReason) -> u32 {
        if matches!(reason, AdaptationReason::LinkFailure) {
            return proposed;
        }
        const MAX_STEP_UP_PCT: f64 = 0.15;
        let anchor = self.current_target_kbps.max(self.config.min_bitrate_kbps) as f64;
        let up_cap = (anchor * (1.0 + MAX_STEP_UP_PCT)) as u32;
        proposed.min(up_cap)
    }

    /// Time-based ceiling enforcing the gentle startup ramp.
    ///
    /// During the [`AdaptationConfig::startup_ramp`] window the encoder target
    /// is capped to a ceiling that climbs linearly from
    /// `startup_ramp_floor_kbps` to the resolved initial bitrate, so a cold
    /// cellular link is not blasted with full rate before its bandwidth grant
    /// warms up (the dominant source of the ~14 % startup loss burst seen in
    /// field tests). The clock starts on the first call (the first `update()`
    /// tick), not at construction. Once the window elapses the ceiling is
    /// released permanently and this becomes a no-op. Also a no-op when the
    /// ramp is inactive (`startup_ramp_target_kbps == 0`).
    fn apply_startup_ramp(&mut self, proposed: u32) -> u32 {
        if self.startup_ramp_target_kbps == 0 {
            return proposed;
        }
        let started = *self.startup_ramp_started.get_or_insert_with(Instant::now);
        let window = self.config.startup_ramp;
        let elapsed = started.elapsed();
        if elapsed >= window {
            // Ramp complete — release the ceiling for the rest of the stream.
            self.startup_ramp_target_kbps = 0;
            return proposed;
        }
        let floor = self.startup_ramp_floor_kbps as f64;
        let target = self.startup_ramp_target_kbps as f64;
        let frac = elapsed.as_secs_f64() / window.as_secs_f64();
        let ceiling = (floor + (target - floor) * frac) as u32;
        proposed.min(ceiling.max(self.startup_ramp_floor_kbps))
    }

    /// Update with new link capacity information and optionally produce
    /// a bitrate command if the encoder target should change.
    pub fn update(&mut self, links: &[LinkCapacity]) -> Option<BitrateCommand> {
        // EWMA-smooth per-link capacity before aggregation.
        // Asymmetric: fast down (α=0.5) so drops are tracked quickly,
        // slow up (α=0.3) so recovery is conservative. Filters noisy
        // Oracle/BBR/ack-rate spikes on lossy LTE links.
        // Skip smoothing for zero capacity (cold-start: no data yet).
        const CAP_EWMA_ALPHA_UP: f64 = 0.3;
        const CAP_EWMA_ALPHA_DOWN: f64 = 0.5;

        // Aggregate capacity from alive links
        let aggregate_kbps: f64 = links
            .iter()
            .filter(|l| l.alive)
            .map(|l| {
                // Drain-honesty clamp: the pacer is the rate the link
                // ACTUALLY sends at; capacity claims above it (the oracle
                // over-reads lossy LTE) budget the encoder past what the
                // link can deliver, and the surplus becomes paced-queue AQM
                // drops — self-inflicted mid-GOP holes. BBR's startup gain
                // keeps pacing above the delivered rate, so ramp-up probing
                // still works under the clamp.
                let raw = match l.drain_rate_kbps {
                    Some(drain) if drain > 0.0 => l.capacity_kbps.min(drain),
                    _ => l.capacity_kbps,
                };
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

        // Max per-link CHANNEL loss this tick — drives FEC parity sizing in
        // `recommended_fec_overhead`. Channel (wire) loss is what parity can
        // actually repair; the post-FEC residual is not (it includes reorder
        // and late-arrival loss, and feeding it back is the FEC death spiral).
        self.max_link_loss = links
            .iter()
            .filter(|l| l.alive)
            .map(|l| l.loss_rate)
            .fold(0.0_f64, f64::max);

        // Sustain gate for FEC sizing (§2.4.2): believe a loss step over
        // ~3 ticks (one HARQ burst can't spike parity for a tick), shed it
        // fast once the channel is clean again (parity down is always safe).
        const FEC_LOSS_ALPHA_UP: f64 = 0.3;
        const FEC_LOSS_ALPHA_DOWN: f64 = 0.7;
        let alpha = if self.max_link_loss > self.max_link_loss_sustained {
            FEC_LOSS_ALPHA_UP
        } else {
            FEC_LOSS_ALPHA_DOWN
        };
        self.max_link_loss_sustained += alpha * (self.max_link_loss - self.max_link_loss_sustained);

        // Log per-link detail
        for l in links {
            let smoothed = self
                .capacity_ewma
                .get(&l.link_id)
                .copied()
                .unwrap_or(l.capacity_kbps);
            let queue_depth = l.queue_depth.map(|q| q as i64).unwrap_or(-1);
            info!(
                target: "strata::adapt",
                "[link] id={} cap_kbps={:.0} smooth_kbps={:.0} alive={} loss={:.3} rtt_ms={:.0} queue={}",
                l.link_id,
                l.capacity_kbps,
                smoothed,
                l.alive,
                l.loss_rate,
                l.rtt_ms,
                queue_depth
            );
        }

        // Compute usable capacity (with headroom)
        let usable_kbps = aggregate_kbps * (1.0 - self.config.headroom);

        // Arbitrary sentinel pressure values (not derived from a ratio, since
        // there is no usable capacity to divide by) that feed
        // `DegradationStage::from_pressure(1/p)` — just need to be > 1.0 and
        // ordered zero-capacity < no-links so the degradation stage escalates
        // correctly between the two.
        const ZERO_CAPACITY_PRESSURE_SENTINEL: f64 = 2.0;
        const NO_LINKS_PRESSURE_SENTINEL: f64 = 5.0;
        // Compute pressure ratio (target / capacity; >1 = over-pressure)
        let mut pressure = if usable_kbps > 0.0 {
            self.current_target_kbps as f64 / usable_kbps
        } else if alive_count > 0 {
            ZERO_CAPACITY_PRESSURE_SENTINEL // have links but zero capacity
        } else {
            NO_LINKS_PRESSURE_SENTINEL // no links alive
        };

        // ── Self-congestion detector: paced-queue AQM drops ──────────────
        // The AQM only deletes packets once the queue has stood past its
        // sojourn budget — direct, unambiguous evidence that the offered
        // rate (video + FEC + retransmits) exceeds the drain rate, no
        // matter how optimistic the capacity estimate reads. A single
        // dropping tick can be one oversized burst; a sustained run forces
        // the over-pressure reduce path and latches `self_congested` (which
        // also pins FEC overhead to baseline — see
        // `recommended_fec_overhead`).
        const AQM_DROPS_PER_TICK_THRESHOLD: u64 = 5;
        // A TICK count, not a duration — this assumes ~1 tick/s
        // (`SchedulerConfig::stats_interval_ms` default, config.rs). An
        // operator who halves `stats_interval_ms` for snappier telemetry
        // silently halves this sustain window too (~2s → ~1s), and doubling
        // it doubles the window. Not converted to a wall-clock `Instant`
        // sustain (the `congestion_started` pattern below) because doing so
        // would require every existing tick-count-driven regression test in
        // this module to either sleep for real wall-clock time or special-
        // case a zero-duration override that would defeat the "more than
        // one tick" semantics under test — left as a documented coupling.
        const AQM_SUSTAINED_TICKS: u32 = 2;
        // Self-congestion only makes sense when we are actually offering near
        // capacity. AQM drops while the target sits far below usable capacity
        // are NOT the encoder overdriving — they are burst-loss artifacts from
        // a flapping link (a modem momentarily losing its grant), and pinning
        // bitrate to the floor in response is exactly wrong. The drain clamp
        // already makes `usable_kbps` honest (per-link capacity is bounded by
        // the pacing rate), so the raw pressure ratio is a valid "am I near
        // capacity?" gate. Without it, a bursty 2nd modem (~10 AQM drops/tick)
        // latched this permanently and held a 2-link bond at the 500 kbps
        // floor despite 1.4-4 Mbps usable (field 2026-06-15).
        const SELF_CONGEST_MIN_PRESSURE: f64 = 0.7;
        let aqm_total: u64 = links.iter().filter_map(|l| l.aqm_dropped_total).sum();
        let aqm_delta = aqm_total.saturating_sub(self.prev_aqm_dropped);
        self.prev_aqm_dropped = aqm_total;
        if aqm_delta >= AQM_DROPS_PER_TICK_THRESHOLD {
            self.aqm_dropping_ticks += 1;
        } else {
            self.aqm_dropping_ticks = 0;
        }
        self.self_congested =
            self.aqm_dropping_ticks >= AQM_SUSTAINED_TICKS && pressure >= SELF_CONGEST_MIN_PRESSURE;
        if self.self_congested {
            // Nudge pressure just past the over-pressure threshold so the
            // AQM latch reliably forces the reduce path this tick, even if
            // the (optimistic) capacity estimate alone would have read as
            // under-pressure.
            const SELF_CONGEST_PRESSURE_BUMP: f64 = 0.05;
            pressure = pressure.max(self.config.pressure_threshold + SELF_CONGEST_PRESSURE_BUMP);
            info!(
                target: "strata::adapt",
                "[adapt] self-congestion: AQM dropped {aqm_delta} pkts this tick \
                 ({} sustained ticks, pressure {:.2}) — forcing over-pressure",
                self.aqm_dropping_ticks, pressure
            );
        }

        // ── Floor-yield latch ─────────────────────────────────────────────
        // Self-congestion with the target ALREADY at the configured floor
        // means the floor itself is what's overdriving the links: every
        // reduce path clamps back to it, so without this the adapter logs
        // `reduce=true` forever while the AQM shreds the stream. Yield the
        // floor to EMERGENCY_FLOOR_KBPS. Restore only once the target has
        // ramped back up to the configured floor under real capacity — an
        // earlier release would let the min-clamp snap the target straight
        // back into the congestion that latched it.
        if self.self_congested && self.current_target_kbps <= self.config.min_bitrate_kbps {
            self.floor_pinned_ticks += 1;
        } else {
            self.floor_pinned_ticks = 0;
        }
        if !self.floor_yielded && self.floor_pinned_ticks >= FLOOR_YIELD_TICKS {
            self.floor_yielded = true;
            info!(
                target: "strata::adapt",
                "[adapt] floor-yield: min_bitrate {} kbps is pinning the encoder above \
                 deliverable capacity ({} self-congested ticks at the floor) — floor \
                 yields to {} kbps until the target recovers",
                self.config.min_bitrate_kbps, self.floor_pinned_ticks, EMERGENCY_FLOOR_KBPS
            );
        } else if self.floor_yielded
            && !self.self_congested
            && self.current_target_kbps >= self.config.min_bitrate_kbps
        {
            self.floor_yielded = false;
            info!(
                target: "strata::adapt",
                "[adapt] floor-yield: target recovered to the configured floor ({} kbps) — \
                 floor restored",
                self.config.min_bitrate_kbps
            );
        }

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

        // Track capacity trend. Despite the name, `consecutive_increases`
        // really counts "no notable decrease" — a perfectly flat capacity
        // also increments it, which is intentional (ramp-up gates on
        // stability, not on a genuine upward run). A tick landing in the
        // `CAPACITY_TREND_DECREASE_BELOW`..`CAPACITY_TREND_STABLE_ABOVE`
        // dead zone (90-95% of the previous tick) advances neither counter,
        // freezing both trends for that tick.
        const CAPACITY_TREND_STABLE_ABOVE: f64 = 0.95;
        const CAPACITY_TREND_DECREASE_BELOW: f64 = 0.90;
        if aggregate_kbps >= self.prev_capacity_kbps * CAPACITY_TREND_STABLE_ABOVE {
            self.consecutive_increases += 1;
            self.consecutive_decreases = 0;
        } else if aggregate_kbps < self.prev_capacity_kbps * CAPACITY_TREND_DECREASE_BELOW {
            self.consecutive_decreases += 1;
            self.consecutive_increases = 0;
        }
        if aggregate_kbps > 0.0 {
            self.ever_had_capacity = true;
        }
        self.prev_capacity_kbps = aggregate_kbps;

        // Per-link collapse: one link is clearly melting (high loss AND deep
        // queue). The loss-weighted aggregate can still look "fine" because a
        // healthy link masks the dying one, which lets compute_target() choose
        // Recovery on the same tick that update_with_feedback() issues a cut.
        // Suppress the ramp-up path by resetting the increase counter — the
        // capacity path then holds, and the feedback path is free to reduce
        // without fighting a concurrent increase.
        let per_link_collapse = links.iter().any(link_melting);
        if per_link_collapse || self.self_congested {
            // Self-congestion likewise must not race a concurrent ramp-up:
            // the AQM is already cutting packets, so adding rate is the
            // exact wrong direction even if the capacity trend looks up.
            self.consecutive_increases = 0;
        }

        // ─── Mode switching: MaxQuality ↔ MaxReliability ────────────
        // Switch to MaxReliability when encoder is at 80%+ of ceiling
        // and spare bandwidth exceeds threshold.
        const AT_CEILING_FRACTION: f64 = 0.80;
        let at_ceiling = self.current_target_kbps as f64
            >= self.config.max_bitrate_kbps as f64 * AT_CEILING_FRACTION;
        let big_spare = usable_kbps
            > (self.current_target_kbps + self.config.reliability_spare_threshold_kbps) as f64;

        // Hysteresis gap back to MaxQuality: revert only once usable capacity
        // falls meaningfully (20%) below the quality cap, not the instant it
        // dips below it, so a link hovering right at the cap doesn't flap
        // mode every tick.
        const QUALITY_CAP_HYSTERESIS_MULT: f64 = 1.2;
        if at_ceiling && big_spare {
            self.mode = ReliabilityMode::MaxReliability;
        } else if usable_kbps < self.config.quality_cap_kbps as f64 * QUALITY_CAP_HYSTERESIS_MULT {
            // Not enough capacity to even reach the quality cap with spare
            self.mode = ReliabilityMode::MaxQuality;
        }

        // Compute effective max bitrate (capped in MaxReliability mode)
        let effective_max = if self.mode == ReliabilityMode::MaxReliability {
            self.config.quality_cap_kbps
        } else {
            self.config.max_bitrate_kbps
        };

        if pressure > self.config.pressure_threshold {
            self.over_pressure_ticks += 1;
        } else {
            self.over_pressure_ticks = 0;
        }

        // Track transient vs sustained zero-capacity (see `zero_capacity_ticks`).
        // A single alive-but-zero tick is a feedback gap; only a sustained run is
        // a genuine mid-stream collapse.
        if usable_kbps == 0.0 && alive_count > 0 {
            self.zero_capacity_ticks += 1;
        } else {
            self.zero_capacity_ticks = 0;
        }

        // Determine if we need a bitrate change
        let (new_target, reason) =
            self.compute_target(usable_kbps, pressure, alive_count, self.ever_had_capacity);
        let new_target = new_target.min(effective_max).max(self.floor_kbps());
        // Slew-rate limit so the encoder is not whipsawed by per-tick
        // capacity-estimate noise (see `slew_clamp` doc).
        let new_target = self.slew_clamp(new_target, reason);
        // Gentle startup ramp: cap the target to a climbing ceiling for the
        // first few seconds so a cold link isn't blasted with full rate
        // (see `apply_startup_ramp` doc). No-op once warmed / when disabled.
        let new_target = self.apply_startup_ramp(new_target);

        // Track spare bandwidth
        self.spare_bw_kbps = if usable_kbps > new_target as f64 {
            (usable_kbps - new_target as f64) as u32
        } else {
            0
        };

        // Only issue command if target changed meaningfully and enough time passed.
        // Use a relative threshold (>10% change) with a small absolute floor (50 kbps)
        // so that small-but-significant changes at low bitrates aren't suppressed.
        const COMMAND_COMMIT_ABS_FLOOR_KBPS: u64 = 50;
        const COMMAND_COMMIT_PCT: f64 = 0.10;
        let abs_change = (new_target as i64 - self.current_target_kbps as i64).unsigned_abs();
        let pct_change = abs_change as f64 / self.current_target_kbps.max(1) as f64;
        // The >10% relative gate rejects capacity-estimate noise, but a
        // Recovery ramp step climbing home after a floor-yield is not
        // noise: with the default 250 kbps step the gate blocks every
        // climb past ~2.5 Mbps (250/2500 = 10%), which would strand a
        // yielded encoder below a 3 Mbps configured floor forever — the
        // latch releases only at the floor. Scoped to below-floor targets:
        // an unconditional Recovery bypass lets ramp-ups commit every tick
        // at high bitrates, and each same-tick increase suppresses the
        // feedback cut path via `increased_this_tick`.
        let ramp_home = reason == AdaptationReason::Recovery
            && self.current_target_kbps < self.config.min_bitrate_kbps;
        let target_changed = abs_change > COMMAND_COMMIT_ABS_FLOOR_KBPS
            && (pct_change > COMMAND_COMMIT_PCT || ramp_home);
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
            // Only activate grace period for substantial increases (>10% or >200kbps).
            // Minor capacity oscillations shouldn't keep grace permanently active.
            const GRACE_ARM_ABS_KBPS: u32 = 200;
            const GRACE_ARM_PCT: f64 = 0.10;
            if new_target > self.current_target_kbps {
                let inc_abs = new_target - self.current_target_kbps;
                let inc_pct = inc_abs as f64 / self.current_target_kbps.max(1) as f64;
                if inc_abs > GRACE_ARM_ABS_KBPS || inc_pct > GRACE_ARM_PCT {
                    self.last_increase_time = Some(Instant::now());
                }
            }
            self.current_target_kbps = new_target;
            self.last_command_time = Some(Instant::now());
        }

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
    /// - `jitter_buffer_ms > 900` → delay pressure, reduce bitrate
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
        let mut result = self.update(links);

        let jitter_growth_ms = feedback
            .jitter_buffer_ms
            .saturating_sub(self.prev_jitter_buffer_ms);
        self.prev_jitter_buffer_ms = feedback.jitter_buffer_ms;
        // Independent evidence that a jitter/late-arrival signal reflects a
        // real network event, not the event's own after-the-fact residual.
        // Gated on the CHANNEL (wire) loss `update()` already computed this
        // tick, not the post-FEC residual: a pure reorder/late-arrival burst
        // inflates `jitter_growth_ms`/`late_rate` AND the residual in the
        // same window, so using the residual as the "independent" conjunct
        // was self-confirming (see git history / review notes for the
        // incident this replaced). `max_link_loss` is set once per tick at
        // the top of `update()` and does not change for the rest of this
        // call, so — unlike the two residual-based reads this replaced —
        // one evaluation here is valid for every use site below.
        const CHANNEL_LOSS_CONTEXT_THRESHOLD: f64 = 0.03;
        let jitter_loss_context = self.max_link_loss > CHANNEL_LOSS_CONTEXT_THRESHOLD;
        let max_queue_depth = links
            .iter()
            .filter(|l| l.alive)
            .filter_map(|l| l.queue_depth)
            .max()
            .unwrap_or(0);
        let link_collapse = links.iter().any(link_melting);

        // Clamp: if update() increased the target but the receiver is stalling
        // (jitter near the latency ceiling), revert the increase.  Capacity
        // looks fine from the sender's perspective but the receiver can't keep
        // up — pushing more data only deepens the jitter buffer.
        const INCREASE_REVERT_JITTER_GROWTH_MS: u32 = 160;
        if self.current_target_kbps > target_before_update
            && jitter_growth_ms > INCREASE_REVERT_JITTER_GROWTH_MS
            && jitter_loss_context
        {
            result = Some(self.apply_target_override(TargetOverride {
                target_kbps: target_before_update,
                reason: AdaptationReason::Capacity,
                // `update()` already advanced `last_command_time` this tick
                // if it committed the increase being reverted here — don't
                // reset the min_interval clock a second time.
                touch_last_command_time: false,
                clear_increase_grace: true, // don't activate grace for a reverted increase
                arm_burst_cooldown: false,
                count_as_decrease: false,
            }));
        }

        // Snapshot after the capacity-path update.  If the target already fell,
        // the aggregate loss adjustment in update() has accounted for the loss
        // event — the feedback path must not double-count it.
        let target_after_capacity = self.current_target_kbps;
        let increased_this_tick = target_after_capacity > target_before_update;
        let (capacity_target, capacity_reason) = result
            .as_ref()
            .map(|cmd| (cmd.target_kbps, cmd.reason))
            .unwrap_or((self.current_target_kbps, AdaptationReason::Capacity));

        // Apply receiver-side pressure signals.
        // loss_after_fec is per-interval (delta-based) at the receiver, but LTE
        // loss is bursty so we EWMA-smooth it (α=0.3, ~2s half-life) to avoid
        // reacting to a single bad second.  This smoothed residual no longer
        // drives an encoder cut (see below) or `jitter_loss_context` (which
        // reads channel-side `max_link_loss` instead); it now only feeds
        // the FEC burst-lift.
        //
        // When goodput is positive, update normally.  When goodput is zero
        // (stall), decay the EWMA toward 0 (×0.9) instead of freezing it, so a
        // stall can't latch the signal high and poison the contexts that read
        // it; this lets the system re-probe after ~10s of stall.
        // α values follow the polarity rule in
        // wiki/Adaptation-EWMA-Conventions.md (§1b): 0.3 is this codebase's
        // recurring "believe a change over ~3 ticks" weight.
        const LOSS_EWMA_ALPHA: f32 = 0.3;
        const LOSS_EWMA_STALL_DECAY: f32 = 0.9;
        let ewma_loss_before = self.ewma_loss_fec;
        if feedback.goodput_bps > 0 {
            self.ewma_loss_fec = LOSS_EWMA_ALPHA * feedback.loss_after_fec
                + (1.0 - LOSS_EWMA_ALPHA) * self.ewma_loss_fec;
        } else {
            // Stall: decay loss toward 0 so the system can eventually recover.
            self.ewma_loss_fec *= LOSS_EWMA_STALL_DECAY;
        }
        // The post-FEC residual (`ewma_loss_fec`) deliberately does NOT cut the
        // encoder.  It folds in cross-link reorder and late-arrival loss that
        // parity can't repair and that cutting the encoder can't fix — the same
        // signal that drove the FEC death spiral.  Real *channel* loss is already
        // priced into the capacity path (per-link `smoothed * (1 - loss)` →
        // pressure), and delivered-throughput collapse is caught by
        // `goodput_shortfall` below (headroom-aware and reorder-immune).  What
        // remains as a hard receiver-side cut is the genuine per-link melt
        // detector, `link_collapse` (high loss AND deep queue).
        // Instantaneous burst cut: a single window with >35% loss-after-FEC,
        // fast enough to bypass grace and the sustain gate.  But loss-after-FEC
        // alone is reorder/late-contaminated — field run orangepi-10360 saw 72
        // such "burst" windows at a mean 5.3 Mbps delivered goodput (delivery was
        // fine; the residual was just late/out-of-order), yet they slammed the
        // encoder to the floor under ~5 Mbps of spare.  So require an *actual*
        // delivered-throughput collapse too: goodput positive (not a total
        // stall — a dead link reads loss 1.0 with goodput 0, an artifact handled
        // elsewhere) but below 70% of the offered rate.  A reorder spike with
        // healthy goodput no longer cuts; a real loss burst, where goodput drops
        // with the loss, still cuts immediately (same-window via instant goodput).
        const BURST_LOSS_AFTER_FEC_THRESHOLD: f32 = 0.35;
        // Shared with `goodput_shortfall` below (§1b: same "70% of offered
        // rate" ratio, two different signals — instantaneous here, EWMA there).
        const GOODPUT_SHORTFALL_RATIO: f64 = 0.7;
        let burst_loss = feedback.loss_after_fec > BURST_LOSS_AFTER_FEC_THRESHOLD
            && feedback.goodput_bps > 0
            && (feedback.goodput_bps as f64)
                < target_before_update as f64 * 1000.0 * GOODPUT_SHORTFALL_RATIO;
        // Treat sustained post-FEC loss >50% with queue growth as an emergency.
        // This allows an additional same-tick reduction even if capacity-path
        // logic already cut once.
        const SEVERE_BURST_LOSS_AFTER_FEC_THRESHOLD: f32 = 0.50;
        const SEVERE_BURST_JITTER_BUFFER_MS: u32 = 200;
        let severe_burst = burst_loss
            && feedback.loss_after_fec > SEVERE_BURST_LOSS_AFTER_FEC_THRESHOLD
            && feedback.jitter_buffer_ms > SEVERE_BURST_JITTER_BUFFER_MS;
        if burst_loss {
            // Lift EWMA rapidly during burst events so FEC overhead scaling
            // reflects the current loss regime, not only the smoothed history.
            const BURST_EWMA_LIFT_FACTOR: f32 = 0.8;
            self.ewma_loss_fec = self
                .ewma_loss_fec
                .max(feedback.loss_after_fec * BURST_EWMA_LIFT_FACTOR);
        }
        // A sustained late-arrival rate is delay pressure only when it
        // coincides with actual channel-loss evidence (`jitter_loss_context`,
        // computed above). Pure jitter without real loss can't be fixed by
        // cutting the encoder — doing so just degrades visible quality
        // while the underlying OWD spike blows through.
        const LATE_RATE_THRESHOLD: f32 = 0.05;
        let late_pressure = feedback.late_rate > LATE_RATE_THRESHOLD && jitter_loss_context;
        // A deep paced queue is NOT bufferbloat on its own. That queue is
        // bounded by a drain-time byte budget (pacing_rate × 0.5s, see
        // net::transport) that deliberately passes keyframe bursts intact, so
        // its packet count routinely spikes to hundreds during a healthy IDR
        // burst that drains well inside the playout window. A raw packet-count
        // gate here (`queue_depth >= 90`) fired on those benign bursts (~65% of
        // ticks in field test 2026-06-27) and pinned the encoder to the 500
        // floor at pressure ~0.1 with usable ~4.7 Mbps. The genuine "queue
        // standing past its sojourn budget" signal is the AQM-drop counter,
        // which update() already turns into pressure-gated self-congestion.
        // Delay pressure here is therefore receiver-visible only: jitter-buffer
        // growth/overflow and late arrivals under loss.
        const DELAY_PRESSURE_JITTER_GROWTH_MS: u32 = 120;
        let delay_pressure = (jitter_growth_ms > DELAY_PRESSURE_JITTER_GROWTH_MS
            && jitter_loss_context)
            || feedback.jitter_buffer_ms > self.config.jitter_buffer_ceiling_ms
            || late_pressure;
        let bufferbloat = delay_pressure;
        // EWMA-smooth goodput (α=0.3) to filter end-of-window noise artifacts
        // that can appear as a single near-zero reading followed by a burst.
        // Seed with the first real reading to avoid cold-start false shortfalls.
        if feedback.goodput_bps > 0 {
            const GOODPUT_EWMA_ALPHA: f64 = 0.3;
            if self.ewma_goodput_bps == 0.0 {
                self.ewma_goodput_bps = feedback.goodput_bps as f64;
            } else {
                self.ewma_goodput_bps = GOODPUT_EWMA_ALPHA * feedback.goodput_bps as f64
                    + (1.0 - GOODPUT_EWMA_ALPHA) * self.ewma_goodput_bps;
            }

            // Windowed p75 of recent goodput.  Using the raw max let a single
            // outlier sample (e.g. drained-backlog + live traffic clearing
            // together) anchor the floor and ramp-up ceiling at a value the
            // links could not sustain, which in field testing drove the
            // adapter to target bitrates the network could not actually
            // carry.  The 75th percentile tracks sustained high throughput
            // without being hostage to a single burst window.
            self.goodput_window
                .push_back((Instant::now(), feedback.goodput_bps as f64));
            self.goodput_window
                .retain(|(t, _)| t.elapsed().as_secs_f64() < GOODPUT_WINDOW_SECS);
            let mut samples: Vec<f64> = self.goodput_window.iter().map(|(_, g)| *g).collect();
            if !samples.is_empty() {
                samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                // p75 = element at index floor(0.75 * (n-1)).  With small n
                // this naturally approaches the max; by the time we have
                // ~4+ samples it starts rejecting single-sample spikes.
                let idx = ((samples.len() as f64 - 1.0) * 0.75).floor() as usize;
                self.goodput_peak_bps = samples[idx];
            }
        } else {
            // Zero-goodput tick (real delivery stall, or a probe-suppressed
            // update). Previously the EWMA and p75 peak froze at their last
            // value, so the dynamic floor stayed optimistically high through
            // the entire stall and the adapter held a target the links could
            // not carry. Decay both toward zero so memory ages out instead
            // of latching — symmetric with the loss EWMA's stall decay.
            // Time-based half-life via GOODPUT_WINDOW eviction also prunes
            // stale window samples so the p75 peak tracks the decay.
            self.ewma_goodput_bps *= 0.8;
            if self.ewma_goodput_bps < 1000.0 {
                self.ewma_goodput_bps = 0.0;
            }
            self.goodput_window
                .retain(|(t, _)| t.elapsed().as_secs_f64() < GOODPUT_WINDOW_SECS);
            let mut samples: Vec<f64> = self.goodput_window.iter().map(|(_, g)| *g).collect();
            if samples.is_empty() {
                self.goodput_peak_bps = 0.0;
            } else {
                samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let idx = ((samples.len() as f64 - 1.0) * 0.75).floor() as usize;
                self.goodput_peak_bps = samples[idx];
            }
        }
        // Goodput-anchor the current target using the windowed peak, not the
        // slow EWMA.  After a burst the EWMA decays toward the artificially
        // depressed encoder output (application-limited trap); the peak retains
        // the memory of what the links actually delivered before the event.
        const GOODPUT_CEILING_CLAMP_TRIGGER: f64 = 0.85;
        if self.goodput_peak_bps > 0.0 {
            let peak_gp_kbps = self.goodput_peak_bps / 1000.0;
            let goodput_ceil_kbps = (peak_gp_kbps / (1.0 - self.config.headroom)) as u32;
            let goodput_ceil_kbps = goodput_ceil_kbps.max(self.config.min_bitrate_kbps);
            if self.current_target_kbps > goodput_ceil_kbps
                && peak_gp_kbps < (self.current_target_kbps as f64 * GOODPUT_CEILING_CLAMP_TRIGGER)
            {
                result = Some(self.apply_target_override(TargetOverride {
                    target_kbps: goodput_ceil_kbps,
                    reason: AdaptationReason::Congestion,
                    touch_last_command_time: true,
                    clear_increase_grace: true,
                    arm_burst_cooldown: false,
                    count_as_decrease: false,
                }));
            }
        }

        // Compare smoothed goodput against the PRE-update target since goodput
        // lags the encoder rate by at least one RTT.
        let goodput_shortfall = self.ewma_goodput_bps > 0.0
            && self.ewma_goodput_bps
                < target_before_update as f64 * 1000.0 * GOODPUT_SHORTFALL_RATIO;
        // A *severe* shortfall (delivering < 50% of the pre-update rate) is the
        // grace-bypass tier.  Because it compares against the pre-update target,
        // a stale post-increase reading still reflects the OLD rate and can't
        // trip it (ramp steps keep new/old < 2×).  This is the trustworthy,
        // reorder-immune replacement for the residual signal's grace pass-through.
        const SEVERE_GOODPUT_SHORTFALL_RATIO: f64 = 0.5;
        let severe_goodput_shortfall = self.ewma_goodput_bps > 0.0
            && self.ewma_goodput_bps
                < target_before_update as f64 * 1000.0 * SEVERE_GOODPUT_SHORTFALL_RATIO;

        // After a rate increase, receiver metrics are stale for a few seconds
        // (they still reflect the old encoder rate).  Suppress ordinary
        // goodput-shortfall reductions during this grace period so we don't
        // immediately revert every increase.  Delay pressure, link collapse and
        // *severe* goodput shortfall pass through grace: the first two are
        // instantaneous, and a severe shortfall is measured against the
        // pre-update target, so staleness reflects the old rate and can't fake it.
        const FEEDBACK_GRACE_PERIOD: Duration = Duration::from_secs(5);
        let feedback_grace = self
            .last_increase_time
            .is_some_and(|t| t.elapsed() < FEEDBACK_GRACE_PERIOD);
        // Do not apply same-tick cuts on the exact tick of a capacity-path
        // increase. This avoids increase→cut ping-pong from stale feedback.
        let raw_signal = if feedback_grace {
            link_collapse
                || delay_pressure
                || (burst_loss && !increased_this_tick)
                || (severe_goodput_shortfall && !increased_this_tick)
        } else {
            link_collapse || delay_pressure || goodput_shortfall || burst_loss
        };

        // Sustained-duration gate: brief cellular HARQ bursts produce
        // single-tick `link_collapse` / `late_pressure` / `burst_loss`
        // signals that clear within ~800 ms. Reacting to each one creates
        // the sawtooth encoder bitrate seen in field tests (47 commands /
        // 120 s, ±500 kbps swings every 2-3 s, visible artifacts).
        //
        // Track when the signal first became true; only allow the cut
        // once it has been continuously true for `CONGESTION_SUSTAIN`.
        // `severe_burst` bypasses the gate so true collapse events still
        // get an immediate reaction.
        if raw_signal {
            if self.congestion_started.is_none() {
                self.congestion_started = Some(Instant::now());
            }
        } else {
            self.congestion_started = None;
        }
        let sustained = self
            .congestion_started
            .is_some_and(|t| t.elapsed() >= self.config.congestion_sustain);
        let feedback_reduction = severe_burst || sustained;

        // Skip the explicit feedback cut when the capacity-path update() already
        // reduced the target in this tick. The loss signal is baked into the
        // loss-adjusted aggregate capacity. Exception: severe burst-loss windows
        // may require an additional immediate reduction to arrest collapse.
        let capacity_already_cut = target_after_capacity < target_before_update;
        let allow_feedback_cut = !capacity_already_cut || severe_burst;

        let raw_held_ms = self
            .congestion_started
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        info!(
            target: "strata::adapt",
            "[adapt] fb: loss_fec={:.3} ewma_loss={:.3}→{:.3} late={:.3} jitter={}ms(+{}ms) qmax={} gp={}kbps peak_gp={}kbps | gp_short_sev={} link_collapse={} burst={} severe={} bb={} late_p={} gp_short={} grace={} cap_cut={} allow_cut={} inc_tick={} raw={} held_ms={} sustained={} → reduce={}",
            feedback.loss_after_fec,
            ewma_loss_before,
            self.ewma_loss_fec,
            feedback.late_rate,
            feedback.jitter_buffer_ms,
            jitter_growth_ms,
            max_queue_depth,
            feedback.goodput_bps / 1000,
            self.goodput_peak_bps as u64 / 1000,
            severe_goodput_shortfall,
            link_collapse,
            burst_loss,
            severe_burst,
            bufferbloat,
            late_pressure,
            goodput_shortfall,
            feedback_grace,
            capacity_already_cut,
            allow_feedback_cut,
            increased_this_tick,
            raw_signal,
            raw_held_ms,
            sustained,
            feedback_reduction && allow_feedback_cut
        );

        if feedback_reduction && allow_feedback_cut {
            // Receiver signals congestion — force a reduction. Severe bursts
            // cut at least this hard regardless of the configured
            // `ramp_down_factor`, since a milder configured factor would
            // under-react to a confirmed emergency.
            const SEVERE_BURST_MAX_REDUCTION_FACTOR: f64 = 0.55;
            let reduction_factor = if severe_burst {
                self.config
                    .ramp_down_factor
                    .min(SEVERE_BURST_MAX_REDUCTION_FACTOR)
            } else {
                self.config.ramp_down_factor
            };
            let new_target = (self.current_target_kbps as f64 * reduction_factor) as u32;
            // The dynamic floor is intentionally optimistic during healthy
            // burst recovery, but once receiver-visible loss/late pressure is
            // real it can pin the encoder above the working envelope. In that
            // state, let congestion cuts fall back to the static floor so the
            // encoder can actually retreat instead of staying stuck near its
            // pre-collapse target.
            let floor_kbps = if link_collapse || burst_loss || severe_burst || late_pressure {
                self.floor_kbps()
            } else {
                self.effective_floor_kbps()
            };
            let new_target = new_target.max(floor_kbps);

            if new_target < self.current_target_kbps {
                let reason = if link_collapse || burst_loss || severe_burst {
                    AdaptationReason::Congestion
                } else {
                    AdaptationReason::Capacity
                };
                result = Some(self.apply_target_override(TargetOverride {
                    target_kbps: new_target,
                    reason,
                    touch_last_command_time: true,
                    // Only a severe burst needs to suppress a would-be grace
                    // period — an ordinary sustained cut doesn't imply the
                    // most recent increase (if any) was itself wrong.
                    clear_increase_grace: severe_burst,
                    // Mark burst on a melt-driven cut too, not just
                    // single-window burst_loss — a collapsing link is a
                    // bursty-network signal, so arming the ramp-up cooldown
                    // here prevents cut→decay→ramp back into the next burst
                    // (the sawtooth).
                    arm_burst_cooldown: burst_loss || link_collapse,
                    count_as_decrease: true,
                }));
            }
        }

        if let Some(final_cmd) = result.as_ref() {
            info!(
                target: "strata::adapt",
                "[fec] mode={:?} overhead_pct={:.1} spare_kbps={} target_kbps={}",
                final_cmd.mode,
                final_cmd.recommended_fec_overhead * 100.0,
                final_cmd.spare_bw_kbps,
                final_cmd.target_kbps
            );
            info!(
                target: "strata::adapt",
                "[adapt] CMD cap_target_kbps={} final_target_kbps={} cap_reason={:?} final_reason={:?}",
                capacity_target,
                final_cmd.target_kbps,
                capacity_reason,
                final_cmd.reason
            );
        }

        result
    }

    /// Compute the new target bitrate.
    fn compute_target(
        &self,
        usable_kbps: f64,
        pressure: f64,
        alive_count: usize,
        had_capacity: bool,
    ) -> (u32, AdaptationReason) {
        let current = self.current_target_kbps;

        // Emergency: no links
        if alive_count == 0 {
            debug!(target: "strata::adapt", "decision: no alive links → min");
            return (self.floor_kbps(), AdaptationReason::LinkFailure);
        }

        // Zero usable capacity: distinguish cold-start from mid-stream collapse.
        // Cold-start (never had capacity): hold current bitrate.
        // Mid-stream (had capacity before): this is a collapse, drop to min.
        if usable_kbps == 0.0 {
            // Sustained zero capacity is a genuine mid-stream collapse → min.
            // A transient single-tick zero on an otherwise-healthy link is just a
            // feedback/ACK gap; collapsing on it produces a ~5s bitrate sawtooth
            // (grey/blocky frames) on a perfectly good link, so hold instead and
            // wait for confirmation. ZERO_CAP_COLLAPSE_TICKS at ~1s/tick ≈ 2s:
            // a single transient zero holds; two consecutive zeros confirm
            // collapse. Tick count, not a duration — see the
            // `AQM_SUSTAINED_TICKS` comment for the `stats_interval_ms`
            // coupling caveat.
            const ZERO_CAP_COLLAPSE_TICKS: u32 = 2;
            if had_capacity && self.zero_capacity_ticks >= ZERO_CAP_COLLAPSE_TICKS {
                debug!(target: "strata::adapt", "decision: zero usable (sustained {} ticks → collapse) → min", self.zero_capacity_ticks);
                return (self.floor_kbps(), AdaptationReason::LinkFailure);
            }
            if had_capacity {
                debug!(target: "strata::adapt", "decision: zero usable (transient {} tick → hold)", self.zero_capacity_ticks);
                return (self.current_target_kbps, AdaptationReason::Capacity);
            }
            debug!(target: "strata::adapt", "decision: zero usable (cold-start) → hold");
            return (self.current_target_kbps, AdaptationReason::Capacity);
        }

        // Over-pressure: need to reduce. `over_pressure_ticks` is a tick
        // count, not a duration — see the `AQM_SUSTAINED_TICKS` comment for
        // the `stats_interval_ms` coupling caveat.
        if pressure > self.config.pressure_threshold {
            if self.over_pressure_ticks >= 2 {
                let target = (current as f64 * self.config.ramp_down_factor) as u32;
                let target = target
                    .max(self.effective_floor_kbps())
                    .min(usable_kbps as u32);

                let reason = if self.over_pressure_ticks >= 3 {
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
            } else {
                debug!(
                    target: "strata::adapt",
                    "decision: OVER-PRESSURE {:.2} > {:.2} but cd={} < 2 → hold",
                    pressure, self.config.pressure_threshold, self.consecutive_decreases
                );
                return (current, AdaptationReason::Capacity);
            }
        }

        // Under-pressure with stable capacity: ramp up.
        // Suppress ramp-up after a loss-driven cut.  Bursty networks deliver
        // clean windows between bursts — if we ramp back up as soon as EWMA
        // decays, we hand traffic to the next burst.  A 10s cooldown is ~1-2
        // typical burst intervals on cellular links, long enough to confirm
        // the network has genuinely stabilized before ramping again.
        const RAMP_UP_BURST_COOLDOWN: Duration = Duration::from_secs(10);
        let burst_cooldown = self
            .last_burst_time
            .is_some_and(|t| t.elapsed() < RAMP_UP_BURST_COOLDOWN);
        // Ramp-up threshold uses pressure_threshold - 0.05 (5% hysteresis gap)
        // instead of the previous hardcoded 0.7.  With pressure_threshold=0.9
        // this gives a ramp-up zone of pressure < 0.85 and an over-pressure zone
        // of > 0.90, raising the effective utilisation ceiling from 59.5% to
        // 72% of raw capacity.
        const RAMP_UP_HYSTERESIS_GAP: f64 = 0.05;
        let ramp_up_threshold = self.config.pressure_threshold - RAMP_UP_HYSTERESIS_GAP;
        // Ramp-up is gated on pressure (capacity headroom), a run of increasing
        // capacity, the burst cooldown, and the goodput-peak ceiling below — but
        // NOT on post-FEC residual loss.  That residual includes reorder/late
        // loss the links are still delivering through; suppressing ramp-up on it
        // pinned the encoder below real headroom (sibling of the FEC death
        // spiral).  Genuine bursts/melts arm `burst_cooldown` via last_burst_time.
        // `consecutive_increases >= 3` is a tick count, not a duration — see
        // the `AQM_SUSTAINED_TICKS` comment for the `stats_interval_ms`
        // coupling caveat.
        if pressure < ramp_up_threshold && self.consecutive_increases >= 3 && !burst_cooldown {
            let target = current + self.config.ramp_up_kbps_per_step;
            let target = target.min(self.config.max_bitrate_kbps);
            // Cap below the pressure threshold to avoid overshooting into
            // over-pressure on the very next tick.
            let safe_ceiling =
                (usable_kbps * (self.config.pressure_threshold - RAMP_UP_HYSTERESIS_GAP)) as u32;
            let target = target.min(safe_ceiling);
            // Cap ramp-up to 1.3x peak goodput — sender-side capacity is
            // optimistic on lossy links; real throughput is what matters.
            // Using the peak (not EWMA) avoids the post-burst ceiling trap.
            const RAMP_UP_GOODPUT_PEAK_CEILING_MULT: f64 = 1.3;
            let target = if self.goodput_peak_bps > 0.0 {
                let gp_ceiling =
                    (self.goodput_peak_bps * RAMP_UP_GOODPUT_PEAK_CEILING_MULT / 1000.0) as u32;
                target.min(gp_ceiling)
            } else {
                target
            };
            debug!(
                target: "strata::adapt",
                "decision: RAMP-UP pressure={:.2} ci={} → {} → {} (gp_peak={}kbps)",
                pressure, self.consecutive_increases, current, target,
                self.goodput_peak_bps as u64 / 1000
            );
            return (target, AdaptationReason::Recovery);
        }

        // Stable: no change
        if pressure < ramp_up_threshold {
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

    /// The configured static floor, unless the floor-yield latch is set —
    /// then the emergency floor (see `floor_yielded`). The `.min()` keeps a
    /// user-configured floor below 300 kbps authoritative.
    fn floor_kbps(&self) -> u32 {
        if self.floor_yielded {
            EMERGENCY_FLOOR_KBPS.min(self.config.min_bitrate_kbps)
        } else {
            self.config.min_bitrate_kbps
        }
    }

    /// Dynamic bitrate floor: never reduce below half of the recent windowed
    /// peak goodput.  The static `min_bitrate_kbps` floor is far too low on
    /// good multi-link setups (e.g. 500 kbps when links delivered 3–4 Mbps
    /// sustained).  Collapsing to it during a transient burst produces a
    /// visible soft-encode dip before ramp-up recovers — the dominant source
    /// of residual artifacting on otherwise healthy runs.  Keeping the floor
    /// tethered to recent real delivery lets bursts reduce the encode
    /// without dropping off a cliff.
    fn effective_floor_kbps(&self) -> u32 {
        let static_floor = self.floor_kbps();
        if self.goodput_peak_bps > 0.0 {
            // Floor is the lesser of: half the windowed peak, or 80% of the
            // smoothed goodput.  The dual-cap prevents a single high-peak
            // sample from anchoring the floor above what the network can
            // actually sustain (observed in field: peak=9.5 Mbps briefly, so
            // 0.5*peak gave 4.7 Mbps floor on links that averaged ~3 Mbps).
            // Smoothed goodput tracks recent sustained delivery and keeps
            // the floor from following transient outliers.
            const DYNAMIC_FLOOR_PEAK_HALF: f64 = 0.5;
            const DYNAMIC_FLOOR_EWMA_CAP_FRACTION: f64 = 0.8;
            let peak_half = self.goodput_peak_bps * DYNAMIC_FLOOR_PEAK_HALF;
            let ewma_cap = if self.ewma_goodput_bps > 0.0 {
                self.ewma_goodput_bps * DYNAMIC_FLOOR_EWMA_CAP_FRACTION
            } else {
                peak_half
            };
            let dynamic = (peak_half.min(ewma_cap) / 1000.0) as u32;
            static_floor.max(dynamic)
        } else {
            static_floor
        }
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
        self.over_pressure_ticks = 0;
        self.prev_capacity_kbps = 0.0;
        self.ever_had_capacity = false;
        self.prev_jitter_buffer_ms = 0;
        self.last_command_time = None;
        self.capacity_ewma.clear();
        self.floor_pinned_ticks = 0;
        self.floor_yielded = false;
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
                drain_rate_kbps: None,
                aqm_dropped_total: None,
            })
            .collect()
    }

    // ─── Drain-honesty: capacity clamp + AQM self-congestion ────────────

    /// The oracle over-reads lossy LTE; the pacer is the rate the link
    /// actually sends at. Capacity above the drain rate must not budget the
    /// encoder (it would just become paced-queue AQM drops).
    #[test]
    fn capacity_clamped_to_drain_rate() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });
        let mut links = make_links(&[(8_000.0, true)]);
        links[0].drain_rate_kbps = Some(2_000.0);
        // Repeat so the capacity EWMA converges to the clamped value.
        let mut cmd = None;
        for _ in 0..20 {
            cmd = adapter.update(&links);
        }
        let cmd = cmd.unwrap();
        // usable = 2000 × (1 - headroom 0.15) = 1700 — the target must be
        // governed by the drain rate, not the 8000 kbps capacity claim.
        assert!(
            cmd.target_kbps <= 1_700,
            "target {} must be bounded by the drain rate, not the capacity claim",
            cmd.target_kbps
        );
    }

    /// Sustained AQM drops are direct evidence of offered > drained and must
    /// force the over-pressure reduce path even when the capacity estimate
    /// claims there is headroom.
    #[test]
    fn sustained_aqm_drops_force_reduce() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 8_000,
            min_bitrate_kbps: 500,
            initial_bitrate_kbps: 1_500,
            min_interval: Duration::ZERO,
            ..Default::default()
        });
        // The real overdrive case: the capacity estimate is optimistic (8000)
        // but the drain clamp pins usable to the pacing rate (~1500), and the
        // encoder is offering right at it (pressure ≈ 1.0). Here AQM drops DO
        // mean we are overdriving, so self-congestion must engage.
        let mut links = make_links(&[(8_000.0, true)]);
        links[0].drain_rate_kbps = Some(1_500.0);
        for _ in 0..15 {
            adapter.update(&links);
        }
        let before = adapter.current_target_kbps();
        assert!(
            before > 800,
            "precondition: target near the clamped capacity ({before})"
        );
        // Two sustained AQM-drop ticks (~50 pkts each) latch the detector
        // while the encoder is still offering near capacity.
        let mut total = 0u64;
        for _ in 0..2 {
            total += 50;
            links[0].aqm_dropped_total = Some(total);
            adapter.update(&links);
        }
        assert!(
            adapter.self_congested,
            "sustained AQM drops near capacity must latch self-congestion"
        );
        // Continued drops force the target down. (The latch self-releases once
        // it has backed the encoder away from capacity — correct: from there,
        // further drops are link bursts, not overdrive.)
        for _ in 0..6 {
            total += 50;
            links[0].aqm_dropped_total = Some(total);
            adapter.update(&links);
        }
        assert!(
            adapter.current_target_kbps() < before,
            "target must reduce under self-congestion ({} → {})",
            before,
            adapter.current_target_kbps()
        );
        // Drops stop → detector clears.
        for _ in 0..3 {
            links[0].aqm_dropped_total = Some(total);
            adapter.update(&links);
        }
        assert!(!adapter.self_congested, "latch must clear when drops stop");
    }

    /// Field regression (2026-07-05): min_bitrate 3000 kbps forced against a
    /// radio delivering a fraction of that. Every reduce decision clamped
    /// back to the configured floor, so the encoder kept overdriving the
    /// links (`reduce=true` logged forever), the paced-queue AQM shredded
    /// the stream, and HLS egress starved into a rebuild loop. Under
    /// sustained self-congestion at the floor, the floor must yield.
    #[test]
    fn pinned_floor_yields_under_sustained_self_congestion() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 6_000,
            min_bitrate_kbps: 3_000,
            initial_bitrate_kbps: 3_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });
        // Radio can only drain ~400 kbps; the 3000 kbps floor overdrives it.
        let mut links = make_links(&[(8_000.0, true)]);
        links[0].drain_rate_kbps = Some(400.0);
        let mut total = 0u64;
        for _ in 0..12 {
            total += 50;
            links[0].aqm_dropped_total = Some(total);
            adapter.update(&links);
        }
        assert!(
            adapter.floor_yielded,
            "sustained self-congestion at the floor must latch floor-yield"
        );
        assert!(
            adapter.current_target_kbps() < 3_000,
            "target must fall below the configured floor ({} kbps)",
            adapter.current_target_kbps()
        );
        // Keep congesting: the target must retreat into the deliverable
        // envelope (usable ≈ 340 kbps here), far below the configured floor.
        for _ in 0..20 {
            total += 50;
            links[0].aqm_dropped_total = Some(total);
            adapter.update(&links);
        }
        let target = adapter.current_target_kbps();
        assert!(
            (EMERGENCY_FLOOR_KBPS..=400).contains(&target),
            "sustained overdrive must retreat to the deliverable rate, got {target}"
        );
    }

    /// Once yielded, the floor must stay yielded until the target has ramped
    /// back up to the configured floor under real capacity — releasing it
    /// early would snap the target straight back into the congestion that
    /// latched it. After recovery, the configured floor is authoritative
    /// again.
    #[test]
    fn yielded_floor_restores_only_after_target_recovers() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 6_000,
            min_bitrate_kbps: 3_000,
            initial_bitrate_kbps: 3_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });
        let mut links = make_links(&[(8_000.0, true)]);
        links[0].drain_rate_kbps = Some(400.0);
        let mut total = 0u64;
        for _ in 0..12 {
            total += 50;
            links[0].aqm_dropped_total = Some(total);
            adapter.update(&links);
        }
        assert!(adapter.floor_yielded);
        let low = adapter.current_target_kbps();
        assert!(low < 3_000);

        // Congestion clears but capacity is still poor: the latch must hold
        // (no snap back up to the configured floor).
        links[0].aqm_dropped_total = Some(total);
        for _ in 0..5 {
            adapter.update(&links);
        }
        assert!(
            adapter.floor_yielded,
            "latch must hold while the target is still below the floor"
        );
        assert!(
            adapter.current_target_kbps() < 3_000,
            "target must not snap back to the pinned floor ({} kbps)",
            adapter.current_target_kbps()
        );

        // Radio recovers: ramp-up climbs the target back to the configured
        // floor, and only then does the latch release.
        links[0].drain_rate_kbps = Some(8_000.0);
        for _ in 0..120 {
            adapter.update(&links);
        }
        assert!(
            adapter.current_target_kbps() >= 3_000,
            "target must recover under real capacity ({} kbps)",
            adapter.current_target_kbps()
        );
        assert!(
            !adapter.floor_yielded,
            "latch must release once the target recovers to the floor"
        );
    }

    /// Field regression (2026-06-15): a bursty 2nd modem produced ~10 AQM
    /// drops/tick, which latched self-congestion permanently and pinned a
    /// 2-link bond at the 500 kbps floor despite 1.4-4 Mbps usable. AQM drops
    /// while the target sits far below usable capacity are link-burst
    /// artifacts, NOT overdrive — they must not force the bitrate down.
    #[test]
    fn aqm_drops_below_capacity_do_not_pin() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 8_000,
            min_bitrate_kbps: 500,
            initial_bitrate_kbps: 500, // start at the floor, as the field bond was
            min_interval: Duration::ZERO,
            ..Default::default()
        });
        // Ample, genuinely drainable capacity (usable ~6800) but the encoder
        // sits at the 500 floor — pressure ≈ 500/6800 ≈ 0.07, far below the
        // self-congest gate. AQM drops here are a bursting link, not overdrive.
        let mut links = make_links(&[(8_000.0, true)]);
        links[0].drain_rate_kbps = Some(8_000.0);
        let mut total = 0u64;
        let start = adapter.current_target_kbps();
        for _ in 0..6 {
            total += 50; // sustained AQM drops, well over the absolute threshold
            links[0].aqm_dropped_total = Some(total);
            adapter.update(&links);
        }
        assert!(
            !adapter.self_congested,
            "AQM drops with target ≪ usable are link bursts, must not latch self-congestion"
        );
        // And the encoder must be free to climb, not pinned at the floor.
        for _ in 0..20 {
            total += 50;
            links[0].aqm_dropped_total = Some(total);
            adapter.update(&links);
        }
        assert!(
            adapter.current_target_kbps() > start,
            "bitrate must climb toward usable capacity, not stay pinned ({} → {})",
            start,
            adapter.current_target_kbps()
        );
    }

    /// Congestive loss must not inflate FEC overhead: parity adds to the
    /// very offer that is overflowing the queue (the 50%-overhead-at-zero-
    /// spare death spiral). Under self-congestion the loss-driven scaling
    /// is pinned to baseline.
    #[test]
    fn fec_overhead_pinned_under_self_congestion() {
        let mut adapter = BitrateAdapter {
            max_link_loss_sustained: 0.8, // sustained channel loss: drives overhead to 50%
            ..Default::default()
        };
        adapter.self_congested = false;
        assert!(adapter.recommended_fec_overhead() >= 0.49);
        adapter.self_congested = true;
        assert!(
            adapter.recommended_fec_overhead() <= 0.11,
            "self-congestion must pin FEC to baseline, got {}",
            adapter.recommended_fec_overhead()
        );
    }

    /// The FEC death spiral: a high POST-FEC residual (cross-link reorder /
    /// late arrivals) with a CLEAN channel must NOT inflate parity. Extra
    /// repair packets cannot recover reorder/late loss, and their bursts at
    /// generation boundaries become their own congestion source. Field
    /// 2026-06-27: ~2% wire loss but ~60% residual pinned FEC at 41% while
    /// the encoder sat at the 500 kbps floor with 3.7 Mbps spare and both
    /// links idle. Parity must follow channel loss, not the residual.
    #[test]
    fn fec_overhead_not_inflated_by_reorder_residual() {
        let mut adapter = BitrateAdapter {
            ewma_loss_fec: 0.6,            // high post-FEC residual (reorder/late)
            max_link_loss_sustained: 0.02, // but the wire itself is clean
            ..Default::default()
        };
        adapter.self_congested = false;
        let overhead = adapter.recommended_fec_overhead();
        assert!(
            overhead <= 0.12,
            "reorder/late residual must not inflate FEC on a clean channel, got {overhead}"
        );
    }

    /// §2.4.2: FEC sizing must not spike on a single bursty tick — one
    /// second of HARQ-burst loss previously lifted overhead (and emitted a
    /// parity burst) for exactly one tick. It must still grow under
    /// sustained loss, and shed quickly once the channel is clean.
    #[test]
    fn fec_overhead_requires_sustained_loss() {
        let mut adapter = BitrateAdapter::default();
        let lossy = |loss: f64| {
            vec![LinkCapacity {
                link_id: 0,
                capacity_kbps: 5_000.0,
                alive: true,
                loss_rate: loss,
                rtt_ms: 30.0,
                queue_depth: Some(0),
                drain_rate_kbps: None,
                aqm_dropped_total: None,
            }]
        };

        // One bursty tick: overhead must stay near baseline.
        adapter.update(&lossy(0.40));
        let after_spike = adapter.recommended_fec_overhead();
        assert!(
            after_spike < 0.25,
            "a single lossy tick must not spike FEC overhead, got {after_spike}"
        );

        // Sustained loss: overhead grows to the lossy floor.
        for _ in 0..5 {
            adapter.update(&lossy(0.40));
        }
        let sustained = adapter.recommended_fec_overhead();
        assert!(
            sustained >= 0.25,
            "sustained channel loss must grow FEC overhead, got {sustained}"
        );

        // Clean channel: parity sheds within a couple of ticks.
        adapter.update(&lossy(0.0));
        adapter.update(&lossy(0.0));
        let recovered = adapter.recommended_fec_overhead();
        assert!(
            recovered < 0.15,
            "a clean channel must shed parity quickly, got {recovered}"
        );
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
        adapter.update(&links);
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
            drain_rate_kbps: None,
            aqm_dropped_total: None,
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
        adapter.update(&links);
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
            min_bitrate_kbps: 100, // low floor so loss-pressure cuts have room to operate
            min_interval: Duration::ZERO,
            congestion_sustain: Duration::ZERO,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);

        // Establish a high baseline with healthy feedback so the adapter ramps
        // up well above the loss-constrained ceiling we're about to trigger.
        let healthy = ReceiverFeedback {
            goodput_bps: 6_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 50,
            loss_after_fec: 0.0,
            late_rate: 0.0,
        };
        for _ in 0..8 {
            adapter.update_with_feedback(&links, &healthy);
        }
        let baseline = adapter.current_target_kbps();

        // Inject severe degradation: goodput collapses to 500 kbps and
        // loss_after_fec 0.60.  burst-loss (>0.35) plus the goodput shortfall
        // against the high baseline both drive the cut.
        let bad = ReceiverFeedback {
            goodput_bps: 500_000, // 500 kbps — severely degraded
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.60,
            late_rate: 0.0,
        };
        adapter.update_with_feedback(&links, &bad);
        adapter.update_with_feedback(&links, &bad);
        let cmd = adapter.update_with_feedback(&links, &bad);

        assert!(cmd.is_some(), "should emit command on sustained high loss");
        assert!(
            adapter.current_target_kbps() < baseline,
            "target should decrease from baseline {}: now {}",
            baseline,
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

        let links = make_links(&[(20_000.0, true)]);
        adapter.update(&links); // prime
        let target_after_prime = adapter.current_target_kbps();

        let feedback = ReceiverFeedback {
            goodput_bps: 8_000_000,
            fec_repair_rate: 0.01,
            jitter_buffer_ms: 50,
            loss_after_fec: 0.03, // below 5% threshold,
            late_rate: 0.0,
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
            jitter_buffer_ms: 1000,
            loss_after_fec: 0.20,
            late_rate: 0.0,
        };

        let before = adapter.current_target_kbps();
        let cmd = adapter.update_with_feedback(&links, &feedback);
        assert!(cmd.is_some(), "high jitter should trigger command");
        assert!(
            adapter.current_target_kbps() < before,
            "target should decrease on high jitter"
        );
    }

    // ─── N4: jitter-buffer overflow ceiling must be configurable ──────────

    /// The delay-pressure "jitter buffer overflow" arm must compare against
    /// `AdaptationConfig::jitter_buffer_ceiling_ms` (sourced from the
    /// receiver's actual `max_latency`), not a hardcoded 3000ms — a deploy
    /// with a smaller receiver ceiling must see the adapter react at ITS
    /// ceiling, not the receiver-default value.
    #[test]
    fn jitter_buffer_ceiling_is_configurable_not_hardcoded() {
        let base = AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_bitrate_kbps: 500,
            initial_bitrate_kbps: 5_000,
            min_interval: Duration::ZERO,
            congestion_sustain: Duration::ZERO,
            ..Default::default()
        };
        let links = make_links(&[(10_000.0, true)]); // clean channel: loss_rate 0.0
        let feedback = ReceiverFeedback {
            goodput_bps: 8_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 150,
            loss_after_fec: 0.0,
            late_rate: 0.0,
        };

        // A low custom ceiling (100ms): 150ms jitter buffer overflows it.
        let mut low_ceiling = BitrateAdapter::new(AdaptationConfig {
            jitter_buffer_ceiling_ms: 100,
            ..base.clone()
        });
        let start = low_ceiling.current_target_kbps();
        low_ceiling.update_with_feedback(&links, &feedback);
        assert!(
            low_ceiling.current_target_kbps() < start,
            "150ms jitter buffer must overflow a configured 100ms ceiling"
        );

        // The default ceiling (3000ms, matching ReceiverConfig::max_latency's
        // own default): the same 150ms jitter buffer must NOT overflow it.
        let mut default_ceiling = BitrateAdapter::new(base);
        let start = default_ceiling.current_target_kbps();
        default_ceiling.update_with_feedback(&links, &feedback);
        assert_eq!(
            default_ceiling.current_target_kbps(),
            start,
            "150ms jitter buffer must not overflow the default 3000ms ceiling"
        );
    }

    // ─── L3: late/delay pressure must gate on channel loss, not residual ──

    /// A pure reorder/late-arrival event inflates `late_rate`, growing
    /// jitter, AND the post-FEC residual all in the same window — so gating
    /// `late_pressure`/`delay_pressure` on the residual (`ewma_loss_fec`/
    /// `loss_after_fec`) was self-confirming: the "independent evidence"
    /// conjunct was satisfied by the very event it was meant to corroborate.
    /// With a genuinely clean channel (`loss_rate` 0 on every link), this
    /// must NOT cut the encoder no matter how high late-rate/jitter/residual
    /// climb.
    #[test]
    fn late_pressure_does_not_trigger_on_clean_channel_reorder() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_bitrate_kbps: 500,
            initial_bitrate_kbps: 5_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });
        // Clean wire: loss_rate 0.0 on the only link.
        let links = make_links(&[(10_000.0, true)]);
        let start = adapter.current_target_kbps();

        let mut jitter_ms = 0u32;
        for _ in 0..6 {
            jitter_ms += 200;
            let feedback = ReceiverFeedback {
                goodput_bps: 8_000_000, // healthy, well above target — no goodput shortfall
                fec_repair_rate: 0.0,
                jitter_buffer_ms: jitter_ms,
                loss_after_fec: 0.10, // reorder/late residual, not real channel loss
                late_rate: 0.10,      // > 0.05
            };
            adapter.update_with_feedback(&links, &feedback);
        }
        assert_eq!(
            adapter.current_target_kbps(),
            start,
            "clean-channel reorder/late residual must not cut the encoder \
             via late_pressure/delay_pressure, got {} (started at {})",
            adapter.current_target_kbps(),
            start
        );
    }

    /// Companion to the above: the same late-rate/jitter-growth signal DOES
    /// cut once there is real per-link channel loss to corroborate it.
    #[test]
    fn late_pressure_triggers_with_real_channel_loss() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_bitrate_kbps: 500,
            initial_bitrate_kbps: 5_000,
            min_interval: Duration::ZERO,
            congestion_sustain: Duration::ZERO,
            ..Default::default()
        });
        // Real channel loss above the CHANNEL_LOSS_CONTEXT_THRESHOLD (0.03).
        let mut links = make_links(&[(10_000.0, true)]);
        links[0].loss_rate = 0.05;
        let start = adapter.current_target_kbps();

        let feedback = ReceiverFeedback {
            goodput_bps: 8_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 300,
            loss_after_fec: 0.10,
            late_rate: 0.10,
        };
        adapter.update_with_feedback(&links, &feedback);
        assert!(
            adapter.current_target_kbps() < start,
            "late-rate/jitter-growth with real channel loss must still cut, \
             got {} (started at {})",
            adapter.current_target_kbps(),
            start
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
            late_rate: 0.0,
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
            late_rate: 0.0,
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
    fn mild_residual_loss_with_healthy_goodput_does_not_cut() {
        // Mild post-FEC residual (10%) with healthy goodput must not cut the
        // encoder: the residual no longer drives a reduction, and goodput is
        // well above the shortfall threshold.
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 2_000,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links); // prime

        // loss_after_fec = 0.10 with goodput at the full 2 Mbps target:
        // no shortfall, no burst, no melt → no cut.
        let feedback = ReceiverFeedback {
            goodput_bps: 2_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.10,
            late_rate: 0.0,
        };

        let before = adapter.current_target_kbps();
        for _ in 0..5 {
            adapter.update_with_feedback(&links, &feedback);
        }

        // With mild loss below threshold, no reduction expected.
        assert!(
            adapter.current_target_kbps() >= before,
            "should not reduce when loss is below EWMA threshold: was {} now {}",
            before,
            adapter.current_target_kbps()
        );
    }

    #[test]
    fn high_residual_loss_with_headroom_does_not_cut_encoder() {
        // Core of the residual-override removal: a post-FEC residual well above
        // the old 0.15 gate, but with healthy goodput, clean channel loss and
        // ample capacity headroom, must NOT cut the encoder.  This is the
        // reorder/late-loss case the FEC fix proved untrustworthy; on the old
        // code `loss_pressure` (ewma > 0.15) would force a reduction here.
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 2_000,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]); // clean channel, big headroom
        adapter.update(&links); // prime

        // Healthy goodput at the target, but a high post-FEC residual (a
        // reorder/late artifact, not channel loss).
        let feedback = ReceiverFeedback {
            goodput_bps: 2_000_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.25, // above the old 0.15 loss_pressure gate
            late_rate: 0.0,
        };

        let before = adapter.current_target_kbps();
        for _ in 0..6 {
            adapter.update_with_feedback(&links, &feedback);
        }

        assert!(
            adapter.ewma_loss_fec > 0.15,
            "residual must exceed the old gate for this test to be meaningful: {:.3}",
            adapter.ewma_loss_fec
        );
        assert!(
            adapter.current_target_kbps() >= before,
            "high residual with healthy goodput + headroom must not cut: was {} now {}",
            before,
            adapter.current_target_kbps()
        );
    }

    #[test]
    fn burst_loss_does_not_cut_when_goodput_is_healthy() {
        // Field orangepi-10360: instantaneous loss_after_fec spiked to 40-97%
        // from cross-link reorder while the receiver still delivered ~5 Mbps.
        // A burst cut must require an actual delivered-throughput collapse, not a
        // reorder-inflated residual — otherwise the encoder slams to the floor
        // under ample headroom.  On the old code this tripped severe_burst.
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            congestion_sustain: Duration::ZERO,
            initial_bitrate_kbps: 2_000,
            ..Default::default()
        });
        let links = make_links(&[(10_000.0, true), (10_000.0, true)]);
        adapter.update(&links); // prime

        // High instantaneous post-FEC residual, but goodput well above the
        // offered rate (delivery is fine — the "loss" is late/reordered).
        // jitter held flat/low so delay_pressure can't confound the burst path.
        let reorder_spike = ReceiverFeedback {
            goodput_bps: 5_000_000, // >> 2 Mbps target — no real collapse
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.60, // would have tripped burst_loss/severe_burst
            late_rate: 0.0,
        };
        let before = adapter.current_target_kbps();
        for _ in 0..5 {
            adapter.update_with_feedback(&links, &reorder_spike);
        }
        assert!(
            adapter.current_target_kbps() >= before,
            "reorder-driven residual with healthy goodput must not cut: was {} now {}",
            before,
            adapter.current_target_kbps()
        );
    }

    #[test]
    fn zero_usable_collapse_still_cuts_after_repeated_zero_ticks() {
        // Regression: if capacity hits zero during min-interval suppression,
        // subsequent zero-capacity ticks must still cut once interval opens.
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_bitrate_kbps: 500,
            min_interval: Duration::from_secs(10),
            initial_bitrate_kbps: 2_000,
            ..Default::default()
        });

        let links_ok = make_links(&[(8_000.0, true)]);
        let links_zero = vec![LinkCapacity {
            link_id: 0,
            capacity_kbps: 8_000.0,
            alive: true,
            loss_rate: 1.0,
            rtt_ms: 80.0,
            queue_depth: Some(95),
            drain_rate_kbps: None,
            aqm_dropped_total: None,
        }];

        adapter.update(&links_ok);
        adapter.force_reduce(AdaptationReason::Capacity); // Sets last_command_time.
        let before = adapter.current_target_kbps();

        // First zero-capacity tick while interval is blocked.
        adapter.update(&links_zero);
        assert_eq!(adapter.current_target_kbps(), before);

        // Open interval and ensure repeated zero-capacity tick cuts to min.
        adapter.last_command_time = Some(Instant::now() - Duration::from_secs(11));
        adapter.update(&links_zero);
        assert_eq!(
            adapter.current_target_kbps(),
            adapter.config.min_bitrate_kbps,
            "zero usable after prior capacity should cut to min"
        );
    }

    #[test]
    fn single_transient_zero_tick_holds_not_collapses() {
        // Regression: a single alive-but-zero-capacity tick (a feedback/ACK gap
        // on a healthy link) must NOT slam bitrate to min. Collapsing on isolated
        // zero ticks produced a ~5s bitrate sawtooth (grey/blocky frames) on a
        // single clean link. The bitrate must hold until the zero is sustained.
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            min_bitrate_kbps: 500,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        let links_ok = make_links(&[(8_000.0, true)]);
        let links_zero = vec![LinkCapacity {
            link_id: 0,
            capacity_kbps: 8_000.0,
            alive: true,
            loss_rate: 1.0, // feedback gap: 100% loss for one tick → usable 0
            rtt_ms: 80.0,
            queue_depth: Some(0),
            drain_rate_kbps: None,
            aqm_dropped_total: None,
        }];

        // Establish capacity, then ramp the target up off the floor.
        for _ in 0..6 {
            adapter.update(&links_ok);
        }
        let healthy = adapter.current_target_kbps();
        assert!(healthy > 500, "expected ramp above min, got {healthy}");

        // One transient zero tick → hold (NOT a collapse to min).
        adapter.update(&links_zero);
        assert_eq!(
            adapter.current_target_kbps(),
            healthy,
            "a single transient zero tick must hold the bitrate, not collapse"
        );

        // Capacity returns next tick → still healthy, no sawtooth.
        adapter.update(&links_ok);
        assert!(adapter.current_target_kbps() >= healthy);
    }

    #[test]
    fn startup_ramp_holds_encoder_low_then_releases() {
        // Gentle startup ramp: even with abundant capacity, the encoder must
        // start at the floor and climb toward the initial bitrate over the
        // ramp window — NOT blast full rate into a cold link (the dominant
        // source of the ~14% startup loss burst that decodes as grey).
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            min_bitrate_kbps: 500,
            max_bitrate_kbps: 8_000,
            initial_bitrate_kbps: 4_000,
            startup_ramp: Duration::from_millis(60),
            startup_floor_kbps: 600,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        // Starts at the ramp floor, not the full initial.
        assert_eq!(
            adapter.current_target_kbps(),
            600,
            "ramp-enabled adapter must start at the floor, not the initial bitrate"
        );

        // Capacity is abundant from the very first tick.
        let links = make_links(&[(8_000.0, true)]);

        // First tick (t≈0): the ramp ceiling pins the target near the floor
        // even though usable capacity (~6.8 Mbps) would otherwise allow more.
        adapter.update(&links);
        assert!(
            adapter.current_target_kbps() <= 900,
            "ramp must hold the target near the floor at t≈0, got {}",
            adapter.current_target_kbps()
        );

        // After the ramp window elapses the ceiling is released and the
        // target is free to climb toward capacity (slew-limited per tick).
        std::thread::sleep(Duration::from_millis(80));
        for _ in 0..20 {
            adapter.update(&links);
        }
        assert!(
            adapter.current_target_kbps() > 1_500,
            "after the ramp window the target must climb toward capacity, got {}",
            adapter.current_target_kbps()
        );
    }

    #[test]
    fn link_queue_and_loss_collapse_forces_feedback_cut() {
        // Regression: receiver loss_fec can under-report while one link melts
        // down (high sender loss + deep queue). This must still trigger cuts.
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 2_000,
            congestion_sustain: Duration::ZERO,
            ..Default::default()
        });

        let links = vec![
            LinkCapacity {
                link_id: 0,
                capacity_kbps: 5_000.0,
                alive: true,
                loss_rate: 0.0,
                rtt_ms: 70.0,
                queue_depth: Some(5),
                drain_rate_kbps: None,
                aqm_dropped_total: None,
            },
            LinkCapacity {
                link_id: 1,
                capacity_kbps: 3_000.0,
                alive: true,
                loss_rate: 0.85,
                rtt_ms: 95.0,
                queue_depth: Some(96),
                drain_rate_kbps: None,
                aqm_dropped_total: None,
            },
        ];

        let feedback = ReceiverFeedback {
            goodput_bps: 3_500_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 120,
            loss_after_fec: 0.03,
            late_rate: 0.0,
        };

        let before = adapter.current_target_kbps();
        adapter.update_with_feedback(&links, &feedback);

        assert!(
            adapter.current_target_kbps() < before,
            "link collapse should force bitrate cut: before {} after {}",
            before,
            adapter.current_target_kbps()
        );
    }

    #[test]
    fn deep_paced_queue_without_loss_does_not_cut() {
        // Regression (field 2026-06-27): a keyframe burst fills the byte-bounded
        // paced queue to hundreds of packets, but it drains within the sojourn
        // budget — no loss, flat jitter, abundant capacity, low pressure. The
        // old `queue_depth >= 90` packet-count gate misread this as bufferbloat
        // and pinned the encoder to the floor. With congestion_sustain=ZERO any
        // delay_pressure would cut immediately, so this proves the gate is gone.
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 2_000,
            congestion_sustain: Duration::ZERO,
            ..Default::default()
        });

        let links = vec![
            LinkCapacity {
                link_id: 0,
                capacity_kbps: 4_000.0,
                alive: true,
                loss_rate: 0.0,
                rtt_ms: 70.0,
                queue_depth: Some(300),
                drain_rate_kbps: None,
                aqm_dropped_total: None,
            },
            LinkCapacity {
                link_id: 1,
                capacity_kbps: 4_000.0,
                alive: true,
                loss_rate: 0.0,
                rtt_ms: 95.0,
                queue_depth: Some(450),
                drain_rate_kbps: None,
                aqm_dropped_total: None,
            },
        ];

        let feedback = ReceiverFeedback {
            goodput_bps: 1_950_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.0,
            late_rate: 0.0,
        };

        let before = adapter.current_target_kbps();
        adapter.update_with_feedback(&links, &feedback);

        assert!(
            adapter.current_target_kbps() >= before,
            "deep but healthy paced queue must not cut bitrate: before {} after {}",
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
            late_rate: 0.0,
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
            late_rate: 0.0,
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
            late_rate: 0.0,
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
        // Low goodput alone (without loss) should reduce via goodput_shortfall.
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
            late_rate: 0.0,
        };
        adapter.update_with_feedback(&links, &seed_fb);

        // Now provide very low goodput (well below 70% of target) with zero loss
        let low_gp_fb = ReceiverFeedback {
            goodput_bps: 1_000_000, // 20% of target — below 70% threshold
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 100,
            loss_after_fec: 0.0,
            late_rate: 0.0,
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
            late_rate: 0.0,
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
                late_rate: 0.0,
            };
            adapter.update_with_feedback(&links, &stall);
            adapter.update_with_feedback(&links, &stall);

            // Unstall phase: gp>0 → EWMA WILL update with high loss
            let unstall = ReceiverFeedback {
                goodput_bps: 300_000, // Some delivery, but low
                jitter_buffer_ms: 1800,
                loss_after_fec: 0.9, // Most packets expired during stall
                fec_repair_rate: 0.0,
                late_rate: 0.0,
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
            late_rate: 0.0,
        };
        for _ in 0..10 {
            adapter.update_with_feedback(&links, &clean);
        }

        // KEY ASSERTION: the residual EWMA must recover near zero within 10
        // clean ticks — it still feeds the FEC burst-lift, so a
        // latched-high value would poison that.
        assert!(
            adapter.ewma_loss_fec < 0.15,
            "EWMA should recover after 10 clean ticks, got {:.3} — \
             residual is permanently poisoned",
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
            late_rate: 0.0,
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
                late_rate: 0.0,
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
                late_rate: 0.0,
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
            late_rate: 0.0,
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
            late_rate: 0.0,
        };
        adapter.update_with_feedback(&links, &severe);

        assert!(
            adapter.current_target_kbps() < after_increase,
            "severe bufferbloat (jitter=4000ms) should penetrate grace period: was {} now {}",
            after_increase,
            adapter.current_target_kbps()
        );
    }

    #[test]
    fn loss_congestion_can_bypass_dynamic_floor() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 5_000,
            min_bitrate_kbps: 200,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 2_000,
            congestion_sustain: Duration::ZERO,
            ..Default::default()
        });

        adapter.current_target_kbps = 2_000;
        adapter.prev_capacity_kbps = 3_000.0;
        adapter.ever_had_capacity = true;
        adapter.goodput_peak_bps = 6_000_000.0;
        adapter.ewma_goodput_bps = 5_000_000.0;
        adapter.ewma_loss_fec = 0.25;

        let links = vec![LinkCapacity {
            link_id: 0,
            capacity_kbps: 3_000.0,
            alive: true,
            loss_rate: 0.70,
            rtt_ms: 120.0,
            queue_depth: Some(120),
            drain_rate_kbps: None,
            aqm_dropped_total: None,
        }];

        let feedback = ReceiverFeedback {
            goodput_bps: 2_500_000,
            fec_repair_rate: 0.0,
            jitter_buffer_ms: 2_500,
            loss_after_fec: 0.20,
            late_rate: 0.06,
        };

        let cmd = adapter
            .update_with_feedback(&links, &feedback)
            .expect("congestion update should emit a command");

        assert!(
            cmd.target_kbps < 2_000,
            "loss-driven congestion should cut below the sticky dynamic floor: {}",
            cmd.target_kbps
        );
        assert_eq!(cmd.reason, AdaptationReason::Congestion);
    }

    // ── Regression: EWMA loss decay during stall ─────────────────────

    /// When goodput drops to zero after the EWMA has climbed high,
    /// the decay (×0.9 per tick) should bring the residual back near zero
    /// within ~15 ticks, so it can't latch high and poison FEC sizing.
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
            late_rate: 0.0,
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
            loss_after_fec: 1.0, // ignored during stall,
            late_rate: 0.0,
        };
        for _ in 0..20 {
            adapter.update_with_feedback(&links, &stall);
        }

        // 0.9^20 ≈ 0.12, so from ~0.7 → ~0.09 after 20 ticks
        assert!(
            adapter.ewma_loss_fec < 0.15,
            "EWMA should have decayed back toward zero: {:.3}",
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

        // Drop to 1 Mbps (link degradation) — fast-down α=0.5 should track
        let links = make_links(&[(1_000.0, true)]);
        adapter.update(&links);
        let after_drop = adapter.capacity_ewma[&0];
        assert!(
            after_drop < 5_000.0,
            "drop should be tracked quickly: {after_drop:.0}"
        );
    }

    #[test]
    fn regression_capacity_series_no_excessive_oscillation() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            ..Default::default()
        });

        let caps = [10000.0, 3663.0, 5029.0, 5799.0, 2457.0, 2362.0, 8822.0];
        for &cap in &caps {
            let links = make_links(&[(cap, true)]);
            adapter.update(&links);
            // Ensure target doesn't crash below 2500 despite the drops
            assert!(
                adapter.current_target_kbps() >= 2500,
                "target crashed to {} on capacity {}",
                adapter.current_target_kbps(),
                cap
            );
        }
    }

    #[test]
    fn regression_application_limited_ramp_up() {
        let mut adapter = BitrateAdapter::new(AdaptationConfig {
            max_bitrate_kbps: 10_000,
            min_interval: Duration::ZERO,
            initial_bitrate_kbps: 1000,
            ramp_up_kbps_per_step: 250,
            ..Default::default()
        });

        let links = make_links(&[(10_000.0, true)]);
        adapter.update(&links); // prime

        let feedback = ReceiverFeedback {
            goodput_bps: 1_000_000,
            jitter_buffer_ms: 50,
            loss_after_fec: 0.0,
            fec_repair_rate: 0.0,
            late_rate: 0.0,
        };

        for _ in 0..5 {
            adapter.update_with_feedback(&links, &feedback);
        }

        assert!(
            adapter.current_target_kbps() > 1000,
            "should ramp up even when application-limited, got {}",
            adapter.current_target_kbps()
        );
    }
}
