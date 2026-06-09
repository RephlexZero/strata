use bytes::Bytes;
use quanta::Instant;
use std::collections::VecDeque;
use std::time::Duration;

/// An incoming packet with its bonding sequence ID and arrival timestamp.
pub struct Packet {
    pub seq_id: u64,
    pub payload: Bytes,
    pub arrival_time: Instant,
    /// Sender wire send-time (µs, sender clock, wraps ~71 min). Used only
    /// for the *relative* per-packet delay `arrival − send_ts`: the
    /// constant clock offset cancels when we take the spread (max − min)
    /// over a short window, which is exactly the bonded inter-link arrival
    /// skew the playout buffer must absorb. Inter-arrival jitter (the old
    /// sole input) is blind to this — it reads ~19 ms while the real
    /// cross-link spread during a one-modem fade is 250–400 ms.
    pub send_ts_us: u32,
}

/// Jitter buffer that reorders and releases packets in sequence order.
///
/// Packets are held for at least the configured latency before release.
/// The latency adapts upward based on observed inter-arrival jitter
/// (p95 × multiplier), capped at `max_latency`. Missing packets are
/// skipped after the `skip_after` timeout to prevent head-of-line blocking.
pub struct ReassemblyBuffer {
    buffer: Vec<Option<Packet>>,
    capacity: usize,
    buffered: usize,
    next_seq: u64,
    latency: Duration,
    start_latency: Duration,
    skip_after: Option<Duration>,
    jitter_latency_multiplier: f64,
    max_latency: Duration,
    min_latency: Duration,
    pub lost_packets: u64,
    pub late_packets: u64,
    pub duplicate_packets: u64,
    pub discontinuities: u64,
    pub packets_delivered: u64,

    // Adaptive latency — jitter tracking
    last_arrival: Option<Instant>,
    avg_iat: f64,
    jitter_smoothed: f64,
    jitter_samples: VecDeque<f64>,

    // Adaptive latency — bonded inter-link delay-spread tracking.
    // `rel = arrival_since_epoch_us − send_ts_us`; the constant offset
    // cancels in the windowed (max − min) spread, which is the true
    // cross-link arrival skew the buffer must cover. Monotonic deques give
    // O(1) amortised sliding-window min & max over `DELAY_SPREAD_WINDOW`.
    epoch: Instant,
    rel_min_deque: VecDeque<(Instant, i64)>,
    rel_max_deque: VecDeque<(Instant, i64)>,
    delay_spread_us: i64,

    // Adaptive latency — bidirectional smoothing
    target_latency: Duration,
    ramp_up_alpha: f64,
    ramp_down_alpha: f64,
    stable_since: Option<Instant>,
    stability_threshold: Duration,

    // Adaptive latency — loss-aware sizing
    loss_rate_smoothed: f64,
    loss_penalty_ms: f64,

    // Adaptive latency — closed-loop late-arrival feedback.
    // Every late packet is hard evidence our deadline was too tight, so we
    // widen the buffer.  When clean (no late arrivals for a stable period),
    // this drains back toward zero.  AIMD keeps the buffer at "just enough"
    // regardless of the user's config, bounded only by max_latency.
    late_pressure_ms: f64,
    last_late_arrival: Option<Instant>,

    // Desync recovery: track consecutive late packets to detect when
    // next_seq has jumped ahead of the sender's actual sequence space.
    consecutive_late: u64,
    /// Highest seq_id seen among consecutive late packets — used as the
    /// resync target so we resume from the most recent sender position,
    /// not from an arbitrary old packet that happened to arrive last.
    max_late_seq: u64,
    /// Highest seq_id actually emitted downstream. Resync must never rewind
    /// to or below this — re-emitting an already-delivered sequence makes
    /// the downstream MPEG-TS demuxer see a PTS/continuity regression and
    /// post a fatal "Timestamping error on input streams".
    last_emitted_seq: Option<u64>,
}

/// Configuration for the reassembly jitter buffer.
#[derive(Debug, Clone)]
pub struct ReassemblyConfig {
    pub start_latency: Duration,
    pub buffer_capacity: usize,
    pub skip_after: Option<Duration>,
    /// Multiplier for p95 jitter in adaptive latency (default: 4.0)
    pub jitter_latency_multiplier: f64,
    /// Hard ceiling on adaptive reassembly latency (default: 3000 ms).
    pub max_latency_ms: u64,
    /// Floor for adaptive latency in ms (default: 1000 ms). Defense-in-depth
    /// clamp: late-pressure drain and downward smoothing must never let the
    /// buffer dip below this even if the formula transiently goes lower.
    pub min_latency_ms: u64,
    /// Smoothing factor for upward adaptation (default: 0.3 = fast ramp-up).
    pub ramp_up_alpha: f64,
    /// Smoothing factor for downward adaptation (default: 0.02 = slow ramp-down).
    pub ramp_down_alpha: f64,
    /// Stable period (ms) before allowing ramp-down (default: 2000).
    pub stability_threshold_ms: u64,
    /// Extra latency (ms) added at 100% loss rate (default: 500). Scaled linearly.
    pub loss_penalty_ms: f64,
}

#[cfg(test)]
impl ReassemblyConfig {
    /// Permissive defaults for unit tests: 10 ms start, 10 ms floor, 2000 ms
    /// ceiling. Lets tests exercise sub-second playout behaviour without the
    /// production 1000 ms floor clamping every assertion. Production code
    /// must use [`ReassemblyConfig::default`].
    pub(crate) fn test_defaults() -> Self {
        Self {
            start_latency: Duration::from_millis(10),
            min_latency_ms: 10,
            max_latency_ms: 2000,
            ..Self::default()
        }
    }
}

impl Default for ReassemblyConfig {
    fn default() -> Self {
        Self {
            // Bonded-cellular tail OWD (HARQ retries + per-link saturation
            // probe pinning) reaches 600-1500 ms. Adaptive playout used to
            // chase calm-period averages back down to ~500-700 ms and then
            // discard ~3 packets/sec as late, blowing H.265 reference-frame
            // chains. For HLS ingest use cases (YouTube etc.) latency is
            // free — segment duration dominates glass-to-glass anyway —
            // so we pin the baseline well above the tail.
            start_latency: Duration::from_millis(1500),
            buffer_capacity: 2048,
            skip_after: None,
            jitter_latency_multiplier: 4.0,
            max_latency_ms: 3000,
            min_latency_ms: 1000,
            ramp_up_alpha: 0.3,
            ramp_down_alpha: 0.05,
            stability_threshold_ms: 2000,
            loss_penalty_ms: 200.0,
        }
    }
}

/// Snapshot of per-link receive statistics for telemetry.
#[derive(Default, Clone, Debug)]
pub struct ReassemblyLinkStats {
    pub link_id: usize,
    pub packets_received: u64,
    pub packets_delivered: u64,
    pub loss_rate: f64,
}

/// Snapshot of reassembly buffer statistics for telemetry.
#[derive(Default, Clone, Debug)]
pub struct ReassemblyStats {
    pub queue_depth: usize,
    pub next_seq: u64,
    pub lost_packets: u64,
    pub late_packets: u64,
    pub duplicate_packets: u64,
    pub discontinuities: u64,
    pub current_latency_ms: u64,
    /// The computed ideal latency the buffer is tracking toward.
    pub target_latency_ms: u64,
    /// Current smoothed jitter estimate in milliseconds.
    pub jitter_estimate_ms: f64,
    /// Recent smoothed loss rate (0.0–1.0).
    pub loss_rate: f64,
    /// Packets successfully delivered.
    pub packets_delivered: u64,
    /// Per-link receive/delivery stats from transport readers.
    pub per_link: Vec<ReassemblyLinkStats>,
}

