//! # Biscay Congestion Control
//!
//! Radio-aware congestion control extending BBRv3 with cellular modem
//! intelligence. Named for the Bay of Biscay (rough waters).
//!
//! ## State Machine (from Master Plan §5)
//!
//! ```text
//!                     ┌──────────┐
//!           ┌────────▶│  NORMAL  │◀────────┐
//!           │         └────┬─────┘         │
//!      CQI stable    CQI dropping     Handover
//!      & RSRP OK     (3 readings)     complete
//!           │              │               │
//!           │         ┌────▼─────┐         │
//!           │         │ CAUTIOUS │         │
//!           │         │ (-30%)   │         │
//!           │         └────┬─────┘         │
//!           │         RSRP slope           │
//!           │         < -2.5 dB/s          │
//!           │              │               │
//!           │         ┌────▼─────┐         │
//!           └─────────│PRE_HAND- │─────────┘
//!                     │  OVER    │
//!                     └──────────┘
//! ```
//!
//! ## BBRv3 Base Layer
//!
//! Per-link: models bottleneck bandwidth (BtlBw) and min RTT (RTprop).
//! Radio feed-forward adds SINR→capacity ceiling and CQI derivative tracking.

use quanta::Instant;
use std::collections::VecDeque;
use std::time::Duration;
use tracing::debug;

// ─── Biscay State ───────────────────────────────────────────────────────────

/// Congestion control state per link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiscayState {
    /// Normal operation — full pacing rate.
    Normal,
    /// CQI dropping — proactive 30% rate reduction.
    Cautious,
    /// Pre-handover — drain queue, pause probing.
    PreHandover,
}

/// Inferred (or operator-pinned) path regime.
///
/// Per the versatility doctrine the *control* path never branches on this —
/// every mechanism is expressed relative to the link's own measured
/// baseline/variance (BDP, delay-gradient). This classification exists
/// purely so the system can **explain its own decisions** in metrics and so
/// auto-detection mis-fires (a bloated Wi-Fi AP mimicking cellular) are
/// visible and overridable, not so behaviour is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathRegime {
    /// Not enough measurement yet to classify.
    #[default]
    Unknown,
    /// ~ms RTT, near-zero loss, high capacity — loss is the signal, fill it.
    Fiber,
    /// Moderate RTT, variable capacity, deep buffers — delay-bounded.
    Cellular,
    /// Hundreds-of-ms RTT by design, low loss — must not be flagged stalled.
    Satellite,
    /// Low RTT but aggregation bursts / contention jitter.
    Wifi,
    /// Shallow buffer, persistent random loss — loss-driven, high util.
    Lossy,
}

impl PathRegime {
    pub fn as_str(&self) -> &'static str {
        match self {
            PathRegime::Unknown => "unknown",
            PathRegime::Fiber => "fiber",
            PathRegime::Cellular => "cellular",
            PathRegime::Satellite => "satellite",
            PathRegime::Wifi => "wifi",
            PathRegime::Lossy => "lossy",
        }
    }

    /// Parse an operator override string (`auto` → `None`).
    pub fn parse_override(s: &str) -> Option<PathRegime> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => None,
            "fiber" => Some(PathRegime::Fiber),
            "cellular" => Some(PathRegime::Cellular),
            "satellite" => Some(PathRegime::Satellite),
            "wifi" => Some(PathRegime::Wifi),
            "lossy" => Some(PathRegime::Lossy),
            _ => None,
        }
    }
}

/// BBRv3-inspired phase within Normal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BbrPhase {
    /// Initial slow start — exponential ramp.
    SlowStart,
    /// Steady-state bandwidth probing.
    ProbeBw,
    /// Periodic RTT probing (drain to measure RTprop).
    ProbeRtt,
}

// ─── Radio Metrics ──────────────────────────────────────────────────────────

/// RF metrics from the modem supervisor.
#[derive(Debug, Clone, Default)]
pub struct RadioMetrics {
    /// RSRP in dBm (e.g., -90).
    pub rsrp_dbm: f64,
    /// RSRQ in dB (e.g., -12).
    pub rsrq_db: f64,
    /// SINR in dB (e.g., 15.0).
    pub sinr_db: f64,
    /// CQI (0-15).
    pub cqi: u8,
    /// Measurement timestamp.
    pub timestamp: Option<Instant>,
}

// ─── Per-Link Congestion Controller ─────────────────────────────────────────

/// Biscay congestion controller for a single link.
pub struct BiscayController {
    // ─── State ───
    /// Current radio-aware state.
    pub state: BiscayState,
    /// BBR phase within Normal state.
    pub bbr_phase: BbrPhase,

    // ─── BBR estimates ───
    /// Estimated bottleneck bandwidth (bytes/sec).
    btl_bw: f64,
    /// Minimum RTT observed (RTprop) in µs.
    rt_prop_us: f64,
    /// Current pacing rate (bytes/sec).
    pacing_rate: f64,
    /// Congestion window (bytes).
    cwnd: f64,

    // ─── Bandwidth tracking ───
    /// Recent bandwidth samples with timestamps for BtlBw estimation.
    bw_samples: VecDeque<(Instant, f64)>,
    /// Max samples to keep.
    max_bw_samples: usize,
    /// Window duration for bandwidth samples — peaks older than this expire.
    bw_window: Duration,

    // ─── RTT tracking ───
    /// Recent RTT samples (µs) for RTprop estimation.
    rtt_samples: VecDeque<f64>,
    /// Sliding window of `(timestamp, rtt_us)` whose minimum *is* RTprop.
    ///
    /// RTprop must be a windowed-min propagation delay, never a recent RTT:
    /// a sample taken while the modem/RAN buffer is already bloated (e.g.
    /// 500 ms) would, as a lifetime minimum, pin the BDP cap astronomically
    /// high forever and the cap could never force a drain. Entries older
    /// than `rt_prop_expiry` expire out of the window exactly like BBR's
    /// windowed min_rtt, so any stale bloated sample self-heals.
    rt_prop_window: VecDeque<(Instant, f64)>,
    /// When the current windowed-min RTprop sample was observed.
    rt_prop_stamp: Instant,
    /// RTprop expiry — also the sliding-window length. Probe RTT if the
    /// current min is this old (forced drain keeps RTprop honest).
    rt_prop_expiry: Duration,

    // ─── Radio state ───
    /// CQI history for derivative tracking.
    cqi_history: VecDeque<(Instant, u8)>,
    /// RSRP history for slope tracking.
    rsrp_history: VecDeque<(Instant, f64)>,
    /// SINR → capacity ceiling (kbps).
    sinr_capacity_ceiling: Option<f64>,
    /// Number of consecutive CQI drops.
    consecutive_cqi_drops: u8,

    // ─── Timing ───
    /// When this controller was created.
    #[allow(dead_code)]
    created_at: Instant,
    /// Last time the state machine was evaluated.
    last_tick: Instant,