fn percentile(samples: &VecDeque<f64>, pct: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = samples.iter().copied().collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() - 1) as f64 * pct).round() as usize;
    v[idx.min(v.len() - 1)]
}

impl ReassemblyBuffer {
    pub fn new(start_seq: u64, latency: Duration) -> Self {
        Self::with_config(
            start_seq,
            ReassemblyConfig {
                start_latency: latency,
                ..ReassemblyConfig::default()
            },
        )
    }

    /// Test-only convenience constructor that uses permissive defaults
    /// (10 ms floor, 2 s ceiling) so tests can exercise sub-second playout
    /// behaviour without the production 1000 ms floor clamping assertions.
    #[cfg(test)]
    pub(crate) fn new_for_test(start_seq: u64, latency: Duration) -> Self {
        Self::with_config(
            start_seq,
            ReassemblyConfig {
                start_latency: latency,
                ..ReassemblyConfig::test_defaults()
            },
        )
    }

    pub fn with_config(start_seq: u64, config: ReassemblyConfig) -> Self {
        let capacity = config.buffer_capacity.max(16);
        Self {
            buffer: (0..capacity).map(|_| None).collect(),
            capacity,
            buffered: 0,
            next_seq: start_seq,
            latency: config.start_latency,
            start_latency: config.start_latency,
            skip_after: config.skip_after,
            jitter_latency_multiplier: config.jitter_latency_multiplier,
            max_latency: Duration::from_millis(config.max_latency_ms),
            min_latency: Duration::from_millis(config.min_latency_ms),
            lost_packets: 0,
            late_packets: 0,
            duplicate_packets: 0,
            discontinuities: 0,
            packets_delivered: 0,
            last_arrival: None,
            avg_iat: 0.0,
            jitter_smoothed: 0.0,
            jitter_samples: VecDeque::with_capacity(128),
            epoch: Instant::now(),
            rel_min_deque: VecDeque::new(),
            rel_max_deque: VecDeque::new(),
            delay_spread_us: 0,
            target_latency: config.start_latency,
            ramp_up_alpha: config.ramp_up_alpha,
            ramp_down_alpha: config.ramp_down_alpha,
            stable_since: None,
            stability_threshold: Duration::from_millis(config.stability_threshold_ms),
            loss_rate_smoothed: 0.0,
            loss_penalty_ms: config.loss_penalty_ms,
            late_pressure_ms: 0.0,
            last_late_arrival: None,
            consecutive_late: 0,
            max_late_seq: 0,
            last_emitted_seq: None,
        }
    }

    pub fn get_stats(&self) -> ReassemblyStats {
        ReassemblyStats {
            queue_depth: self.buffered,
            next_seq: self.next_seq,
            lost_packets: self.lost_packets,
            late_packets: self.late_packets,
            duplicate_packets: self.duplicate_packets,
            discontinuities: self.discontinuities,
            current_latency_ms: self.latency.as_millis() as u64,
            target_latency_ms: self.target_latency.as_millis() as u64,
            jitter_estimate_ms: self.jitter_smoothed * 1000.0,
            loss_rate: self.loss_rate_smoothed,
            packets_delivered: self.packets_delivered,
            per_link: Vec::new(),
        }
    }

    /// Test-only / legacy entry point. Synthesises a `send_ts_us` that
    /// tracks the arrival clock so `rel = arrival − send_ts` stays
    /// constant across pushes — the delay-spread component collapses to
    /// zero and the dynamic component falls back to inter-arrival jitter,
    /// preserving pre-`push_with_ts` behaviour for unit tests.
    pub fn push(&mut self, seq_id: u64, payload: Bytes, now: Instant) {
        let synthetic_ts = now.saturating_duration_since(self.epoch).as_micros() as u32;
        self.push_with_ts(seq_id, payload, now, synthetic_ts);
    }