    // ─── Bufferbloat drain ───
    /// Multiplicative factor applied to pacing rate when bufferbloat is
    /// detected (RTT >> RTprop). Decays toward 0.1 on bloat, recovers
    /// toward 1.0 when RTT normalises. Persists across `update_pacing_rate`
    /// calls so the reduction actually sticks.
    drain_factor: f64,

    // ─── Phase-shifted probing ───
    /// Whether this link holds the round-robin probe token.
    ///
    /// When `true`, ProbeBw applies a 1.25× gain (UP phase) to measure spare
    /// capacity. When `false`, pacing stays at 1.0× (cruise) to avoid
    /// simultaneous probing across all bonded links.
    probe_allowed: bool,

    // ─── Path-regime observability (F6) ───
    /// Operator override; `None` = auto-infer from measurement.
    profile_override: Option<PathRegime>,
    /// Slow EWMA of the per-interval loss rate (0.0–1.0). Fed by the
    /// transport adapter; used only to *classify* the regime for metrics,
    /// never to gate control (loss already drives backoff via the CC).
    recent_loss_rate: f64,

    // ─── Delay-gradient signal (F3) ───
    /// EWMA of the receiver-reported relative-OWD gradient (µs). A
    /// sustained positive value means the bottleneck queue is filling.
    delay_grad_ewma: f64,
    /// EWMA of the gradient's absolute deviation — the link's own
    /// gradient *jitter*. "Queue building" is the gradient exceeding
    /// `k × this`, so the trip point is path-relative, never a constant.
    delay_grad_jitter: f64,
    /// True once at least one receiver delay-gradient sample has arrived.
    /// Until then the coarse RTT-ratio heuristic is the fallback delay
    /// signal (signal fusion: whichever fires first drives backoff).
    has_gradient_signal: bool,
    /// Number of gradient samples seen (warm-up gate).
    grad_samples: u32,
    /// Throttle for gradient-driven drain updates.
    last_grad_tick: Instant,
}

impl BiscayController {
    /// Create a new controller with default parameters.
    pub fn new() -> Self {
        let now = Instant::now();
        BiscayController {
            state: BiscayState::Normal,
            bbr_phase: BbrPhase::SlowStart,

            btl_bw: 0.0,
            rt_prop_us: f64::MAX,
            pacing_rate: 100_000.0, // 100 KB/s initial (conservative)
            cwnd: 14_000.0,         // ~10 packets initial window

            bw_samples: VecDeque::with_capacity(64),
            max_bw_samples: 64,
            bw_window: Duration::from_secs(10),

            rtt_samples: VecDeque::with_capacity(32),
            rt_prop_window: VecDeque::with_capacity(64),
            rt_prop_stamp: now,
            rt_prop_expiry: Duration::from_secs(10),

            cqi_history: VecDeque::with_capacity(16),
            rsrp_history: VecDeque::with_capacity(16),
            sinr_capacity_ceiling: None,
            consecutive_cqi_drops: 0,

            created_at: now,
            last_tick: now,

            drain_factor: 1.0,
            probe_allowed: true, // default: allowed until coordinator assigns tokens
            profile_override: None,
            recent_loss_rate: 0.0,
            delay_grad_ewma: 0.0,
            delay_grad_jitter: 0.0,
            has_gradient_signal: false,
            grad_samples: 0,
            last_grad_tick: now,
        }
    }

    /// Pin the path regime (operator escape hatch). `None` re-enables
    /// auto-inference. Does not change control behaviour — only the
    /// regime reported in metrics.
    pub fn set_profile_override(&mut self, regime: Option<PathRegime>) {
        self.profile_override = regime;
    }

    /// Feed the latest per-interval loss rate (0.0–1.0) so the regime
    /// classifier can distinguish a lossy link from a clean one. Slow EWMA
    /// so a single burst doesn't reclassify the path.
    pub fn observe_loss_rate(&mut self, loss_rate: f64) {
        let l = loss_rate.clamp(0.0, 1.0);
        self.recent_loss_rate = if self.recent_loss_rate == 0.0 {
            l
        } else {
            0.1 * l + 0.9 * self.recent_loss_rate
        };
    }

    /// Multiple of the link's own gradient *jitter* beyond which a
    /// sustained positive delay gradient counts as "queue building". This
    /// is a statistical noise multiple (≈3σ), not a network constant — it
    /// is correct on any path because `delay_grad_jitter` is measured
    /// per-link.
    const GRAD_TRIP_SIGMA: f64 = 3.0;

    /// Feed a receiver-reported relative-OWD gradient sample (µs, F3).
    ///
    /// This is the *primary* delay-pressure signal: it fires strictly
    /// before loss and, unlike the coarse RTT-ratio heuristic, is
    /// path-relative (compared to the link's own gradient jitter) so the
    /// same code is correct on fiber, cellular and satellite. It feeds the
    /// shared `drain_factor` knob, so it composes with loss-driven backoff
    /// (signal fusion — whichever fires first wins). The *specific* loading
    /// link is demoted; there is no global all-links reaction.
    pub fn on_delay_gradient_us(&mut self, grad_us: u32) {
        let g = grad_us as f64;
        // EWMA of the gradient and of its absolute deviation (jitter).
        let dev = (g - self.delay_grad_ewma).abs();
        self.delay_grad_ewma = if self.grad_samples == 0 {
            g
        } else {
            0.2 * g + 0.8 * self.delay_grad_ewma
        };
        self.delay_grad_jitter = if self.grad_samples == 0 {
            0.0
        } else {
            0.2 * dev + 0.8 * self.delay_grad_jitter
        };
        self.has_gradient_signal = true;
        self.grad_samples = self.grad_samples.saturating_add(1);

        // Need a propagation-delay reference to express the noise floor
        // path-relatively, and a short warm-up so jitter is meaningful.
        if self.rt_prop_us >= f64::MAX || self.grad_samples < 4 {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_grad_tick) < Duration::from_millis(100) {
            return;
        }
        self.last_grad_tick = now;

        // Trip point: max(kσ of this link's gradient jitter, 5% of its
        // own RTprop). Both terms are path-relative — no absolute constant.
        let trip = (Self::GRAD_TRIP_SIGMA * self.delay_grad_jitter).max(0.05 * self.rt_prop_us);
        if self.delay_grad_ewma > trip {
            // Severity scales with how far past the trip point we are.
            let over = (self.delay_grad_ewma / trip).clamp(1.0, 8.0);
            let decay = 1.0 - 0.05 * (over - 1.0).min(3.0);
            self.drain_factor = (self.drain_factor * decay).max(0.5);
        } else if self.delay_grad_ewma < 0.5 * trip {
            // Gradient back near baseline — queue drained, recover.
            self.drain_factor = (self.drain_factor + 0.05).min(1.0);
        }
        self.update_pacing_rate();
    }

    /// Current smoothed delay-gradient (µs) — observability.
    pub fn delay_gradient_us(&self) -> f64 {
        self.delay_grad_ewma
    }

    /// Opportunistic modem flow-control hook (F5).
    ///
    /// Some modems expose explicit transmit backpressure — Qualcomm/rmnet
    /// **QMAP DFC** (Data Flow Control) grant withdrawal, or vendor AT
    /// stats. When such a backend reports "slow down", forward it here and
    /// it composes with the loss/delay-gradient backoff through the same
    /// `drain_factor` knob. This is **strictly additive**: if no modem
    /// backend ever calls it, nothing changes (no QMI/MBIM dependency is
    /// introduced). A grant restoration (`slow_down = false`) lets
    /// `drain_factor` recover.
    pub fn on_modem_flow_control(&mut self, slow_down: bool) {
        if slow_down {
            // The modem firmware itself says its TX ring is backing up —
            // an authoritative, earliest-possible congestion signal. Drain
            // gently toward the same safety floor the other signals use.
            self.drain_factor = (self.drain_factor * 0.9).max(0.5);
        } else {
            self.drain_factor = (self.drain_factor + 0.05).min(1.0);
        }
        self.update_pacing_rate();
    }

    /// The regime currently in effect: the operator override if set,
    /// otherwise inferred from the measured path. Pure observability — the
    /// control path is path-relative and never branches on this.
    ///
    /// Inference is expressed against the link's own windowed-min RTprop
    /// (`rt_prop_us`), capacity (`btl_bw`) and loss EWMA. The cut points are
    /// classification boundaries for a human-readable label, not control
    /// thresholds: a wrong label degrades observability, never behaviour.
    pub fn inferred_regime(&self) -> PathRegime {
        if let Some(forced) = self.profile_override {
            return forced;
        }
        if self.rt_prop_us >= f64::MAX || self.btl_bw <= 0.0 {
            return PathRegime::Unknown;
        }
        let rtprop_ms = self.rt_prop_us / 1000.0;
        let loss = self.recent_loss_rate;
        // Satellite: geostationary one-way ≈ 250 ms → RTT ≥ ~400 ms.
        if rtprop_ms >= 400.0 {
            return PathRegime::Satellite;
        }
        // Fiber: ~ms RTT and effectively loss-free.
        if rtprop_ms <= 8.0 && loss < 0.005 {
            return PathRegime::Fiber;
        }
        // Lossy: persistent random loss dominates regardless of RTT.
        if loss >= 0.02 {
            return PathRegime::Lossy;
        }
        // Wi-Fi vs cellular: both are buffered/jittery; without an RF feed
        // the honest call at low RTT with some loss is Wi-Fi, otherwise the
        // bonded-modem common case is cellular.
        if rtprop_ms <= 20.0 {
            PathRegime::Wifi
        } else {
            PathRegime::Cellular
        }
    }

    /// Allow or inhibit ProbeBw UP-gain for phase-shifted multi-link probing.
    ///
    /// When the bonding layer coordinates probing across links, only one link
    /// at a time should hold `probe_allowed = true`. All others cruise at 1.0×
    /// BtlBw until they receive the rotating probe token.
    pub fn set_probe_allowed(&mut self, allowed: bool) {
        self.probe_allowed = allowed;
    }

    /// Seed the congestion controller with a probe-measured bandwidth.
    ///
    /// Called when a saturation probe completes. Clears stale BW samples,
    /// pushes the probe result as the sole sample, and recalculates pacing.
    /// This breaks the feedback loop where btl_bw only tracks delivery rate
    /// (which is limited by the scheduler's own allocation).
    pub fn seed_bandwidth(&mut self, bw_bytes_sec: f64) {
        if bw_bytes_sec <= 0.0 {
            return;
        }
        // Clear old feedback-dependent samples and seed with probe result
        self.bw_samples.clear();
        self.bw_samples.push_back((Instant::now(), bw_bytes_sec));
        self.btl_bw = bw_bytes_sec;

        // If still in SlowStart, transition to ProbeBw since we now have
        // a reliable capacity measurement.
        if self.bbr_phase == BbrPhase::SlowStart {
            self.bbr_phase = BbrPhase::ProbeBw;
        }

        tracing::info!(
            target: "strata::cc",
            seeded_kbps = bw_bytes_sec * 8.0 / 1000.0,
            "CC seeded from saturation probe"
        );

        self.update_pacing_rate();
    }

    // ─── Getters ────────────────────────────────────────────────────────

    /// Get the current pacing rate in bytes/sec.
    pub fn pacing_rate(&self) -> f64 {
        self.pacing_rate
    }

    /// Get the estimated bottleneck bandwidth in bytes/sec.
    pub fn btl_bw(&self) -> f64 {
        self.btl_bw
    }

    /// Get the minimum RTT (RTprop) in µs.
    pub fn rt_prop_us(&self) -> f64 {
        self.rt_prop_us
    }

    /// Get the congestion window in bytes.
    pub fn cwnd(&self) -> f64 {
        self.cwnd
    }

    /// Get the bufferbloat drain factor (0.2–1.0).
    pub fn drain_factor(&self) -> f64 {
        self.drain_factor
    }

    /// Bandwidth-delay product in bytes: `btl_bw × RTprop`.
    ///
    /// Returns `0.0` until both a bandwidth estimate and a windowed-min
    /// RTprop exist. This is the scale-free anchor for the inflight / queue
    /// cap: it auto-sizes to the path (satellite → huge, fiber → large,
    /// cellular → modest) with no per-regime constant. Because RTprop is a
    /// windowed minimum (see `rt_prop_window`), a one-off bloated RTT can
    /// not inflate the BDP permanently.
    pub fn bdp_bytes(&self) -> f64 {
        if self.btl_bw > 0.0 && self.rt_prop_us < f64::MAX {
            (self.btl_bw * (self.rt_prop_us / 1_000_000.0)).max(0.0)
        } else {
            0.0
        }
    }

    /// Inflight / queue cap in bytes: `k × BDP`.
    ///
    /// `k` is a small headroom multiple (~1.25). Returns `0.0` when the BDP
    /// is not yet known, signalling the caller to fall back to its own
    /// bootstrap bound rather than clamp to zero.
    pub fn inflight_cap_bytes(&self, k: f64) -> f64 {
        let bdp = self.bdp_bytes();
        if bdp > 0.0 { bdp * k.max(1.0) } else { 0.0 }
    }

    // ─── BBR feedback processing ────────────────────────────────────────