    /// Production entry point. `send_ts_us` is the sender's wire send-time;
    /// used to size the playout window from the bonded inter-link delay
    /// spread (the signal that actually governs lateness on heterogeneous
    /// bonded links, unlike naive inter-arrival jitter).
    pub fn push_with_ts(&mut self, seq_id: u64, payload: Bytes, now: Instant, send_ts_us: u32) {
        // Bonded inter-link delay spread. `rel` = arrival (local µs since
        // epoch) − sender send-time. The absolute value is meaningless
        // (two unsynced clocks) but its spread over a short sliding window
        // IS the cross-link arrival skew the buffer must absorb: a packet
        // striped on the momentarily-slow modem has a large `rel`, one on
        // the fast modem a small `rel`, and (max − min) is the skew. u32
        // send-ts wraps ~71 min; the windowed diff bounds any wrap blip
        // and max_latency clamps the final window regardless.
        let arrival_us = now.saturating_duration_since(self.epoch).as_micros() as i64;
        let rel = arrival_us - send_ts_us as i64;
        const DELAY_SPREAD_WINDOW: Duration = Duration::from_secs(4);
        let cutoff = now.checked_sub(DELAY_SPREAD_WINDOW);
        // Sliding-window MIN via monotonic-increasing deque.
        while self.rel_min_deque.back().is_some_and(|&(_, v)| v >= rel) {
            self.rel_min_deque.pop_back();
        }
        self.rel_min_deque.push_back((now, rel));
        // Sliding-window MAX via monotonic-decreasing deque.
        while self.rel_max_deque.back().is_some_and(|&(_, v)| v <= rel) {
            self.rel_max_deque.pop_back();
        }
        self.rel_max_deque.push_back((now, rel));
        if let Some(cut) = cutoff {
            while self.rel_min_deque.front().is_some_and(|&(t, _)| t < cut) {
                self.rel_min_deque.pop_front();
            }
            while self.rel_max_deque.front().is_some_and(|&(t, _)| t < cut) {
                self.rel_max_deque.pop_front();
            }
        }
        let rel_min = self.rel_min_deque.front().map(|&(_, v)| v).unwrap_or(rel);
        let rel_max = self.rel_max_deque.front().map(|&(_, v)| v).unwrap_or(rel);
        self.delay_spread_us = (rel_max - rel_min).max(0);

        // Calculate Jitter
        if let Some(last) = self.last_arrival {
            let iat = now.duration_since(last).as_secs_f64();

            // EWMA alpha
            let alpha = 0.1;

            // Update average inter-arrival time
            self.avg_iat = (1.0 - alpha) * self.avg_iat + alpha * iat;

            // Calculate instantaneous jitter
            let jitter = (iat - self.avg_iat).abs();

            // Smooth jitter
            self.jitter_smoothed = (1.0 - alpha) * self.jitter_smoothed + alpha * jitter;
            self.jitter_samples.push_back(jitter);
            if self.jitter_samples.len() > 128 {
                self.jitter_samples.pop_front();
            }

            // Compute jitter component of target latency
            let jitter_est = if self.jitter_samples.len() >= 5 {
                percentile(&self.jitter_samples, 0.95)
            } else {
                self.jitter_smoothed
            };
            let jitter_ms = jitter_est * 1000.0;
            let jitter_component = self.jitter_latency_multiplier * jitter_ms;

            // Bonded inter-link delay-spread component. This is the signal
            // that actually governs lateness on heterogeneous bonded links:
            // the buffer must hold ≥ the cross-link arrival skew or a
            // slow-modem packet is declared "late" and dropped before it
            // can physically arrive (field: spread 250–400 ms during a
            // one-modem fade while inter-arrival jitter read ~19 ms — the
            // old sole input was structurally blind to this). 1.15× covers
            // sampling lag in the windowed max.
            let spread_component = (self.delay_spread_us as f64 / 1000.0) * 1.15;

            // Loss-aware component: more buffer when losing packets
            let loss_component = self.loss_rate_smoothed * self.loss_penalty_ms;

            // Closed-loop late-pressure is now a *secondary trim* on top of
            // the spread floor, so the drain must be genuinely slow:
            // fast-open (≈6 ms per late hit) / slow-close (≈8 ms per 500 ms
            // stable) is real AIMD. The old 40 ms/500 ms drain collapsed
            // the window faster than skew bursts recurred → the observed
            // 312↔933 ms oscillation that re-created the very lateness it
            // was reacting to.
            const STABLE_DRAIN_MS: u128 = 500;
            const DRAIN_STEP_MS: f64 = 8.0;
            if let Some(last_late) = self.last_late_arrival
                && now.duration_since(last_late).as_millis() >= STABLE_DRAIN_MS
                && self.late_pressure_ms > 0.0
            {
                self.late_pressure_ms = (self.late_pressure_ms - DRAIN_STEP_MS).max(0.0);
                // Reset the drain clock so we drain at most DRAIN_STEP_MS per
                // STABLE_DRAIN_MS window, not per push.
                self.last_late_arrival = Some(now);
            }

            // The dynamic component is the MAX of inter-arrival jitter and
            // the bonded delay spread — never let the window sit below the
            // measured cross-link skew (the floor), while still honouring
            // single-link jitter when it is the larger effect.
            let dynamic_component = jitter_component.max(spread_component);

            // Compute target latency: formula gives the floor, late-pressure
            // (closed-loop) trims around it.
            let target_ms = self.start_latency.as_millis() as f64
                + dynamic_component
                + loss_component
                + self.late_pressure_ms;
            self.target_latency = Duration::from_millis(target_ms as u64)
                .max(self.min_latency)
                .min(self.max_latency);

            // Bidirectional smoothing: fast up, slow down
            let current_ms = self.latency.as_secs_f64() * 1000.0;
            let target_ms = self.target_latency.as_secs_f64() * 1000.0;

            if target_ms > current_ms + 0.5 {
                // Fast ramp-up
                let new_ms = current_ms + self.ramp_up_alpha * (target_ms - current_ms);
                self.latency = Duration::from_secs_f64(new_ms / 1000.0);
                self.stable_since = None;
            } else if target_ms < current_ms - 0.5 {
                // Fast ramp-down when target is dramatically lower (stall
                // recovery: loss_rate dropped → loss_penalty shrank).  Use
                // the same ramp-up alpha to avoid being stuck at a bloated
                // latency for seconds after the underlying issue resolved.
                if current_ms > target_ms * 2.0 {
                    let new_ms = current_ms + self.ramp_up_alpha * (target_ms - current_ms);
                    self.latency = Duration::from_secs_f64(new_ms / 1000.0).max(self.min_latency);
                    self.stable_since = None;
                } else {
                    // Normal slow ramp-down, only after stability period
                    match self.stable_since {
                        Some(since) if now.duration_since(since) >= self.stability_threshold => {
                            let new_ms =
                                current_ms + self.ramp_down_alpha * (target_ms - current_ms);
                            self.latency =
                                Duration::from_secs_f64(new_ms / 1000.0).max(self.min_latency);
                        }
                        None => {
                            self.stable_since = Some(now);
                        }
                        _ => {} // Waiting for stability threshold
                    }
                }
            }
        }
        self.last_arrival = Some(now);

        if seq_id < self.next_seq {
            self.consecutive_late += 1;
            // Track the highest seq_id seen among consecutive late packets.
            // This is the resync target: the most recent position the sender
            // was at, not an arbitrary old packet that happened to arrive last.
            if seq_id > self.max_late_seq {
                self.max_late_seq = seq_id;
            }

            // If we see many consecutive late packets, the buffer's next_seq
            // has desynchronised from the sender (e.g. after a burst loss
            // caused a large gap-skip).  Reset to re-sync with the sender.
            const RESYNC_THRESHOLD: u64 = 100;
            if self.consecutive_late >= RESYNC_THRESHOLD {
                // Use the highest seq_id seen in this window as the resync
                // target — it's the best approximation of the sender's current
                // position.  Using `seq_id` (the last, possibly very old
                // retransmission) would reset next_seq to 0 or some stale
                // value and permanently stall the receiver.
                let resync_target = self.max_late_seq + 1;
                // Never rewind across already-emitted sequences. Re-emitting
                // a seq we've already delivered makes the downstream MPEG-TS
                // demuxer see regressing PTS/continuity counters and post a
                // fatal "Timestamping error on input streams" — killing the
                // pipeline. Rewinding is only safe when the target is strictly
                // greater than the highest seq we've actually emitted; i.e.
                // when the gap between old next_seq and target consists purely
                // of gap-skipped (never-emitted) sequences.
                let emit_watermark = self.last_emitted_seq.map(|s| s + 1).unwrap_or(0);
                if resync_target < emit_watermark {
                    self.consecutive_late = 0;
                    self.max_late_seq = 0;
                    const LATE_HIT_MS: f64 = 6.0;
                    let base_headroom_ms = self
                        .max_latency
                        .as_millis()
                        .saturating_sub(self.start_latency.as_millis())
                        as f64;
                    let max_pressure = base_headroom_ms.min(600.0);
                    self.late_pressure_ms = (self.late_pressure_ms + LATE_HIT_MS).min(max_pressure);
                    self.last_late_arrival = Some(now);
                    self.late_packets += 1;
                    return;
                }
                tracing::warn!(
                    old_next_seq = self.next_seq,
                    new_next_seq = resync_target,
                    consecutive_late = self.consecutive_late,
                    "reassembly buffer desync detected — resetting next_seq to re-sync with sender"
                );
                // Clear stale buffer contents below the resync target
                for slot in self.buffer.iter_mut() {
                    if let Some(p) = slot
                        && p.seq_id < resync_target
                    {
                        *slot = None;
                        self.buffered = self.buffered.saturating_sub(1);
                    }
                }
                self.next_seq = resync_target;
                self.consecutive_late = 0;
                self.max_late_seq = 0;
                // Reset latency state but preserve loss EWMA. Hard-resetting
                // loss_rate_smoothed here masks real loss in telemetry and
                // produces impossible near-zero loss estimates during churn.
                self.latency = self.start_latency;
                self.target_latency = self.start_latency;
                self.stable_since = None;
                // Fall through to insert this packet normally
            } else {
                // Late packet, drop.  Bump late_pressure — direct evidence
                // our deadline was too tight.  Additive-increase step is
                // larger than the drain step so a few late hits quickly
                // open the window; drain is slow so we don't oscillate.
                const LATE_HIT_MS: f64 = 3.0;
                let base_headroom_ms =
                    self.max_latency
                        .as_millis()
                        .saturating_sub(self.start_latency.as_millis()) as f64;
                let max_pressure = base_headroom_ms.min(600.0);
                self.late_pressure_ms = (self.late_pressure_ms + LATE_HIT_MS).min(max_pressure);
                self.last_late_arrival = Some(now);
                self.late_packets += 1;
                return;
            }
        } else {
            self.consecutive_late = 0;
            self.max_late_seq = 0;
        }

        let capacity = self.capacity as u64;
        if seq_id >= self.next_seq + capacity {
            let new_next = seq_id.saturating_sub(capacity.saturating_sub(1));
            if new_next > self.next_seq {
                let skipped = new_next - self.next_seq;
                self.lost_packets += skipped;
                self.advance_window(new_next);
            }
        }

        let idx = self.buffer_index(seq_id);
        if let Some(existing) = &self.buffer[idx] {
            if existing.seq_id == seq_id {
                // Duplicate packet (same seq_id arrived again)
                self.duplicate_packets += 1;
                return; // Don't overwrite
            } else if existing.seq_id >= self.next_seq {
                // Different packet in this slot, was lost
                self.lost_packets += 1;
            }
        } else {
            self.buffered += 1;
        }

        self.buffer[idx] = Some(Packet {
            seq_id,
            payload,
            arrival_time: now,
            send_ts_us,
        });
    }