    /// Process a bandwidth sample (from ACK feedback).
    /// `delivered_bytes`: bytes acknowledged in this interval.
    /// `interval_us`: time interval in µs.
    /// `is_app_limited`: true if the sender did not have enough data in flight to fill the pipe.
    pub fn on_bandwidth_sample(
        &mut self,
        delivered_bytes: u64,
        interval_us: u64,
        is_app_limited: bool,
    ) {
        if interval_us == 0 {
            return;
        }
        let now = Instant::now();
        let bw = delivered_bytes as f64 / (interval_us as f64 / 1_000_000.0);

        // If app-limited, only use the sample if it's higher than our
        // current estimate. Otherwise, we'd artificially lower our
        // capacity estimate just because the app isn't sending enough.
        if is_app_limited && bw < self.btl_bw {
            debug!(
                target: "strata::cc",
                bw_sample_Bps = bw,
                btl_bw_Bps = self.btl_bw,
                app_limited = is_app_limited,
                "BW sample REJECTED (app-limited & below btl_bw)"
            );
            return;
        }

        let prev_btl_bw = self.btl_bw;

        // Expire samples older than the bandwidth window.
        let cutoff = now - self.bw_window;
        while let Some(&(ts, _)) = self.bw_samples.front() {
            if ts < cutoff {
                self.bw_samples.pop_front();
            } else {
                break;
            }
        }

        self.bw_samples.push_back((now, bw));
        if self.bw_samples.len() > self.max_bw_samples {
            self.bw_samples.pop_front();
        }

        // BtlBw estimation uses a two-phase strategy:
        //  - Startup (< MIN_STARTUP_SAMPLES): use max of all samples so the
        //    estimator discovers the peak delivery rate quickly.  Without this,
        //    early low samples (timing-dependent) can trap btl_bw in a low
        //    equilibrium: low btl_bw → low capacity → sender throttles →
        //    only low samples → btl_bw stays low.
        //  - Steady state (≥ MIN_STARTUP_SAMPLES): 75th-percentile filters
        //    ACK-burst outliers while still capturing genuine capacity changes.
        const MIN_STARTUP_SAMPLES: usize = 8;
        let mut sorted: Vec<f64> = self.bw_samples.iter().map(|&(_, v)| v).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        self.btl_bw = if sorted.len() < MIN_STARTUP_SAMPLES {
            // Startup: max — discover peak quickly
            sorted.last().copied().unwrap_or(0.0)
        } else {
            // Stable: 75th percentile — filter outliers
            let idx = ((sorted.len() as f64 * 0.75) as usize).min(sorted.len().saturating_sub(1));
            sorted.get(idx).copied().unwrap_or(0.0)
        };

        debug!(
            target: "strata::cc",
            bw_sample_Bps = bw,
            bw_sample_kbps = bw * 8.0 / 1000.0,
            prev_btl_bw_Bps = prev_btl_bw,
            new_btl_bw_Bps = self.btl_bw,
            btl_bw_kbps = self.btl_bw * 8.0 / 1000.0,
            app_limited = is_app_limited,
            delivered_bytes = delivered_bytes,
            interval_us = interval_us,
            num_samples = self.bw_samples.len(),
            phase = ?self.bbr_phase,
            "BW sample ACCEPTED"
        );

        self.update_pacing_rate();
    }

    /// Process an RTT sample (from ACK or PONG).
    pub fn on_rtt_sample(&mut self, rtt_us: f64) {
        if rtt_us <= 0.0 {
            return;
        }

        self.rtt_samples.push_back(rtt_us);
        if self.rtt_samples.len() > 32 {
            self.rtt_samples.pop_front();
        }

        // RTprop = minimum over a sliding window (NOT a lifetime minimum).
        // Push the sample, expire anything older than the window, then take
        // the min of what remains. `rt_prop_stamp` tracks when the *current
        // minimum* was seen so ProbeRtt fires a forced drain right as the
        // honest low sample is about to age out.
        let prev_rt_prop = self.rt_prop_us;
        let now = Instant::now();
        self.rt_prop_window.push_back((now, rtt_us));
        let cutoff = now.checked_sub(self.rt_prop_expiry);
        while let Some(&(ts, _)) = self.rt_prop_window.front() {
            if cutoff.is_some_and(|c| ts < c) {
                self.rt_prop_window.pop_front();
            } else {
                break;
            }
        }
        let mut min_rtt = f64::MAX;
        let mut min_ts = now;
        for &(ts, v) in &self.rt_prop_window {
            if v < min_rtt {
                min_rtt = v;
                min_ts = ts;
            }
        }
        if min_rtt < f64::MAX {
            self.rt_prop_us = min_rtt;
            self.rt_prop_stamp = min_ts;
        }

        debug!(
            target: "strata::cc",
            rtt_us = rtt_us,
            rtt_ms = rtt_us / 1000.0,
            rt_prop_us = self.rt_prop_us,
            prev_rt_prop_us = prev_rt_prop,
            drain_factor = self.drain_factor,
            "RTT sample"
        );

        // Coarse RTT-ratio bufferbloat heuristic — FALLBACK ONLY.
        //
        // The fixed 5×/3×/1.5× RTprop ratios are exactly the kind of
        // trial-fitted cellular constant the versatility doctrine warns
        // against (a stall detector on a 600 ms satellite link, an
        // over-trigger on aggregating Wi-Fi). Once the receiver-reported,
        // path-relative delay GRADIENT (F3) is live it is the authoritative
        // queue-build signal and owns `drain_factor`. This block then only
        // *recovers* drain_factor (never fights the gradient), and acts as
        // the primary detector solely during the brief window before the
        // first receiver report arrives (signal fusion: whichever fires
        // first). rt_prop_us ≥ 1 ms guards against sub-ms startup artifacts.
        if self.rt_prop_us >= 1_000.0 && self.rt_prop_us < f64::MAX {
            // Only update drain_factor periodically to avoid per-ACK overreaction
            if now.duration_since(self.last_tick) > Duration::from_millis(100) {
                if self.has_gradient_signal {
                    // Gradient owns backoff; here we only let drain_factor
                    // recover when RTT is unambiguously near baseline.
                    if rtt_us < self.rt_prop_us * 1.5 {
                        self.drain_factor = (self.drain_factor + 0.05).min(1.0);
                    }
                } else if rtt_us > self.rt_prop_us * 5.0 {
                    // Severe bloat — aggressive drain
                    self.drain_factor = (self.drain_factor * 0.85).max(0.5);
                } else if rtt_us > self.rt_prop_us * 3.0 {
                    // Moderate bloat — gentle drain
                    self.drain_factor = (self.drain_factor * 0.95).max(0.5);
                } else if rtt_us < self.rt_prop_us * 1.5 {
                    // RTT is near baseline — recover
                    self.drain_factor = (self.drain_factor + 0.05).min(1.0);
                }
                self.last_tick = now;
            }
        } else if self.drain_factor < 1.0 {
            // Time-based recovery: if rt_prop_us is stale/invalid, slowly
            // restore drain_factor so we don't stay permanently throttled.
            if now.duration_since(self.last_tick) > Duration::from_millis(100) {
                self.drain_factor = (self.drain_factor + 0.02).min(1.0);
                self.last_tick = now;
            }
        }

        self.update_pacing_rate();
    }

    // ─── Radio feed-forward ─────────────────────────────────────────────

    /// Update with new radio metrics from the modem supervisor.
    pub fn on_radio_metrics(&mut self, metrics: &RadioMetrics) {
        let now = Instant::now();

        // SINR → capacity ceiling
        self.sinr_capacity_ceiling = Some(sinr_to_capacity_kbps(metrics.sinr_db));

        // CQI derivative tracking
        self.cqi_history.push_back((now, metrics.cqi));
        if self.cqi_history.len() > 16 {
            self.cqi_history.pop_front();
        }

        // Track consecutive CQI drops
        if self.cqi_history.len() >= 2 {
            let prev = self.cqi_history[self.cqi_history.len() - 2].1;
            let curr = metrics.cqi;
            if curr < prev {
                self.consecutive_cqi_drops += 1;
            } else {
                self.consecutive_cqi_drops = 0;
            }
        }

        // RSRP slope tracking
        self.rsrp_history.push_back((now, metrics.rsrp_dbm));
        if self.rsrp_history.len() > 16 {
            self.rsrp_history.pop_front();
        }

        // Evaluate state transitions
        self.evaluate_state_transition();

        // Always re-evaluate pacing rate after radio metrics update
        // (SINR ceiling may have changed even without state transition)
        self.update_pacing_rate();
    }

    /// Evaluate state machine transitions based on radio metrics.
    fn evaluate_state_transition(&mut self) {
        match self.state {
            BiscayState::Normal => {
                // CQI dropping for 3+ readings → CAUTIOUS
                if self.consecutive_cqi_drops >= 3 {
                    self.state = BiscayState::Cautious;
                    self.pacing_rate *= 0.7; // 30% reduction
                }
            }
            BiscayState::Cautious => {
                // RSRP slope < -2.5 dB/s AND RSRQ < -12 → PRE_HANDOVER
                let rsrp_slope = self.rsrp_slope_db_per_sec();
                let latest_rsrp = self.rsrp_history.back().map(|(_, v)| *v).unwrap_or(0.0);

                if rsrp_slope < -2.5 && latest_rsrp < -12.0 {
                    self.state = BiscayState::PreHandover;
                }

                // CQI stable (no drops for 3 readings) → NORMAL
                if self.consecutive_cqi_drops == 0 && self.cqi_history.len() >= 3 {
                    self.state = BiscayState::Normal;
                    self.update_pacing_rate();
                }
            }
            BiscayState::PreHandover => {
                // RSRP stabilized (slope > -1.0 for 3 readings) → NORMAL
                let rsrp_slope = self.rsrp_slope_db_per_sec();
                if rsrp_slope > -1.0 && self.rsrp_history.len() >= 3 {
                    self.state = BiscayState::Normal;
                    // Reset BBR state for re-probing
                    self.bbr_phase = BbrPhase::SlowStart;
                    self.bw_samples.clear();
                    self.btl_bw = 0.0;
                    self.update_pacing_rate();
                }
            }
        }
    }

    /// Compute RSRP slope in dB/sec from recent history.
    fn rsrp_slope_db_per_sec(&self) -> f64 {
        if self.rsrp_history.len() < 2 {
            return 0.0;
        }
        let (t1, v1) = self.rsrp_history[0];
        let (t2, v2) = *self.rsrp_history.back().unwrap();
        let dt = t2.duration_since(t1).as_secs_f64();
        if dt < 0.001 {
            return 0.0;
        }
        (v2 - v1) / dt
    }

    /// Update pacing rate from BBR estimates and radio constraints.
    fn update_pacing_rate(&mut self) {
        let mut rate = match self.bbr_phase {
            BbrPhase::SlowStart => {
                // Stay in SlowStart (reported as Probe phase to scheduler)
                // until we accumulate MIN_CALIBRATION_SAMPLES of delivery-
                // rate data.  During SlowStart the scheduler applies a flat
                // capacity floor so all links receive roughly equal traffic.
                // This "calibration period" lets each link's btl_bw converge
                // to its true bottleneck rate under uniform load before the
                // scheduler switches to EDPF arrival-time selection.
                //
                // With ~3.3 samples/sec per link, 30 samples ≈ 9 seconds
                // of calibration — long enough for the 10 s bw_window to
                // fill and the 75th-percentile filter to stabilise.
                const MIN_CALIBRATION_SAMPLES: usize = 30;
                if self.btl_bw > 0.0 && self.bw_samples.len() >= MIN_CALIBRATION_SAMPLES {
                    self.bbr_phase = BbrPhase::ProbeBw;
                    self.btl_bw
                } else if self.btl_bw > 0.0 {
                    // Have an estimate but still in calibration —
                    // use btl_bw for pacing but stay in SlowStart so the
                    // link reports Probe phase to the EDPF scheduler.
                    self.btl_bw
                } else {
                    // No bandwidth data yet — keep current pacing_rate.
                    return;
                }
            }
            BbrPhase::ProbeBw => {
                // Pacing rate = BtlBw × pacing_gain.
                // Only apply the UP-probe gain (1.25×) when this link holds the
                // phase-shifted probe token; otherwise cruise at 1.0× to prevent
                // simultaneous probing from all bonded links.
                let gain = if self.probe_allowed { 1.25 } else { 1.0 };
                self.btl_bw * gain
            }
            BbrPhase::ProbeRtt => {
                // Minimal sending during RTT probe
                self.btl_bw * 0.5
            }
        };

        // Apply radio-aware state dampening
        match self.state {
            BiscayState::Cautious => rate *= 0.7,
            BiscayState::PreHandover => rate *= 0.1, // minimal — drain only
            BiscayState::Normal => {}
        }

        // Enforce SINR capacity ceiling
        if let Some(ceiling_kbps) = self.sinr_capacity_ceiling {
            let ceiling_bps = ceiling_kbps * 1000.0 / 8.0; // kbps → bytes/sec
            rate = rate.min(ceiling_bps);
        }

        // Apply bufferbloat drain factor
        rate *= self.drain_factor;

        // Minimum pacing rate: 10 KB/s
        self.pacing_rate = rate.max(10_000.0);

        // Update cwnd = BDP = BtlBw × RTprop
        if self.btl_bw > 0.0 && self.rt_prop_us < f64::MAX {
            self.cwnd = self.btl_bw * (self.rt_prop_us / 1_000_000.0);
            // Minimum cwnd: 2 packets
            self.cwnd = self.cwnd.max(2800.0);
        }

        debug!(
            target: "strata::cc",
            pacing_rate_Bps = self.pacing_rate,
            pacing_rate_kbps = self.pacing_rate * 8.0 / 1000.0,
            btl_bw_Bps = self.btl_bw,
            btl_bw_kbps = self.btl_bw * 8.0 / 1000.0,
            drain_factor = self.drain_factor,
            cwnd = self.cwnd,
            phase = ?self.bbr_phase,
            state = ?self.state,
            sinr_ceiling_kbps = self.sinr_capacity_ceiling,
            probe_allowed = self.probe_allowed,
            "pacing rate updated"
        );
    }