    /// Release ready packets. Returns `(payload, discont)` pairs where
    /// `discont = true` means a gap was skipped immediately before this
    /// packet (the MPEG-TS byte-alignment may have shifted).
    pub fn tick(&mut self, now: Instant) -> Vec<(Bytes, bool)> {
        let loss_before = self.lost_packets;
        let mut released = Vec::new();
        let skip_after = self.skip_after.unwrap_or(self.latency);
        let release_after = self
            .skip_after
            .map(|v| v.min(self.latency))
            .unwrap_or(self.latency);

        // Set after a gap skip; cleared after the next packet is released.
        let mut discont = false;

        // While loop to process available packets or skip gaps
        loop {
            // Case 1: We have the next packet
            let idx = self.buffer_index(self.next_seq);
            if let Some(packet) = &self.buffer[idx]
                && packet.seq_id == self.next_seq
            {
                // Check if it has satisfied the latency requirement
                if now.duration_since(packet.arrival_time) >= release_after {
                    let p = self.buffer[idx].take().unwrap();
                    self.buffered = self.buffered.saturating_sub(1);
                    let flagged_discont = std::mem::take(&mut discont);
                    if flagged_discont {
                        self.discontinuities += 1;
                    }
                    released.push((p.payload, flagged_discont));
                    self.last_emitted_seq = Some(self.next_seq);
                    self.next_seq += 1;
                    continue;
                }
                // Not ready yet
                break;
            }

            // Case 2: We have a gap (missing next_seq)
            if let Some((first_seq, first_arrival)) = self.find_next_available()
                && now.duration_since(first_arrival) >= skip_after
            {
                let skipped = first_seq.saturating_sub(self.next_seq);
                self.lost_packets += skipped;
                self.advance_window(first_seq);
                discont = true;
                continue;
            }

            // No packets or waiting for gap to fill
            break;
        }

        // Track delivery + loss for adaptive sizing
        self.packets_delivered += released.len() as u64;
        let new_losses = self.lost_packets - loss_before;
        let total_events = released.len() as u64 + new_losses;
        if total_events > 0 {
            let instant_loss = new_losses as f64 / total_events as f64;
            self.loss_rate_smoothed = 0.95 * self.loss_rate_smoothed + 0.05 * instant_loss;
        }
        if new_losses > 0 {
            self.stable_since = None;
        }

        released
    }

    fn buffer_index(&self, seq_id: u64) -> usize {
        (seq_id % self.capacity as u64) as usize
    }

    fn advance_window(&mut self, new_next: u64) {
        let old_next = self.next_seq;
        if new_next <= old_next {
            return;
        }
        for seq in old_next..new_next {
            let idx = self.buffer_index(seq);
            if let Some(packet) = &self.buffer[idx]
                && packet.seq_id == seq
            {
                self.buffer[idx] = None;
                self.buffered = self.buffered.saturating_sub(1);
            }
        }
        self.next_seq = new_next;
    }

    fn find_next_available(&self) -> Option<(u64, Instant)> {
        let mut best: Option<(u64, Instant)> = None;
        for slot in self.buffer.iter().flatten() {
            if slot.seq_id <= self.next_seq {
                continue;
            }
            match best {
                Some((best_seq, _)) if slot.seq_id >= best_seq => {}
                _ => {
                    best = Some((slot.seq_id, slot.arrival_time));
                }
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_order_delivery() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(100));
        let start = Instant::now();
        let p1 = Bytes::from_static(b"P1");

        buf.push(0, p1.clone(), start);

        // Immediate tick - should not release (latency 100ms)
        let out = buf.tick(start);
        assert!(out.is_empty());

        // Tick after latency
        let out = buf.tick(start + Duration::from_millis(100));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, p1);
    }