    /// Periodic tick — check for RTprop probe, state transitions.
    pub fn tick(&mut self) {
        let now = Instant::now();
        self.last_tick = now;

        // Check if RTprop needs re-probing
        if now.duration_since(self.rt_prop_stamp) > self.rt_prop_expiry
            && self.bbr_phase == BbrPhase::ProbeBw
        {
            self.bbr_phase = BbrPhase::ProbeRtt;
        }

        // Exit ProbeRtt after 200ms
        if self.bbr_phase == BbrPhase::ProbeRtt
            && now.duration_since(self.rt_prop_stamp)
                > self.rt_prop_expiry + Duration::from_millis(200)
        {
            self.bbr_phase = BbrPhase::ProbeBw;
            // The ProbeRtt drain exists precisely to obtain a fresh,
            // un-bloated RTprop sample. Discard the stale window (it may be
            // full of samples taken while the queue was bloated) and reseed
            // it with the post-drain reading. This also prevents BDP from
            // being computed against a stale inflated RTprop, and keeps
            // bandwidth samples from being rejected as app-limited (BDP =
            // btl_bw × ∞ when rt_prop = MAX).
            if let Some(&latest) = self.rtt_samples.back() {
                self.rt_prop_us = latest;
                self.rt_prop_window.clear();
                self.rt_prop_window.push_back((now, latest));
            }
            self.rt_prop_stamp = now;
        }
    }

    /// Whether this link should accept new packets (not in PRE_HANDOVER drain).
    pub fn can_enqueue(&self) -> bool {
        self.state != BiscayState::PreHandover
    }

    /// Bytes allowed to send in the next interval (for pacing).
    pub fn bytes_to_send(&self, interval_us: u64) -> usize {
        let bytes = self.pacing_rate * (interval_us as f64 / 1_000_000.0);
        bytes.max(0.0) as usize
    }
}

impl Default for BiscayController {
    fn default() -> Self {
        Self::new()
    }
}

// ─── SINR → Capacity Lookup ────────────────────────────────────────────────

/// Map SINR (dB) to approximate LTE/5G PHY capacity (kbps).
/// Based on simplified 3GPP MCS tables.
fn sinr_to_capacity_kbps(sinr_db: f64) -> f64 {
    // Simplified piecewise-linear mapping
    if sinr_db < -5.0 {
        100.0 // Barely usable
    } else if sinr_db < 0.0 {
        500.0
    } else if sinr_db < 5.0 {
        2_000.0
    } else if sinr_db < 10.0 {
        5_000.0
    } else if sinr_db < 15.0 {
        10_000.0
    } else if sinr_db < 20.0 {
        20_000.0
    } else if sinr_db < 25.0 {
        40_000.0
    } else {
        80_000.0 // Very strong signal (mmWave or close to tower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── State Machine Tests ────────────────────────────────────────────

    #[test]
    fn initial_state_is_normal_slow_start() {
        let cc = BiscayController::new();
        assert_eq!(cc.state, BiscayState::Normal);
        assert_eq!(cc.bbr_phase, BbrPhase::SlowStart);
        assert!(cc.pacing_rate() > 0.0);
    }

    #[test]
    fn normal_to_cautious_on_cqi_drops() {
        let mut cc = BiscayController::new();

        // Simulate 4 consecutive CQI drops
        for cqi in [15, 12, 10, 8u8] {
            cc.on_radio_metrics(&RadioMetrics {
                cqi,
                sinr_db: 10.0,
                rsrp_dbm: -80.0,
                rsrq_db: -8.0,
                timestamp: Some(Instant::now()),
            });
        }

        assert_eq!(
            cc.state,
            BiscayState::Cautious,
            "should transition to Cautious after 3+ CQI drops"
        );
    }

    #[test]
    fn cautious_reduces_pacing_rate() {
        let mut cc = BiscayController::new();
        // Give it a bandwidth estimate first so pacing rate is meaningful
        cc.on_bandwidth_sample(500_000, 1_000_000, false); // 500 KB/s
        cc.on_rtt_sample(20_000.0);

        // Record rate before entering Cautious — use high SINR so ceiling doesn't interfere
        let rate_before_cautious = cc.pacing_rate();

        // Force into Cautious via CQI drops (use high SINR to avoid ceiling effect)
        for cqi in [15, 12, 10, 8u8] {
            cc.on_radio_metrics(&RadioMetrics {
                cqi,
                sinr_db: 30.0, // very high — ceiling won't interfere
                rsrp_dbm: -80.0,
                rsrq_db: -8.0,
                timestamp: Some(Instant::now()),
            });
        }

        assert_eq!(cc.state, BiscayState::Cautious);
        assert!(
            cc.pacing_rate() < rate_before_cautious,
            "Cautious state should reduce pacing rate: {} vs {}",
            cc.pacing_rate(),
            rate_before_cautious
        );
    }

    #[test]
    fn pre_handover_disables_enqueue() {
        let mut cc = BiscayController::new();
        cc.state = BiscayState::PreHandover;
        assert!(!cc.can_enqueue(), "PreHandover should block new enqueues");
    }

    #[test]
    fn normal_allows_enqueue() {
        let cc = BiscayController::new();
        assert!(cc.can_enqueue());
    }

    // ─── BBR Bandwidth Estimation Tests ─────────────────────────────────

    #[test]
    fn bandwidth_sample_updates_btlbw() {
        let mut cc = BiscayController::new();
        assert_eq!(cc.btl_bw(), 0.0);

        // 1 MB in 1 second = 1 MB/s
        cc.on_bandwidth_sample(1_000_000, 1_000_000, false);
        assert!(cc.btl_bw() > 0.0);
        assert!((cc.btl_bw() - 1_000_000.0).abs() < 1.0);
    }

    #[test]
    fn btlbw_is_max_of_recent_samples() {
        let mut cc = BiscayController::new();

        // Low sample
        cc.on_bandwidth_sample(100_000, 1_000_000, false); // 100 KB/s
        let bw1 = cc.btl_bw();

        // High sample
        cc.on_bandwidth_sample(500_000, 1_000_000, false); // 500 KB/s
        let bw2 = cc.btl_bw();

        assert!(bw2 > bw1, "BtlBw should take max");

        // Low sample again — max shouldn't decrease (within window)
        cc.on_bandwidth_sample(50_000, 1_000_000, false);
        assert_eq!(cc.btl_bw(), bw2);

        // Simulate window expiry: shrink window to 0 and feed low sample
        cc.bw_window = Duration::from_millis(0);
        cc.on_bandwidth_sample(50_000, 1_000_000, false); // 50 KB/s
        assert!(
            cc.btl_bw() < bw2,
            "BtlBw should decrease after old peaks expire"
        );
    }

    #[test]
    fn zero_interval_ignored() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(1000, 0, false); // division by zero guard
        assert_eq!(cc.btl_bw(), 0.0);
    }

    // ─── RTT Tests ──────────────────────────────────────────────────────

    #[test]
    fn rtt_sample_updates_rtprop() {
        let mut cc = BiscayController::new();
        assert_eq!(cc.rt_prop_us(), f64::MAX);

        cc.on_rtt_sample(50_000.0); // 50ms
        assert_eq!(cc.rt_prop_us(), 50_000.0);

        cc.on_rtt_sample(40_000.0); // 40ms — new min
        assert_eq!(cc.rt_prop_us(), 40_000.0);

        cc.on_rtt_sample(60_000.0); // 60ms — doesn't change min
        assert_eq!(cc.rt_prop_us(), 40_000.0);
    }

    #[test]
    fn negative_rtt_ignored() {
        let mut cc = BiscayController::new();
        cc.on_rtt_sample(-100.0);
        assert_eq!(cc.rt_prop_us(), f64::MAX);
    }

    // ─── F2: windowed-min RTprop + BDP cap ──────────────────────────────

    #[test]
    fn rtprop_is_windowed_min_not_lifetime_min() {
        let mut cc = BiscayController::new();
        // A clean low sample, then the link bloats (queue fills): RTprop
        // must hold the low value while both are inside the window.
        cc.on_rtt_sample(20_000.0);
        assert_eq!(cc.rt_prop_us(), 20_000.0);
        cc.on_rtt_sample(500_000.0); // bloated
        assert_eq!(
            cc.rt_prop_us(),
            20_000.0,
            "windowed min must ignore the bloated sample, not adopt it"
        );

        // Force the clean sample to age out of the window: only the bloated
        // sample remains, so RTprop rises (cap can then force a drain).
        cc.rt_prop_window
            .iter_mut()
            .for_each(|(ts, _)| *ts = Instant::now() - Duration::from_secs(20));
        cc.on_rtt_sample(480_000.0);
        assert!(
            cc.rt_prop_us() > 100_000.0,
            "stale low sample must expire so a bloated RTprop self-heals, got {}",
            cc.rt_prop_us()
        );
    }

    #[test]
    fn bdp_zero_until_both_known() {
        let mut cc = BiscayController::new();
        assert_eq!(cc.bdp_bytes(), 0.0);
        cc.on_rtt_sample(50_000.0); // RTprop known, btl_bw still 0
        assert_eq!(cc.bdp_bytes(), 0.0);
        cc.on_bandwidth_sample(1_000_000, 1_000_000, false); // 1 MB/s
        // BDP = 1e6 B/s × 0.05 s = 50_000 B
        assert!((cc.bdp_bytes() - 50_000.0).abs() < 1_000.0);
    }

    #[test]
    fn inflight_cap_scales_with_path_no_constant() {
        let mut cc_cell = BiscayController::new();
        cc_cell.on_bandwidth_sample(1_000_000, 1_000_000, false); // 1 MB/s
        cc_cell.on_rtt_sample(60_000.0); // 60 ms
        let cell_cap = cc_cell.inflight_cap_bytes(1.25);

        let mut cc_sat = BiscayController::new();
        cc_sat.on_bandwidth_sample(1_000_000, 1_000_000, false); // 1 MB/s
        cc_sat.on_rtt_sample(600_000.0); // 600 ms (satellite)
        let sat_cap = cc_sat.inflight_cap_bytes(1.25);

        // Same expression, no branch: satellite's huge RTprop yields a
        // proportionally huge cap (it must fill its BDP, not be throttled).
        assert!(
            sat_cap > cell_cap * 5.0,
            "cap must auto-scale with RTprop: cell={cell_cap} sat={sat_cap}"
        );
        assert_eq!(BiscayController::new().inflight_cap_bytes(1.25), 0.0);
    }

    // ─── F6: regime inference + override ────────────────────────────────

    // ─── F5: opportunistic modem flow-control ──────────────────────────

    #[test]
    fn modem_flow_control_is_additive_and_recovers() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(1_000_000, 1_000_000, false);
        cc.on_rtt_sample(50_000.0);
        let before = cc.drain_factor();
        // Modem reports its TX ring backing up → gentle drain.
        cc.on_modem_flow_control(true);
        assert!(
            cc.drain_factor() < before,
            "modem backpressure must reduce pacing"
        );
        assert!(cc.drain_factor() >= 0.5, "must respect the safety floor");
        // Grants restored → recovers.
        for _ in 0..20 {
            cc.on_modem_flow_control(false);
        }
        assert!(
            cc.drain_factor() > 0.9,
            "drain must recover once grants return, got {}",
            cc.drain_factor()
        );
    }

    #[test]
    fn regime_unknown_before_measurement() {
        let cc = BiscayController::new();
        assert_eq!(cc.inferred_regime(), PathRegime::Unknown);
    }

    #[test]
    fn regime_inference_from_measured_path() {
        // Fiber: ~ms RTT, loss-free.
        let mut fiber = BiscayController::new();
        fiber.on_bandwidth_sample(100_000_000, 1_000_000, false);
        fiber.on_rtt_sample(2_000.0);
        assert_eq!(fiber.inferred_regime(), PathRegime::Fiber);

        // Satellite: 600 ms RTT.
        let mut sat = BiscayController::new();
        sat.on_bandwidth_sample(2_000_000, 1_000_000, false);
        sat.on_rtt_sample(600_000.0);
        assert_eq!(sat.inferred_regime(), PathRegime::Satellite);

        // Cellular: 60 ms RTT, light loss.
        let mut cell = BiscayController::new();
        cell.on_bandwidth_sample(3_000_000, 1_000_000, false);
        cell.on_rtt_sample(60_000.0);
        cell.observe_loss_rate(0.005);
        assert_eq!(cell.inferred_regime(), PathRegime::Cellular);

        // Lossy: persistent random loss.
        let mut lossy = BiscayController::new();
        lossy.on_bandwidth_sample(3_000_000, 1_000_000, false);
        lossy.on_rtt_sample(40_000.0);
        for _ in 0..30 {
            lossy.observe_loss_rate(0.05);
        }
        assert_eq!(lossy.inferred_regime(), PathRegime::Lossy);
    }

    // ─── F3: delay-gradient signal fusion ──────────────────────────────

    #[test]
    fn delay_gradient_drains_when_queue_builds() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(1_000_000, 1_000_000, false);
        cc.on_rtt_sample(60_000.0); // 60 ms RTprop reference
        let before = cc.drain_factor();

        // Quiet baseline: tiny, low-jitter gradient → no drain.
        for _ in 0..6 {
            cc.on_delay_gradient_us(50);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            (cc.drain_factor() - before).abs() < 1e-9,
            "a quiet gradient must not drain"
        );

        // Queue builds: gradient climbs far past its own jitter AND past
        // 5% of RTprop (3 ms) → drain_factor must fall.
        for _ in 0..12 {
            cc.on_delay_gradient_us(40_000); // 40 ms of standing queue
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            cc.drain_factor() < before,
            "sustained positive gradient must reduce drain_factor: {} !< {}",
            cc.drain_factor(),
            before
        );
        assert!(
            cc.drain_factor() >= 0.5,
            "drain must not collapse below the safety floor"
        );
    }

    #[test]
    fn gradient_signal_supersedes_rtt_ratio_heuristic() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(1_000_000, 1_000_000, false);
        cc.on_rtt_sample(50_000.0); // RTprop = 50 ms
        // Gradient says the link is clean (queue empty).
        for _ in 0..6 {
            cc.on_delay_gradient_us(100);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let df = cc.drain_factor();
        // A big RTT spike (8× RTprop) would, under the old fixed heuristic,
        // slam drain_factor down. With the gradient signal live and clean,
        // the coarse RTT path must NOT cut — it only recovers.
        std::thread::sleep(std::time::Duration::from_millis(120));
        cc.on_rtt_sample(400_000.0); // 8× RTprop
        assert!(
            cc.drain_factor() >= df,
            "live clean gradient must veto the coarse RTT-ratio drain: {} < {}",
            cc.drain_factor(),
            df
        );
    }

    #[test]
    fn rtt_heuristic_still_fallback_before_gradient_arrives() {
        // Before any receiver gradient sample, the coarse RTT heuristic is
        // the fallback delay detector (signal fusion: whichever fires first).
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(1_000_000, 1_000_000, false);
        cc.on_rtt_sample(50_000.0);
        let before = cc.drain_factor();
        std::thread::sleep(std::time::Duration::from_millis(120));
        cc.on_rtt_sample(400_000.0); // 8× RTprop, no gradient signal yet
        assert!(
            cc.drain_factor() < before,
            "RTT heuristic must still protect before the gradient is live"
        );
    }

    #[test]
    fn regime_override_wins() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(100_000_000, 1_000_000, false);
        cc.on_rtt_sample(2_000.0);
        assert_eq!(cc.inferred_regime(), PathRegime::Fiber);
        cc.set_profile_override(Some(PathRegime::Cellular));
        assert_eq!(cc.inferred_regime(), PathRegime::Cellular);
        cc.set_profile_override(None);
        assert_eq!(cc.inferred_regime(), PathRegime::Fiber);
        assert_eq!(PathRegime::parse_override("auto"), None);
        assert_eq!(
            PathRegime::parse_override("satellite"),
            Some(PathRegime::Satellite)
        );
        assert_eq!(PathRegime::parse_override("garbage"), None);
    }

    // ─── SINR Capacity Ceiling Tests ────────────────────────────────────

    #[test]
    fn sinr_ceiling_limits_pacing() {
        let mut cc = BiscayController::new();

        // Give it a high bandwidth estimate
        cc.on_bandwidth_sample(10_000_000, 1_000_000, false); // 10 MB/s
        cc.on_rtt_sample(10_000.0);

        let uncapped_rate = cc.pacing_rate();

        // Apply a low SINR ceiling
        cc.on_radio_metrics(&RadioMetrics {
            sinr_db: 0.0, // maps to ~500 kbps
            cqi: 5,
            rsrp_dbm: -90.0,
            rsrq_db: -10.0,
            timestamp: Some(Instant::now()),
        });

        // Rate should be capped below uncapped
        assert!(
            cc.pacing_rate() < uncapped_rate,
            "SINR ceiling should limit pacing rate: {} vs {}",
            cc.pacing_rate(),
            uncapped_rate
        );
    }

    #[test]
    fn sinr_to_capacity_ordering() {
        // Higher SINR should always yield higher capacity
        let c1 = sinr_to_capacity_kbps(-5.0);
        let c2 = sinr_to_capacity_kbps(0.0);
        let c3 = sinr_to_capacity_kbps(10.0);
        let c4 = sinr_to_capacity_kbps(25.0);
        assert!(c1 <= c2);
        assert!(c2 <= c3);
        assert!(c3 <= c4);
    }

    // ─── Pacing Budget Tests ────────────────────────────────────────────

    #[test]
    fn bytes_to_send_proportional_to_interval() {
        let cc = BiscayController::new();
        let b1 = cc.bytes_to_send(1_000); // 1ms
        let b2 = cc.bytes_to_send(10_000); // 10ms
        // 10× interval → ~10× bytes (not exact due to rounding)
        assert!(b2 > b1);
    }

    #[test]
    fn bytes_to_send_never_negative() {
        let mut cc = BiscayController::new();
        cc.state = BiscayState::PreHandover;
        cc.update_pacing_rate();
        let _bytes = cc.bytes_to_send(100);
        // usize is always >= 0, just testing it doesn't panic
    }

    // ─── Slow Start Transition ──────────────────────────────────────────

    #[test]
    fn slow_start_stays_until_enough_samples() {
        let mut cc = BiscayController::new();
        assert_eq!(cc.bbr_phase, BbrPhase::SlowStart);

        // Feed samples — need >= 30 (MIN_CALIBRATION_SAMPLES) for transition
        for _ in 0..29 {
            cc.on_bandwidth_sample(150_000, 1_000_000, false);
            assert_eq!(
                cc.bbr_phase,
                BbrPhase::SlowStart,
                "should stay in SlowStart with < 30 samples"
            );
        }

        // 30th sample triggers transition
        cc.on_bandwidth_sample(180_000, 1_000_000, false);
        assert_eq!(
            cc.bbr_phase,
            BbrPhase::ProbeBw,
            "should transition to ProbeBw with ≥ 30 samples"
        );
    }

    // ─── Bufferbloat Detection ──────────────────────────────────────────

    #[test]
    fn bufferbloat_reduces_rate() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(500_000, 1_000_000, false);
        cc.on_rtt_sample(20_000.0); // 20ms baseline

        let rate_before = cc.pacing_rate();

        // RTT spikes to 2× baseline (bufferbloat)
        cc.on_rtt_sample(40_000.0);
        // The pacing_rate gets reduced by 0.9 factor
        assert!(
            cc.pacing_rate() <= rate_before,
            "bufferbloat should reduce pacing rate"
        );
    }

    // ─── Seed Bandwidth Tests ───────────────────────────────────────────

    #[test]
    fn seed_bandwidth_updates_btlbw_and_pacing() {
        let mut cc = BiscayController::new();
        assert_eq!(cc.btl_bw(), 0.0);
        assert_eq!(cc.bbr_phase, BbrPhase::SlowStart);

        // Seed with 1 MB/s (bytes/sec)
        cc.seed_bandwidth(1_000_000.0);

        assert!(
            (cc.btl_bw() - 1_000_000.0).abs() < 1.0,
            "btl_bw should be seeded value"
        );
        assert_eq!(
            cc.bbr_phase,
            BbrPhase::ProbeBw,
            "should transition out of SlowStart after seeding"
        );
        assert!(
            cc.pacing_rate() > 100_000.0,
            "pacing rate should increase from probe seed"
        );
    }

    #[test]
    fn seed_bandwidth_clears_old_samples() {
        let mut cc = BiscayController::new();

        // Accumulate low-rate samples
        for _ in 0..10 {
            cc.on_bandwidth_sample(50_000, 1_000_000, false);
        }
        let low_bw = cc.btl_bw();

        // Seed with high probe result
        cc.seed_bandwidth(500_000.0);

        assert!(
            cc.btl_bw() > low_bw * 5.0,
            "seed should override low feedback samples"
        );
    }

    #[test]
    fn seed_bandwidth_ignores_zero() {
        let mut cc = BiscayController::new();
        cc.on_bandwidth_sample(100_000, 1_000_000, false);
        let before = cc.btl_bw();
        cc.seed_bandwidth(0.0);
        assert_eq!(cc.btl_bw(), before);
    }
}