    #[test]
    fn test_reordering() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(50));
        let start = Instant::now();

        // Arrives: Seq 2, then Seq 0, then Seq 1
        buf.push(2, Bytes::from_static(b"P2"), start);
        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(1, Bytes::from_static(b"P1"), start);

        // Wait for latency
        let out = buf.tick(start + Duration::from_millis(50));

        // Should come out as P0, P1, P2
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, Bytes::from_static(b"P0"));
        assert_eq!(out[1].0, Bytes::from_static(b"P1"));
        assert_eq!(out[2].0, Bytes::from_static(b"P2"));
    }

    #[test]
    fn test_gap_skipping() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(50));
        let start = Instant::now();

        // P0 missing
        // P1 arrives
        buf.push(1, Bytes::from_static(b"P1"), start);

        // Tick at 50ms. P1 is ready, but P0 is missing.
        // P1 arrived at `start`. It has waited 50ms.
        // The logic should say: P1 has expired latency. So we define next_seq = 1.

        let out = buf.tick(start + Duration::from_millis(50));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Bytes::from_static(b"P1"));
    }

    #[test]
    fn test_adaptive_latency() {
        // Base latency 10ms
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(10));
        let start = Instant::now();

        // Push packets with jitter
        // P0 at 0ms
        buf.push(0, Bytes::from_static(b"P0"), start);
        assert_eq!(buf.latency.as_millis(), 10); // First packet, no jitter calc yet

        // P1 at 20ms (IAT 20ms). Avg IAT will move towards 20ms.
        buf.push(
            1,
            Bytes::from_static(b"P1"),
            start + Duration::from_millis(20),
        );

        // P2 at 30ms (IAT 10ms).
        // Jitter introduced.
        buf.push(
            2,
            Bytes::from_static(b"P2"),
            start + Duration::from_millis(30),
        );

        // P3 at 60ms (IAT 30ms).
        buf.push(
            3,
            Bytes::from_static(b"P3"),
            start + Duration::from_millis(60),
        );

        // The latency should have increased from 10ms due to jitter
        let current_latency = buf.latency.as_millis();
        assert!(
            current_latency > 10,
            "Latency should increase due to jitter (current: {})",
            current_latency
        );

        // Check stats
        let stats = buf.get_stats();
        assert_eq!(stats.current_latency_ms, current_latency as u64);
    }

    #[test]
    fn test_percentile_basic() {
        let mut samples = VecDeque::new();
        samples.push_back(1.0);
        samples.push_back(2.0);
        samples.push_back(3.0);
        samples.push_back(100.0);

        let p50 = percentile(&samples, 0.5);
        let p95 = percentile(&samples, 0.95);

        assert_eq!(p50, 3.0);
        assert_eq!(p95, 100.0);
    }

    #[test]
    fn test_aggressive_skip_policy() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(100),
            buffer_capacity: 64,
            skip_after: Some(Duration::from_millis(30)),
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Missing seq 0, seq 1 arrives
        buf.push(1, Bytes::from_static(b"P1"), start);

        // At 30ms, aggressive skip should release P1
        let out = buf.tick(start + Duration::from_millis(30));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Bytes::from_static(b"P1"));
    }

    #[test]
    fn test_far_ahead_packet_advances_window() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 8,
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Push far ahead packet to force window advance
        buf.push(20, Bytes::from_static(b"P20"), start);
        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, Bytes::from_static(b"P20"));
        assert!(buf.lost_packets > 0);
    }

    #[test]
    fn test_duplicate_packet_counting() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(100));
        let start = Instant::now();

        // Push packet with seq_id 0
        buf.push(0, Bytes::from_static(b"P0-original"), start);
        assert_eq!(buf.duplicate_packets, 0);

        // Push same seq_id again (duplicate)
        buf.push(0, Bytes::from_static(b"P0-duplicate"), start);
        assert_eq!(buf.duplicate_packets, 1);

        // Push another different packet
        buf.push(1, Bytes::from_static(b"P1"), start);
        assert_eq!(buf.duplicate_packets, 1); // Still 1

        // Push duplicate of seq_id 1
        buf.push(1, Bytes::from_static(b"P1-duplicate"), start);
        assert_eq!(buf.duplicate_packets, 2);

        // Verify stats expose duplicate count
        let stats = buf.get_stats();
        assert_eq!(stats.duplicate_packets, 2);
    }

    #[test]
    fn test_duplicate_vs_late_packets() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(100));
        let start = Instant::now();

        // Push packet 0 and 1
        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(1, Bytes::from_static(b"P1"), start);

        // Release them
        let out = buf.tick(start + Duration::from_millis(100));
        assert_eq!(out.len(), 2);

        // Now push seq_id 0 again - this is LATE, not duplicate
        // (because next_seq has advanced past it)
        buf.push(
            0,
            Bytes::from_static(b"P0-late"),
            start + Duration::from_millis(120),
        );

        assert_eq!(buf.late_packets, 1);
        assert_eq!(buf.duplicate_packets, 0); // Not counted as duplicate
    }

    #[test]
    fn test_latency_max_capping() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            jitter_latency_multiplier: 100.0,
            max_latency_ms: 200,
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(
            1,
            Bytes::from_static(b"P1"),
            start + Duration::from_millis(1),
        );
        buf.push(
            2,
            Bytes::from_static(b"P2"),
            start + Duration::from_millis(100),
        );
        buf.push(
            3,
            Bytes::from_static(b"P3"),
            start + Duration::from_millis(101),
        );
        buf.push(
            4,
            Bytes::from_static(b"P4"),
            start + Duration::from_millis(300),
        );

        assert!(
            buf.latency <= Duration::from_millis(200),
            "Latency should be capped at max_latency_ms (200ms), got: {:?}",
            buf.latency
        );
    }

    #[test]
    fn test_buffer_capacity_boundary() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 16,
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        for i in 0..16u64 {
            buf.push(i, Bytes::from(format!("P{}", i)), start);
        }
        assert_eq!(buf.buffered, 16);

        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), 16);
    }

    #[test]
    fn test_stats_during_operation() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(50));
        let start = Instant::now();

        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(1, Bytes::from_static(b"P1"), start);

        let stats = buf.get_stats();
        assert_eq!(stats.queue_depth, 2);
        assert_eq!(stats.next_seq, 0);
        assert_eq!(stats.lost_packets, 0);

        let _ = buf.tick(start + Duration::from_millis(50));
        let stats = buf.get_stats();
        assert_eq!(stats.queue_depth, 0);
        assert_eq!(stats.next_seq, 2);
    }

    #[test]
    fn test_percentile_single_sample() {
        let mut samples = VecDeque::new();
        samples.push_back(5.0);
        assert_eq!(percentile(&samples, 0.5), 5.0);
        assert_eq!(percentile(&samples, 0.95), 5.0);
    }

    #[test]
    fn test_percentile_empty() {
        let samples = VecDeque::new();
        assert_eq!(percentile(&samples, 0.5), 0.0);
    }

    #[test]
    fn test_many_packets_in_order() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(10));
        let start = Instant::now();

        for i in 0..1000u64 {
            buf.push(i, Bytes::from(vec![i as u8; 100]), start);
        }

        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), 1000);
        assert_eq!(buf.lost_packets, 0);
        assert_eq!(buf.duplicate_packets, 0);
    }

    // ─── Dynamic Jitter Buffer Tests ────────────────────────────────────

    #[test]
    fn test_dynamic_ramp_down() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            stability_threshold_ms: 0, // Immediate ramp-down for testing
            ramp_down_alpha: 0.5,
            ramp_up_alpha: 1.0, // Instant ramp-up
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Phase 1: heavy jitter (alternating fast/slow arrivals)
        buf.push(0, Bytes::from(vec![0; 100]), start);
        buf.push(
            1,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(5),
        );
        buf.push(
            2,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(55),
        );
        buf.push(
            3,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(60),
        );
        buf.push(
            4,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(110),
        );
        buf.push(
            5,
            Bytes::from(vec![0; 100]),
            start + Duration::from_millis(115),
        );

        let high_latency = buf.latency;
        assert!(
            high_latency > Duration::from_millis(15),
            "Latency should increase from jitter: {:?}",
            high_latency
        );

        // Phase 2: steady arrivals (150+ pushes to flush jitter window)
        for i in 6u64..200 {
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(120 + (i - 6) * 10),
            );
        }

        let lower_latency = buf.latency;
        assert!(
            lower_latency < high_latency,
            "Latency should ramp down with stable conditions: high={:?}, low={:?}",
            high_latency,
            lower_latency
        );
    }

    /// Regression: after a stall inflates latency via loss_penalty, clearing
    /// the loss must ramp latency back down quickly (using ramp_up_alpha, not
    /// the slow ramp_down_alpha) when current_ms > target_ms * 2.0.
    ///
    /// Before the fix the slow path was always taken, leaving latency stuck at
    /// 200+ ms for many seconds after loss cleared — causing A/V sync issues
    /// and head-of-line blocking on recovered links.
    #[test]
    fn stall_recovery_ramp_down_fast() {
        // ramp_up_alpha=1.0 → instant ramp-up; ramp_down_alpha=0.02 (slow default)
        // Without the fast-ramp-down path, after 5 push() calls latency would
        // still be ~200ms when loss clears; with it, it should be ≤ start_latency.
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(20),
            buffer_capacity: 256,
            skip_after: Some(Duration::from_millis(5)),
            ramp_up_alpha: 1.0,
            ramp_down_alpha: 0.02, // very slow — without the fast path we'd stay high
            loss_penalty_ms: 500.0,
            stability_threshold_ms: 0, // no stability wait
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Phase 1: push 50 packets with a gap to drive loss_rate_smoothed up,
        // then tick to register the losses.
        for i in 0u64..50 {
            buf.push(
                i,
                Bytes::from(vec![i as u8]),
                start + Duration::from_millis(i),
            );
        }
        // Create a big gap — skip to seq 200 so ~150 packets appear lost
        let t_gap = start + Duration::from_millis(100);
        buf.push(200, Bytes::from_static(b"jump"), t_gap);
        let _ = buf.tick(t_gap + Duration::from_millis(10));

        // Manually inflate loss_rate_smoothed and latency to worst-case
        buf.loss_rate_smoothed = 1.0;
        buf.latency = Duration::from_millis(500);
        buf.target_latency = Duration::from_millis(500);

        let bloated_latency = buf.latency;

        // Phase 2: loss clears — push steady in-order packets so the buffer
        // computes a low target (start_latency + 0 loss_penalty = 20ms).
        // current_ms(500) > target_ms(20) * 2 → fast ramp-down path fires.
        let t_clear = t_gap + Duration::from_millis(200);
        buf.loss_rate_smoothed = 0.0; // loss cleared
        for i in 0u64..20 {
            buf.push(
                201 + i,
                Bytes::from(vec![i as u8]),
                t_clear + Duration::from_millis(i * 10),
            );
        }

        assert!(
            buf.latency < bloated_latency / 2,
            "fast ramp-down should halve bloated latency quickly: still at {:?} (started at {:?})",
            buf.latency,
            bloated_latency,
        );
    }

    #[test]
    fn test_loss_increases_latency() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            skip_after: Some(Duration::from_millis(5)),
            ramp_up_alpha: 1.0, // Instant ramp-up
            loss_penalty_ms: 1000.0,
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Push seq 0 then skip seq 1, push seq 2-5
        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(
            2,
            Bytes::from_static(b"P2"),
            start + Duration::from_millis(1),
        );
        buf.push(
            3,
            Bytes::from_static(b"P3"),
            start + Duration::from_millis(2),
        );
        buf.push(
            4,
            Bytes::from_static(b"P4"),
            start + Duration::from_millis(3),
        );
        buf.push(
            5,
            Bytes::from_static(b"P5"),
            start + Duration::from_millis(4),
        );

        // Tick to skip gap (seq 1 missing, skip_after=5ms)
        let _ = buf.tick(start + Duration::from_millis(20));
        assert!(buf.lost_packets > 0, "Should have recorded a loss");
        assert!(buf.loss_rate_smoothed > 0.0, "Loss rate should be non-zero");

        // Push more packets — latency should incorporate loss penalty
        for i in 6..10 {
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(20 + (i - 6) * 10),
            );
        }

        let stats = buf.get_stats();
        assert!(
            stats.loss_rate > 0.0,
            "Stats should report non-zero loss rate"
        );
    }

    #[test]
    fn test_min_latency_floor() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(5),
            min_latency_ms: 20,
            ramp_up_alpha: 1.0,
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Push steady packets — target = max(20, 5 + jitter) = 20 when jitter is small
        for i in 0..10 {
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(i * 10),
            );
        }

        assert!(
            buf.latency >= Duration::from_millis(20),
            "Latency should not go below min_latency (20ms): {:?}",
            buf.latency
        );
    }

    #[test]
    fn test_stats_target_and_jitter() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(10));
        let start = Instant::now();

        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(
            1,
            Bytes::from_static(b"P1"),
            start + Duration::from_millis(20),
        );
        buf.push(
            2,
            Bytes::from_static(b"P2"),
            start + Duration::from_millis(30),
        );

        let stats = buf.get_stats();
        assert!(stats.target_latency_ms >= 10);
        assert!(stats.jitter_estimate_ms >= 0.0);
    }

    #[test]
    fn test_delivered_packets_counted() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(10));
        let start = Instant::now();

        for i in 0..5u64 {
            buf.push(i, Bytes::from(vec![0; 100]), start);
        }

        let out = buf.tick(start + Duration::from_millis(10));
        assert_eq!(out.len(), 5);
        assert_eq!(buf.packets_delivered, 5);

        let stats = buf.get_stats();
        assert_eq!(stats.packets_delivered, 5);
    }

    // ── Regression: snag #14 — max_latency_ms must be wired through ──

    /// A custom max_latency_ms should actually change the ceiling.
    /// Before the fix, the default (500ms) was always used regardless
    /// of the config value.
    #[test]
    fn test_max_latency_ms_wired_from_config() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(50),
            max_latency_ms: 3000,
            ..ReassemblyConfig::test_defaults()
        };
        let buf = ReassemblyBuffer::with_config(0, config);
        assert_eq!(
            buf.max_latency,
            Duration::from_millis(3000),
            "max_latency should be set from config, not hardcoded to 500"
        );
    }

    /// Packets arriving within max_latency should NOT be counted as late
    /// when max_latency is raised above the default 500ms.
    #[test]
    fn test_high_max_latency_accepts_slow_packets() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(500),
            max_latency_ms: 3000,
            // Fast ramp-up so latency reaches ceiling quickly
            ramp_up_alpha: 1.0,
            jitter_latency_multiplier: 4.0,
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Simulate high-jitter LTE: packets arrive with 800ms IAT variation
        for i in 0..20u64 {
            let jitter = if i % 3 == 0 { 800 } else { 10 };
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(i * 50 + jitter),
            );
        }

        // Tick well past the arrival window
        let _ = buf.tick(start + Duration::from_millis(5000));

        let stats = buf.get_stats();
        // With max_latency=3000ms, the buffer should have absorbed the
        // jitter without classifying packets as late/lost
        assert!(
            stats.late_packets < 5,
            "With 3000ms ceiling, most packets should be accepted (got {} late)",
            stats.late_packets
        );
    }

    /// Regression guard pinning the current default ceiling. If someone
    /// lowers `max_latency_ms` below the bonded-cellular tail OWD again,
    /// this test will fail loudly.
    #[test]
    fn test_default_max_latency_is_3000ms() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(50),
            // max_latency_ms: 500 (default)
            ramp_up_alpha: 1.0,
            jitter_latency_multiplier: 4.0,
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Same high-jitter pattern as above
        for i in 0..20u64 {
            let jitter = if i % 3 == 0 { 800 } else { 10 };
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(i * 50 + jitter),
            );
        }

        let _ = buf.tick(start + Duration::from_millis(5000));
        let _stats = buf.get_stats();

        // Confirm the default ceiling is 3000ms (regression guard).
        // Raised from 500 → 2000 → 3000 ms across iterations: HLS tolerates
        // generous headroom, and tight ceilings forced packet drops during
        // retransmit-driven recovery and bonded-cellular tail-OWD events.
        assert_eq!(
            ReassemblyConfig::default().max_latency_ms,
            3000,
            "Default max_latency_ms should be 3000"
        );
    }

    // ── Regression: resync resets adaptive latency state ─────────────

    /// After the desync reset (100 consecutive late packets), the buffer's
    /// adaptive latency state should be fully cleared so that new packets
    /// arriving after the reset are not immediately classified as "late".
    ///
    /// Before the fix, loss_rate_smoothed stayed at 1.0, latency stayed at
    /// max (500ms), and target_latency stayed at max — so every packet that
    /// arrived after the resync was also classified as late, triggering
    /// another resync → infinite loop.
    #[test]
    fn resync_resets_adaptive_latency() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(20),
            buffer_capacity: 64,
            skip_after: Some(Duration::from_millis(5)),
            // Fast ramp-up so loss penalty inflates latency quickly
            ramp_up_alpha: 1.0,
            loss_penalty_ms: 500.0,
            max_latency_ms: 500,
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Phase 1: Normal delivery of packets 0..10
        for i in 0u64..10 {
            buf.push(i, Bytes::from(vec![i as u8]), start);
        }
        let _ = buf.tick(start + Duration::from_millis(20));
        assert_eq!(buf.packets_delivered, 10);

        // Phase 2: Advance next_seq far ahead via overflow without emitting
        // the far-ahead packet downstream (no tick). This mirrors the real
        // desync scenario where gap-skip leaves next_seq past the sender but
        // nothing above the old next_seq has been emitted yet.
        let t2 = start + Duration::from_millis(100);
        buf.push(200, Bytes::from_static(b"far"), t2);

        // Manually set latency to max to simulate the worst case
        buf.latency = Duration::from_millis(500);
        buf.target_latency = Duration::from_millis(500);
        buf.loss_rate_smoothed = 1.0;

        // Phase 3: Send 100 consecutive "late" packets (seq < next_seq)
        // to trigger the desync reset.
        let t3 = t2 + Duration::from_millis(200);
        // All pushed seqs must remain strictly below next_seq (which is at
        // ~137 after the overflow advance) to register as consecutive late.
        let resume_seq = 20u64;
        for i in 0u64..100 {
            buf.push(
                resume_seq + i,
                Bytes::from(vec![(resume_seq + i) as u8]),
                t3 + Duration::from_millis(i),
            );
        }

        // The resync should have fired. Verify latency state was reset
        // but loss EWMA was preserved.
        assert!(
            buf.loss_rate_smoothed > 0.0,
            "loss_rate_smoothed must not be clobbered on resync"
        );
        assert_eq!(
            buf.latency,
            Duration::from_millis(20),
            "latency must revert to start_latency on resync"
        );
        assert_eq!(
            buf.target_latency,
            Duration::from_millis(20),
            "target_latency must revert to start_latency on resync"
        );
        assert!(
            buf.stable_since.is_none(),
            "stable_since must be cleared on resync"
        );

        // Phase 4: Verify that packets arriving after the resync are delivered,
        // not immediately dropped as "late" again.
        let t4 = t3 + Duration::from_millis(200);
        let post_resync_start = buf.next_seq;
        for i in 0u64..10 {
            buf.push(
                post_resync_start + i,
                Bytes::from(vec![i as u8]),
                t4 + Duration::from_millis(i * 5),
            );
        }
        let out = buf.tick(t4 + Duration::from_millis(100));
        assert!(
            !out.is_empty(),
            "packets after resync must be delivered normally (got {} late, {} delivered)",
            buf.late_packets,
            buf.packets_delivered
        );
    }

    #[test]
    fn resync_does_not_clobber_loss_ewma() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(20),
            buffer_capacity: 64,
            skip_after: Some(Duration::from_millis(5)),
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Build baseline history and induce meaningful loss EWMA.
        for i in 0u64..10 {
            buf.push(i, Bytes::from(vec![i as u8]), start);
        }
        let _ = buf.tick(start + Duration::from_millis(20));

        let t1 = start + Duration::from_millis(80);
        buf.push(200, Bytes::from_static(b"far"), t1);
        let _ = buf.tick(t1 + Duration::from_millis(10));
        buf.loss_rate_smoothed = buf.loss_rate_smoothed.max(0.4);
        let before_resync = buf.loss_rate_smoothed;

        // Trigger desync resync path with 100 consecutive late packets.
        let t2 = t1 + Duration::from_millis(100);
        for i in 0u64..100 {
            buf.push(
                20 + i,
                Bytes::from(vec![i as u8]),
                t2 + Duration::from_millis(i),
            );
        }

        assert!(
            buf.loss_rate_smoothed >= before_resync * 0.9,
            "resync should preserve loss EWMA: before {:.3}, after {:.3}",
            before_resync,
            buf.loss_rate_smoothed
        );

        // A clean interval should decay EWMA gradually, not collapse it.
        let t3 = t2 + Duration::from_millis(250);
        let seq = buf.next_seq;
        buf.push(seq, Bytes::from_static(b"ok"), t3);
        let _ = buf.tick(t3 + Duration::from_millis(30));
        assert!(
            buf.loss_rate_smoothed > 0.1,
            "loss EWMA should decay gradually after resync, got {:.6}",
            buf.loss_rate_smoothed
        );
    }

    /// Regression for field-test receiver death: the desync detector used to
    /// rewind `next_seq` below the last emitted sequence. Downstream
    /// `tsdemux` then saw a PTS/continuity regression and posted a fatal
    /// "Timestamping error on input streams", killing the receiver ~40s in
    /// and never recovering. The fix: only allow resync when the target is
    /// strictly above the last emitted seq; otherwise drop late packets.
    #[test]
    fn resync_never_rewinds_across_emitted_seqs() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 128,
            skip_after: Some(Duration::from_millis(5)),
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Deliver a long stream so last_emitted_seq is well above any late arrival.
        for i in 0u64..200 {
            buf.push(i, Bytes::from(vec![i as u8]), start);
        }
        let _ = buf.tick(start + Duration::from_millis(20));
        let emitted_before = buf.packets_delivered;
        let next_seq_before = buf.next_seq;
        assert!(next_seq_before >= 200);

        // 100 genuinely late packets (old retransmissions) whose max seq is
        // BELOW next_seq. The old code would rewind here — the new code must
        // drop them and keep next_seq pinned forward.
        let late_time = start + Duration::from_millis(500);
        for i in 0u64..100 {
            buf.push(10 + i, Bytes::from(vec![i as u8]), late_time);
        }

        assert_eq!(
            buf.next_seq, next_seq_before,
            "next_seq must not rewind when the late window sits below emitted range"
        );
        assert_eq!(
            buf.packets_delivered, emitted_before,
            "no spurious re-delivery after rejected resync"
        );
        assert!(buf.late_packets >= 100);
    }

    // ── Regression: desync recovery after burst loss ──────────────────

    /// After a large gap-skip pushes next_seq far ahead, subsequent packets
    /// from the sender all have seq < next_seq and are dropped as "late".
    /// The desync detector should reset next_seq after 100 consecutive late
    /// arrivals, allowing delivery to resume.
    #[test]
    fn test_desync_recovery_after_burst_loss() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 64,
            skip_after: Some(Duration::from_millis(5)),
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Phase 1: Normal delivery of packets 0..50
        for i in 0u64..50 {
            buf.push(i, Bytes::from(vec![i as u8]), start);
        }
        let _ = buf.tick(start + Duration::from_millis(10));
        assert_eq!(buf.packets_delivered, 50);

        // Phase 2: Simulate burst loss — a packet arrives far ahead,
        // causing next_seq to jump (gap-skip via the capacity overflow path).
        // We deliberately do NOT tick here: the far-ahead packet must remain
        // unemitted so that a later resync can rewind next_seq without
        // re-emitting an already-delivered sequence (which would kill the
        // downstream MPEG-TS pipeline with a timestamping error).
        let burst_time = start + Duration::from_millis(100);
        buf.push(5000, Bytes::from_static(b"far-ahead"), burst_time);

        // next_seq should now be well ahead of 50
        assert!(
            buf.next_seq > 100,
            "next_seq should have jumped: {}",
            buf.next_seq
        );

        // Phase 3: Sender continues from seq 60 (it doesn't know about our jump).
        // All of these will be "late" since 60 < next_seq (~4937+).
        let resume_time = burst_time + Duration::from_millis(200);
        for i in 60u64..260 {
            buf.push(
                i,
                Bytes::from(vec![i as u8]),
                resume_time + Duration::from_millis(i - 60),
            );
        }

        // After 100 consecutive late packets, desync recovery should fire.
        // Packets after the reset should be insertable.
        // next_seq should have been reset to somewhere around seq 160.
        assert!(
            buf.next_seq < 5000,
            "next_seq should have been reset after desync: {}",
            buf.next_seq
        );

        // Deliver the packets that were inserted after the resync
        let out = buf.tick(resume_time + Duration::from_millis(300));
        assert!(
            !out.is_empty(),
            "Should deliver packets after desync recovery (delivered {} total)",
            buf.packets_delivered,
        );
    }

    /// The desync counter resets when a normal (non-late) packet arrives,
    /// so occasional late arrivals during normal operation don't falsely
    /// trigger a reset.
    #[test]
    fn test_desync_counter_resets_on_normal_arrival() {
        let config = ReassemblyConfig {
            start_latency: Duration::from_millis(10),
            buffer_capacity: 64,
            skip_after: Some(Duration::from_millis(5)),
            ..ReassemblyConfig::test_defaults()
        };
        let mut buf = ReassemblyBuffer::with_config(0, config);
        let start = Instant::now();

        // Deliver packets 0..10
        for i in 0u64..10 {
            buf.push(i, Bytes::from(vec![0; 10]), start);
        }
        let _ = buf.tick(start + Duration::from_millis(10));

        // Send 50 late packets (below next_seq=10), then a normal one
        for i in 0u64..50 {
            buf.push(
                i,
                Bytes::from(vec![0; 10]),
                start + Duration::from_millis(20),
            );
        }
        // Interrupt with a valid in-sequence packet
        buf.push(
            10,
            Bytes::from(vec![0; 10]),
            start + Duration::from_millis(20),
        );

        // Counter should have been reset. Send 50 more late packets —
        // total late is 100 but the counter only reached 50 each time.
        for i in 0u64..50 {
            buf.push(
                i,
                Bytes::from(vec![0; 10]),
                start + Duration::from_millis(30),
            );
        }

        // next_seq should NOT have been reset — seq 10 was accepted but
        // not yet released (needs tick), so next_seq stays at 10.
        assert_eq!(
            buf.next_seq, 10,
            "next_seq should not reset with intermittent normal arrivals"
        );
    }

    #[test]
    fn bonded_inter_link_skew_widens_playout_window() {
        // Field run: inter-arrival jitter ~19 ms while bonded inter-link
        // arrival skew hit 250–400 ms during a one-modem fade, causing the
        // playout window to sit below the skew → ~4 % of delivered packets
        // dropped as "late". Regression: the new delay-spread component
        // must lift `target_latency` to cover the skew so spread-induced
        // lateness never fires.
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(50));
        let start = Instant::now();

        // Phase 1: warm with steady inter-arrival to seed jitter EWMA.
        for i in 0u64..20 {
            let t = start + Duration::from_millis(i * 10);
            let ts = (i as u32) * 10_000; // synthetic sender ts, 10 ms cadence
            buf.push_with_ts(i, Bytes::from(vec![0u8; 100]), t, ts);
        }
        let baseline_latency_ms = buf.target_latency.as_millis() as u64;

        // Phase 2: simulate two bonded links with a 300 ms inter-link skew —
        // odd seqs ride a slow link (large send-vs-arrival lag), even seqs
        // a fast link (small lag). Inter-arrival cadence stays steady.
        for i in 20u64..80 {
            let t = start + Duration::from_millis(200 + (i - 20) * 10);
            // Fast link: send_ts ≈ arrival − 50 ms. Slow link: send_ts ≈
            // arrival − 350 ms (300 ms more delay).
            let arrival_us = ((200 + (i - 20) * 10) * 1000) as u32;
            let ts = if i % 2 == 0 {
                arrival_us.saturating_sub(50_000)
            } else {
                arrival_us.saturating_sub(350_000)
            };
            buf.push_with_ts(i, Bytes::from(vec![0u8; 100]), t, ts);
        }

        let skewed_target_ms = buf.target_latency.as_millis() as u64;
        assert!(
            skewed_target_ms >= 300,
            "playout window must cover the 300 ms inter-link skew \
             (target_latency={} ms, baseline={} ms)",
            skewed_target_ms,
            baseline_latency_ms,
        );
        assert!(
            skewed_target_ms > baseline_latency_ms,
            "inter-link skew must widen the window beyond a no-skew \
             baseline (was {} ms, now {} ms)",
            baseline_latency_ms,
            skewed_target_ms,
        );
    }

    #[test]
    fn tick_sets_discont_after_gap_skip() {
        let mut buf = ReassemblyBuffer::new_for_test(0, Duration::from_millis(50));
        let start = Instant::now();

        // Push seq 0 and seq 2 (gap at seq 1)
        buf.push(0, Bytes::from_static(b"P0"), start);
        buf.push(2, Bytes::from_static(b"P2"), start);

        // Tick after latency — seq 0 released, then gap at 1 skipped, then seq 2 released
        let out = buf.tick(start + Duration::from_millis(50));
        assert_eq!(out.len(), 2);
        // First packet had no gap before it
        assert_eq!(out[0].0, Bytes::from_static(b"P0"));
        assert!(!out[0].1, "P0 should NOT have discont flag");
        // Second packet was preceded by a gap (seq 1 skipped)
        assert_eq!(out[1].0, Bytes::from_static(b"P2"));
        assert!(out[1].1, "P2 should have discont flag after gap skip");
        assert_eq!(buf.get_stats().discontinuities, 1);
    }

    // ── Phase 1 (HLS floor): production defaults absorb bonded-cellular tail OWD ──

    /// With the production defaults (start=1500, min=1000, max=3000), a
    /// 1.2 s tail spike — representative of cellular HARQ retries plus
    /// saturation-probe pinning — must not produce any late drops. This
    /// is the artifact-stopping regression guard from the field saga.
    #[test]
    fn production_defaults_absorb_1200ms_owd_spike() {
        let mut buf = ReassemblyBuffer::with_config(0, ReassemblyConfig::default());
        let start = Instant::now();

        // 100 packets, in-order seq, 10 ms apart on average. Every 10th
        // arrives 1200 ms later than its neighbours (simulating a probe-
        // induced tail event). With a 1500 ms baseline buffer those late
        // arrivals are still within the playout window.
        for i in 0..100u64 {
            let extra = if i % 10 == 0 { 1200 } else { 0 };
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(i * 10 + extra),
            );
        }
        let _ = buf.tick(start + Duration::from_millis(5000));

        let stats = buf.get_stats();
        assert_eq!(
            stats.late_packets, 0,
            "1.2 s OWD spike must not cause late drops with a 1500 ms baseline (got {} late)",
            stats.late_packets
        );
    }

    /// Steady-state with low jitter and zero loss must settle the target
    /// near the configured baseline, not above. Guards against the
    /// adaptive controller bloating the window when nothing is wrong.
    #[test]
    fn production_defaults_steady_state_settles_near_baseline() {
        let mut buf = ReassemblyBuffer::with_config(0, ReassemblyConfig::default());
        let start = Instant::now();

        // 200 packets, in-order, 5 ms apart, ±2 ms jitter.
        for i in 0..200u64 {
            let jitter = i % 5; // 0..4 ms
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(i * 5 + jitter),
            );
        }
        let _ = buf.tick(start + Duration::from_millis(2000));

        let stats = buf.get_stats();
        assert!(
            stats.target_latency_ms >= 1500 && stats.target_latency_ms <= 1700,
            "target should settle near 1500 ms baseline under low jitter (got {} ms)",
            stats.target_latency_ms
        );
        assert_eq!(
            stats.late_packets, 0,
            "no late drops under steady low-jitter traffic"
        );
    }

    /// The downward smoothing path must never let `latency` dip below the
    /// configured `min_latency_ms`, even when the AIMD drain has fully
    /// emptied `late_pressure_ms` and the dynamic component is tiny.
    #[test]
    fn production_defaults_latency_never_below_min_floor() {
        let mut buf = ReassemblyBuffer::with_config(0, ReassemblyConfig::default());
        let start = Instant::now();

        // Long, calm traffic: 2 minutes of in-order packets 20 ms apart.
        // Ample time for downward smoothing and any drain to act.
        for i in 0..6000u64 {
            buf.push(
                i,
                Bytes::from(vec![0; 100]),
                start + Duration::from_millis(i * 20),
            );
        }
        let _ = buf.tick(start + Duration::from_millis(125_000));

        let stats = buf.get_stats();
        assert!(
            stats.current_latency_ms >= 1000,
            "latency must never dip below min_latency_ms=1000 (got {} ms)",
            stats.current_latency_ms
        );
    }
}
