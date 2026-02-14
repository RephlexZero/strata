use crate::config::SchedulerConfig;
use crate::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use crate::scheduler::sbd::SbdEngine;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-link state tracked by the DWRR scheduler.
///
/// Holds the link's credit balance, throughput measurements, trend slopes,
/// and penalty factors used to compute effective capacity during link selection.
pub(crate) struct LinkState<L: ?Sized> {
    pub link: Arc<L>,
    pub credits: f64,
    pub last_update: Instant,
    pub metrics: LinkMetrics,
    pub sent_bytes: u64,
    pub last_sent_bytes: u64,
    pub last_sent_at: Instant,
    pub measured_bps: f64,
    pub spare_capacity_bps: f64,
    pub has_traffic: bool,
    pub prev_capacity_bps: f64,
    pub prev_rtt_ms: f64,
    pub prev_loss_rate: f64,
    pub bw_slope_bps_s: f64,
    pub rtt_slope_ms_s: f64,
    pub loss_slope_per_s: f64,
    pub last_metrics_update: Instant,
    pub penalty_factor: f64,
    // --- AIMD Capacity Estimator state ---
    /// The AIMD delay-gradient capacity estimate (bps).
    pub estimated_capacity_bps: f64,
    /// Fast sliding window (~3s) for min RTT baseline tracking.
    pub rtt_min_fast_window: VecDeque<f64>,
    /// Slow sliding window (~30s) for min RTT baseline tracking.
    pub rtt_min_slow_window: VecDeque<f64>,
    /// Effective RTT baseline: min(fast_min, slow_min).
    pub rtt_baseline: f64,
    /// Timestamp of the last multiplicative decrease (for cooldown).
    pub last_decrease_at: Instant,
    /// Whether the AIMD estimate has been initialized from first traffic.
    pub aimd_initialized: bool,
    // --- RFC 8698 NADA state ---
    /// Minimum filter buffer for RTT samples (RFC 8698 §5.1.1, 15 samples).
    pub rtt_sample_filter: VecDeque<f64>,
    /// Previous aggregate congestion signal for PI-controller (RFC 8698 §4.3).
    pub x_prev: f64,
    /// Rate adaptation mode: false = accelerated ramp-up, true = gradual update.
    pub gradual_mode: bool,
    /// Loss-free sample counter for accelerated ramp-up mode detection.
    pub loss_free_samples: u32,
}

/// Deficit Weighted Round Robin (DWRR) packet scheduler.
///
/// Distributes packets across links proportional to their effective capacity.
/// Each link accumulates byte "credits" at a rate proportional to its
/// quality-adjusted bandwidth. A link is selected when it has enough credits
/// to cover the packet cost. Credits are capped to a burst window that
/// scales with the link's lifecycle phase and loss rate.
///
/// The scheduler also provides broadcast, best-N selection (for redundancy),
/// and spare-capacity queries used by higher-level bonding logic.
pub struct Dwrr<L: LinkSender + ?Sized> {
    links: HashMap<usize, LinkState<L>>,
    sorted_ids: Vec<usize>,
    current_rr_idx: usize,
    config: SchedulerConfig,
    /// Cached spare-capacity ratio updated by `refresh_metrics()`.
    /// Avoids cloning all LinkMetrics on the hot packet path.
    cached_spare_ratio: f64,
    /// Cached total capacity of alive Live/Warm links (bps).
    cached_total_capacity: f64,
    /// SBD bottleneck groups: links with the same group ID share a bottleneck.
    /// Updated by SBD module when `sbd_enabled`. Group 0 = no bottleneck.
    pub(crate) sbd_groups: HashMap<usize, usize>,
    /// Optional SBD engine: instantiated when `sbd_enabled` is true.
    sbd_engine: Option<SbdEngine>,
    /// Instant of the last SBD `process_interval` call.
    sbd_last_process: Instant,
}

/// Computes the burst window (in seconds) for credit capping.
///
/// Links in healthier phases (Live) get larger burst windows, allowing
/// them to absorb short traffic spikes. Degraded or probing links are
/// tightly limited. Loss further reduces the window.
fn compute_burst_window_s(phase: LinkPhase, loss_rate: f64) -> f64 {
    let base = match phase {
        LinkPhase::Probe => 0.02,
        LinkPhase::Warm => 0.05,
        LinkPhase::Live => 0.1,
        LinkPhase::Degrade => 0.04,
        LinkPhase::Cooldown | LinkPhase::Reset | LinkPhase::Init => 0.01,
    };

    let loss_factor = (1.0 - loss_rate).clamp(0.1, 1.0);
    (base * loss_factor).clamp(0.01, 0.1)
}

impl<L: LinkSender + ?Sized> Dwrr<L> {
    pub fn new() -> Self {
        Self::with_config(SchedulerConfig::default())
    }

    pub fn with_config(config: SchedulerConfig) -> Self {
        let sbd_engine = if config.sbd_enabled {
            Some(SbdEngine::new(
                config.sbd_n,
                config.sbd_c_s,
                config.sbd_c_h,
                config.sbd_p_l,
            ))
        } else {
            None
        };
        Self {
            links: HashMap::new(),
            sorted_ids: Vec::new(),
            current_rr_idx: 0,
            config,
            cached_spare_ratio: 0.0,
            cached_total_capacity: 0.0,
            sbd_groups: HashMap::new(),
            sbd_engine,
            sbd_last_process: Instant::now(),
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    pub fn update_config(&mut self, config: SchedulerConfig) {
        self.config = config;
    }

    pub fn add_link(&mut self, link: Arc<L>) {
        let id = link.id();
        let metrics = link.get_metrics();
        let now = Instant::now();
        self.links.insert(
            id,
            LinkState {
                metrics: metrics.clone(),
                link,
                credits: 0.0,
                last_update: now,
                sent_bytes: 0,
                last_sent_bytes: 0,
                last_sent_at: now,
                measured_bps: 0.0,
                spare_capacity_bps: 0.0,
                has_traffic: false,
                prev_capacity_bps: metrics.capacity_bps,
                prev_rtt_ms: metrics.rtt_ms,
                prev_loss_rate: metrics.loss_rate,
                bw_slope_bps_s: 0.0,
                rtt_slope_ms_s: 0.0,
                loss_slope_per_s: 0.0,
                last_metrics_update: Instant::now(),
                penalty_factor: 1.0,
                // AIMD state
                estimated_capacity_bps: 0.0,
                rtt_min_fast_window: VecDeque::new(),
                rtt_min_slow_window: VecDeque::new(),
                rtt_baseline: 0.0,
                last_decrease_at: now - Duration::from_secs(10), // allow immediate first MD
                aimd_initialized: false,
                // NADA state
                rtt_sample_filter: VecDeque::new(),
                x_prev: 0.0,
                gradual_mode: false,
                loss_free_samples: 0,
            },
        );
        self.sorted_ids.push(id);
        self.sorted_ids.sort();
        if let Some(sbd) = &mut self.sbd_engine {
            sbd.add_link(id);
        }
    }

    pub fn refresh_metrics(&mut self) {
        let capacity_floor = self.config.capacity_floor_bps;
        let penalty_decay = self.config.penalty_decay;
        let penalty_recovery = self.config.penalty_recovery;

        // AIMD config snapshot (avoid repeated field access)
        let aimd_enabled = self.config.capacity_estimate_enabled;
        let rtt_congestion_ratio = self.config.rtt_congestion_ratio;
        let md_factor = self.config.md_factor;
        let ai_step_ratio = self.config.ai_step_ratio;
        let decrease_cooldown = Duration::from_millis(self.config.decrease_cooldown_ms);
        let fast_window_samples =
            (self.config.rtt_min_fast_window_s / 0.1).round().max(1.0) as usize; // ~100ms per sample
        let slow_window_samples =
            (self.config.rtt_min_slow_window_s / 0.1).round().max(1.0) as usize;
        let max_capacity_bps = self.config.max_capacity_bps;
        let loss_md_threshold = self.config.loss_md_threshold;

        // NADA / RFC 8698 config snapshot
        let dloss_ref = self.config.dloss_ref_ms;
        let plr_ref = self.config.plr_ref;
        let gamma_max = self.config.gamma_max;
        let qbound = self.config.qbound_ms;
        let qeps = self.config.qeps_ms;
        let nada_kappa = self.config.nada_kappa;
        let nada_eta = self.config.nada_eta;
        let nada_tau = self.config.nada_tau_ms;
        let nada_xref = self.config.nada_xref_ms;
        let nada_prio = self.config.nada_prio;

        // --- Coupled AI pre-pass (RFC 6356 §3) ---
        // Computes a coupling factor `alpha` that ensures the aggregate
        // additive increase across N sub-flows sharing a bottleneck does
        // not exceed the increase of a single TCP flow.
        //   alpha = cap_total * max_i(cap_i / rtt_i²) / (sum_i(cap_i / rtt_i))²
        //
        // When SBD is enabled, coupling is computed per-group so that only
        // links sharing a bottleneck are coupled.  Links in group 0 (no
        // shared bottleneck) get alpha = 1.0.
        let per_link_alpha: HashMap<usize, f64> = if aimd_enabled {
            if self.config.sbd_enabled && !self.sbd_groups.is_empty() {
                // Build per-group accumulators.
                let mut group_accum: HashMap<usize, (f64, f64, f64)> = HashMap::new();
                for (&id, state) in &self.links {
                    if state.aimd_initialized && state.rtt_baseline > 0.0 {
                        let group = self.sbd_groups.get(&id).copied().unwrap_or(0);
                        let cap = state.estimated_capacity_bps;
                        let rtt = state.rtt_baseline;
                        let entry = group_accum.entry(group).or_insert((0.0, 0.0, 0.0));
                        entry.0 += cap; // cap_total
                        entry.1 = entry.1.max(cap / (rtt * rtt)); // max_cap_rtt2
                        entry.2 += cap / rtt; // sum_cap_rtt
                    }
                }
                // Compute per-link alpha from group membership.
                self.links
                    .keys()
                    .map(|&id| {
                        let group = self.sbd_groups.get(&id).copied().unwrap_or(0);
                        if group == 0 {
                            // Not in a shared bottleneck — no coupling.
                            (id, 1.0)
                        } else if let Some(&(cap_total, max_cap_rtt2, sum_cap_rtt)) =
                            group_accum.get(&group)
                        {
                            if sum_cap_rtt > 0.0 && cap_total > 0.0 {
                                let alpha = (cap_total * max_cap_rtt2
                                    / (sum_cap_rtt * sum_cap_rtt))
                                    .clamp(0.01, 1.0);
                                (id, alpha)
                            } else {
                                (id, 1.0)
                            }
                        } else {
                            (id, 1.0)
                        }
                    })
                    .collect()
            } else {
                // SBD disabled — global coupling across all initialized links.
                let mut cap_total = 0.0_f64;
                let mut max_cap_rtt2 = 0.0_f64;
                let mut sum_cap_rtt = 0.0_f64;
                for state in self.links.values() {
                    if state.aimd_initialized && state.rtt_baseline > 0.0 {
                        let cap = state.estimated_capacity_bps;
                        let rtt = state.rtt_baseline;
                        cap_total += cap;
                        max_cap_rtt2 = max_cap_rtt2.max(cap / (rtt * rtt));
                        sum_cap_rtt += cap / rtt;
                    }
                }
                let global_alpha = if sum_cap_rtt > 0.0 && cap_total > 0.0 {
                    (cap_total * max_cap_rtt2 / (sum_cap_rtt * sum_cap_rtt)).clamp(0.01, 1.0)
                } else {
                    1.0
                };
                self.links.keys().map(|&id| (id, global_alpha)).collect()
            }
        } else {
            self.links.keys().map(|&id| (id, 1.0)).collect()
        };

        for (&link_id, state) in self.links.iter_mut() {
            let coupled_alpha = per_link_alpha.get(&link_id).copied().unwrap_or(1.0);
            let now = Instant::now();
            state.metrics = state.link.get_metrics();
            // Capture the receiver-reported capacity before bootstrap overwrites it.
            // This value is an external signal free from sender-side feedback loops
            // (see RFC 6356 §5, RFC 8698 §4.3).
            let rist_capacity = state.metrics.capacity_bps;
            let dt_sent = now.duration_since(state.last_sent_at).as_secs_f64();
            if dt_sent > 0.0 {
                let delta_bytes = state.sent_bytes.saturating_sub(state.last_sent_bytes);
                if delta_bytes > 0 {
                    state.measured_bps = (delta_bytes as f64 * 8.0) / dt_sent;
                    state.has_traffic = true;
                }
                state.last_sent_bytes = state.sent_bytes;
                state.last_sent_at = now;
            }

            state.metrics.observed_bps = state.measured_bps;
            state.metrics.observed_bytes = state.sent_bytes;

            if state.metrics.observed_bps > 0.0 {
                state.metrics.alive = true;
            }

            if state.metrics.capacity_bps < 1_000_000.0 {
                // Use configured capacity floor for bootstrap
                if state.measured_bps > (capacity_floor * 0.3) {
                    state.metrics.capacity_bps = state.measured_bps * 2.0;
                } else {
                    state.metrics.capacity_bps = capacity_floor;
                }
            }
            let prev_capacity = state.prev_capacity_bps;
            let curr_capacity = state.metrics.capacity_bps;
            if prev_capacity > 0.0 && curr_capacity < prev_capacity * 0.5 {
                state.penalty_factor = (state.penalty_factor * penalty_decay).max(0.3);
            } else {
                state.penalty_factor = (state.penalty_factor + penalty_recovery).min(1.0);
            }

            let dt = now.duration_since(state.last_metrics_update).as_secs_f64();
            if dt > 0.0 {
                // Keep bw_slope tracking raw wire-rate (capacity_bps), NOT estimated_capacity_bps
                state.bw_slope_bps_s = (curr_capacity - state.prev_capacity_bps) / dt;
                state.rtt_slope_ms_s = (state.metrics.rtt_ms - state.prev_rtt_ms) / dt;
                state.loss_slope_per_s = (state.metrics.loss_rate - state.prev_loss_rate) / dt;
            }

            state.prev_capacity_bps = curr_capacity;
            state.prev_rtt_ms = state.metrics.rtt_ms;
            state.prev_loss_rate = state.metrics.loss_rate;
            state.last_metrics_update = now;

            // ======== NADA-style Capacity Estimator ========
            // Combines:
            //   #1 Coupled AI (RFC 6356 §3) — `coupled_alpha` from pre-pass
            //   #2 Unified Congestion Signal (RFC 8698 §4.2)
            //   #3 Dual-Mode Ramp-Up (RFC 8698 §4.3)
            //   #4 Min Filter for Delay (RFC 8698 §5.1.1)
            //   #7 PI-Controller Gradual Mode (RFC 8698 §4.3)
            if aimd_enabled {
                let current_rtt = state.metrics.rtt_ms;

                // Initialize from first observed traffic
                if !state.aimd_initialized && state.has_traffic && state.metrics.capacity_bps > 0.0
                {
                    state.estimated_capacity_bps = state.metrics.capacity_bps;
                    state.rtt_baseline = current_rtt;
                    state.aimd_initialized = true;
                }

                // Reset windows on lifecycle phase transitions (Probe, Warm, Reset)
                let phase_reset = matches!(
                    state.metrics.phase,
                    LinkPhase::Probe | LinkPhase::Warm | LinkPhase::Reset
                );

                if state.aimd_initialized {
                    // --- #4: Min Filter (RFC 8698 §5.1.1) ---
                    // Push raw RTT through a 15-sample minimum filter to strip
                    // jitter-induced spikes before feeding the baseline tracker.
                    if phase_reset {
                        state.rtt_sample_filter.clear();
                        state.rtt_min_fast_window.clear();
                        state.rtt_min_slow_window.clear();
                        // Reset estimated capacity to the floor so the link
                        // doesn't retain a depressed pre-death value that
                        // causes a slow ramp-up after revival.
                        state.estimated_capacity_bps = capacity_floor;
                        state.x_prev = 0.0;
                        state.gradual_mode = false;
                        state.loss_free_samples = 0;
                    }

                    if current_rtt > 0.0 {
                        state.rtt_sample_filter.push_back(current_rtt);
                        while state.rtt_sample_filter.len() > 15 {
                            state.rtt_sample_filter.pop_front();
                        }
                    }
                    let filtered_rtt = state
                        .rtt_sample_filter
                        .iter()
                        .copied()
                        .fold(f64::MAX, f64::min);
                    let filtered_rtt = if filtered_rtt == f64::MAX {
                        current_rtt
                    } else {
                        filtered_rtt
                    };

                    // --- RTT Baseline Tracking (dual-speed windowed minimum) ---
                    if filtered_rtt > 0.0 {
                        // Fast window (~3s)
                        state.rtt_min_fast_window.push_back(filtered_rtt);
                        while state.rtt_min_fast_window.len() > fast_window_samples {
                            state.rtt_min_fast_window.pop_front();
                        }
                        // Slow window (~30s)
                        state.rtt_min_slow_window.push_back(filtered_rtt);
                        while state.rtt_min_slow_window.len() > slow_window_samples {
                            state.rtt_min_slow_window.pop_front();
                        }
                        let fast_min = state
                            .rtt_min_fast_window
                            .iter()
                            .copied()
                            .fold(f64::MAX, f64::min);
                        let slow_min = state
                            .rtt_min_slow_window
                            .iter()
                            .copied()
                            .fold(f64::MAX, f64::min);
                        state.rtt_baseline = fast_min.min(slow_min);
                    }

                    // --- #2: Unified Congestion Signal (RFC 8698 §4.2) ---
                    // Folds delay-gradient and loss into a single scalar:
                    //   x_curr = d_queuing + DLOSS_REF * (p_loss / PLR_REF)²
                    if state.rtt_baseline > 0.0 {
                        let queuing_delay = (filtered_rtt - state.rtt_baseline).max(0.0);
                        let loss = state.metrics.loss_rate;
                        let loss_penalty = if plr_ref > 0.0 {
                            dloss_ref * (loss / plr_ref).powi(2)
                        } else {
                            0.0
                        };
                        let x_curr = queuing_delay + loss_penalty;

                        let rtt_ratio = filtered_rtt / state.rtt_baseline;
                        let since_last_decrease = now.duration_since(state.last_decrease_at);

                        // Suppress AI during Probe/Cooldown/Init/Reset
                        let suppress_increase = matches!(
                            state.metrics.phase,
                            LinkPhase::Probe
                                | LinkPhase::Cooldown
                                | LinkPhase::Init
                                | LinkPhase::Reset
                        );

                        // --- #3: Mode Detection (RFC 8698 §4.3) ---
                        // Track consecutive loss-free samples for ramp-up eligibility.
                        if loss == 0.0 {
                            state.loss_free_samples = state.loss_free_samples.saturating_add(1);
                        } else {
                            state.loss_free_samples = 0;
                        }

                        if queuing_delay < qeps
                            && loss == 0.0
                            && state.loss_free_samples > 5
                            && !suppress_increase
                            && state.has_traffic
                        {
                            // === Accelerated Ramp-Up Mode (RFC 8698 §4.3) ===
                            // Path is clearly underutilized — multiplicative increase
                            // bounded by self-inflicted queuing budget `qbound`.
                            state.gradual_mode = false;

                            let r_recv = rist_capacity.max(state.estimated_capacity_bps);
                            // gamma ≤ min(gamma_max, qbound / (rtt * r_recv_norm))
                            // r_recv_norm converts bps→Mbps for dimensional balance
                            let gamma = if filtered_rtt > 0.0 && r_recv > 0.0 {
                                gamma_max.min(qbound / (filtered_rtt * (r_recv / 1_000_000.0)))
                            } else {
                                gamma_max
                            };

                            // #1 Coupled AI: scale increase by coupled_alpha
                            state.estimated_capacity_bps += coupled_alpha
                                * gamma
                                * (r_recv - state.estimated_capacity_bps)
                                    .max(state.estimated_capacity_bps * ai_step_ratio);
                        } else {
                            // === Gradual Update Mode ===
                            state.gradual_mode = true;

                            // --- Multiplicative Decrease ---
                            // Two independent triggers, both subject to cooldown:
                            // 1. Delay-gradient: elevated RTT ratio AND rising slope
                            // 2. Loss-based: sustained loss above threshold
                            let delay_md =
                                rtt_ratio > rtt_congestion_ratio && state.rtt_slope_ms_s > 0.0;
                            let loss_md = loss > loss_md_threshold;

                            if (delay_md || loss_md) && since_last_decrease > decrease_cooldown {
                                state.estimated_capacity_bps *= md_factor;
                                state.last_decrease_at = now;
                            } else if !suppress_increase
                                && state.has_traffic
                                && state.measured_bps > state.estimated_capacity_bps * 0.3
                            {
                                // --- #7: PI-Controller (RFC 8698 §4.3) ---
                                // Replaces binary AIMD increase with a smooth
                                // proportional-integral controller:
                                //   x_offset = x_curr − prio·xref·rmax/r_n
                                //   x_diff   = x_curr − x_prev
                                //   r_n *= 1 − κ·(δ/τ)·(x_offset + η·x_diff)
                                let rmax = if max_capacity_bps > 0.0 {
                                    max_capacity_bps
                                } else {
                                    rist_capacity.max(capacity_floor) * 2.0
                                };
                                let r_n = state.estimated_capacity_bps.max(1.0);
                                let x_offset = x_curr - nada_prio * nada_xref * rmax / r_n;
                                let x_diff = x_curr - state.x_prev;
                                let delta_ms = (dt * 1000.0).min(nada_tau);

                                let adjust = nada_kappa
                                    * (delta_ms / nada_tau)
                                    * (x_offset + nada_eta * x_diff);

                                // #1 Coupled AI: scale AI portion by coupled_alpha
                                if adjust < 0.0 {
                                    // Negative adjust → rate increase (signal below target)
                                    state.estimated_capacity_bps *= 1.0 - coupled_alpha * adjust;
                                } else {
                                    // Positive adjust → rate decrease (signal above target)
                                    state.estimated_capacity_bps *= 1.0 - adjust;
                                }
                            }
                        }

                        // Persist congestion signal for next PI-controller iteration
                        state.x_prev = x_curr;
                    }

                    // --- Clamping ---
                    // Upper bound uses only receiver-reported capacity (rist_capacity),
                    // NOT measured_bps which is contaminated by DWRR's own allocation
                    // decisions and creates a positive feedback spiral.
                    // During bootstrap (rist_capacity not yet available from librist),
                    // allow generous headroom so the estimator can probe upward.
                    let upper = if max_capacity_bps > 0.0 {
                        max_capacity_bps
                    } else if rist_capacity > capacity_floor {
                        // Receiver has reported real capacity — tight bound
                        rist_capacity * 2.0
                    } else {
                        // Bootstrap: receiver hasn't reported yet. Use measured
                        // throughput with a 3× factor as a temporary ceiling.
                        // This is less tight than rist_capacity×2 but avoids the
                        // old positive-feedback spiral because the factor is fixed.
                        state.measured_bps.max(capacity_floor) * 3.0
                    };
                    state.estimated_capacity_bps =
                        state.estimated_capacity_bps.clamp(capacity_floor, upper);

                    // Propagate to metrics
                    state.metrics.estimated_capacity_bps = state.estimated_capacity_bps;
                }
            }

            // Calculate spare capacity using estimated_capacity when AIMD is active
            let effective_capacity = if aimd_enabled && state.aimd_initialized {
                state.estimated_capacity_bps
            } else {
                state.metrics.capacity_bps
            };

            if state.has_traffic {
                state.spare_capacity_bps = (effective_capacity - state.measured_bps).max(0.0);
            } else {
                state.spare_capacity_bps = 0.0;
            }
        }

        // Update cached spare-capacity ratio for hot-path use
        let total_spare = self.total_spare_capacity();
        let aimd_active = aimd_enabled;
        let total_cap: f64 = self
            .links
            .values()
            .filter(|s| {
                s.metrics.alive && matches!(s.metrics.phase, LinkPhase::Live | LinkPhase::Warm)
            })
            .map(|s| {
                if aimd_active && s.aimd_initialized {
                    s.estimated_capacity_bps
                } else {
                    s.metrics.capacity_bps
                }
            })
            .sum();
        self.cached_total_capacity = total_cap;
        self.cached_spare_ratio = if total_cap > 0.0 {
            total_spare / total_cap
        } else {
            0.0
        };

        // --- SBD: feed delay/loss samples and process intervals (RFC 8382) ---
        if let Some(sbd) = &mut self.sbd_engine {
            // Feed per-link OWD and loss data collected during the refresh above.
            for (&id, state) in &self.links {
                if state.metrics.owd_ms > 0.0 {
                    sbd.record_delay(id, state.metrics.owd_ms);
                }
                if state.metrics.loss_rate > 0.0 {
                    // Approximate discrete loss events from the continuous loss rate.
                    // Each refresh cycle represents ~1 observation; fractional losses
                    // are rounded so that rates ≥ 0.5% register at least one event.
                    let loss_events = (state.metrics.loss_rate * 10.0).ceil() as u32;
                    for _ in 0..loss_events {
                        sbd.record_loss(id);
                    }
                }
            }

            // Process the SBD base interval on the configured cadence.
            let sbd_interval = Duration::from_millis(self.config.sbd_interval_ms);
            if self.sbd_last_process.elapsed() >= sbd_interval {
                sbd.process_interval();
                self.sbd_groups = sbd.compute_groups();
                self.sbd_last_process = Instant::now();
            }
        }
    }

    pub fn remove_link(&mut self, id: usize) {
        self.links.remove(&id);
        if let Some(pos) = self.sorted_ids.iter().position(|&x| x == id) {
            self.sorted_ids.remove(pos);
        }
        // Reset RR index if out of bounds
        if self.current_rr_idx >= self.sorted_ids.len() {
            self.current_rr_idx = 0;
        }
        if let Some(sbd) = &mut self.sbd_engine {
            sbd.remove_link(id);
        }
        self.sbd_groups.remove(&id);
    }

    pub fn get_active_links(&self) -> Vec<(usize, crate::net::interface::LinkMetrics)> {
        self.links
            .iter()
            .map(|(id, l)| (*id, l.metrics.clone()))
            .collect()
    }

    pub fn record_send(&mut self, id: usize, bytes: u64) {
        if let Some(state) = self.links.get_mut(&id) {
            state.sent_bytes = state.sent_bytes.saturating_add(bytes);
            state.has_traffic = true;
        }
    }

    /// Mark a link as having traffic (for testing). In production, this is set
    /// automatically when bytes are sent or observed.
    pub fn mark_has_traffic(&mut self, id: usize) {
        if let Some(state) = self.links.get_mut(&id) {
            state.has_traffic = true;
        }
    }

    /// Returns the total spare capacity (unused bandwidth) across all Live/Warm links.
    /// This is used for calculating the redundancy budget.
    pub fn total_spare_capacity(&self) -> f64 {
        self.links
            .values()
            .filter(|state| {
                matches!(state.metrics.phase, LinkPhase::Live | LinkPhase::Warm)
                    && state.metrics.alive
            })
            .map(|state| state.spare_capacity_bps)
            .sum()
    }

    /// Returns the cached spare-capacity ratio, updated by `refresh_metrics()`.
    /// Use this on the hot packet path instead of calling `get_active_links()`.
    pub fn cached_spare_ratio(&self) -> f64 {
        self.cached_spare_ratio
    }

    /// Returns all alive links and deducts the cost from their credits.
    /// This is used for broadcasting critical packets.
    pub fn broadcast_links(&mut self, packet_len: usize) -> Vec<Arc<L>> {
        let packet_cost = packet_len as f64;
        let mut alive_links = Vec::new();

        let any_alive = self.links.values().any(|state| state.metrics.alive);

        for state in self.links.values_mut() {
            if state.metrics.alive || !any_alive {
                state.credits -= packet_cost;
                alive_links.push(state.link.clone());
            }
        }
        alive_links
    }

    /// Selects the best N links with diversity preference.
    /// Prefers links from different carriers/interfaces (link_kind) to maximize path independence.
    pub fn select_best_n_links(&mut self, packet_len: usize, n: usize) -> Vec<Arc<L>> {
        let packet_cost = packet_len as f64;
        let mut selected = Vec::new();
        let mut used_kinds: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Score all alive links
        let mut scored_links: Vec<_> = self
            .links
            .iter()
            .filter(|(_, state)| state.metrics.alive)
            .map(|(id, state)| {
                // Quality score: capacity * (loss_quality * 0.5 + rtt_quality * 0.3 + phase * 0.2)
                let loss_quality = (1.0 - state.metrics.loss_rate).max(0.0);
                let rtt_quality = 1.0 / (1.0 + state.metrics.rtt_ms / 100.0);
                let phase_weight = match state.metrics.phase {
                    LinkPhase::Live => 1.0,
                    LinkPhase::Warm => 0.8,
                    LinkPhase::Degrade => 0.5,
                    LinkPhase::Probe => 0.3,
                    _ => 0.1,
                };

                let quality_score = state.metrics.capacity_bps
                    * (loss_quality * 0.5 + rtt_quality * 0.3 + phase_weight * 0.2);

                (*id, quality_score, state.metrics.link_kind.clone())
            })
            .collect();

        // Sort by quality score descending
        scored_links.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // First pass: select diverse links (different link_kind) in quality order
        for (id, _score, link_kind) in &scored_links {
            if selected.len() >= n {
                break;
            }

            let is_diverse = match link_kind {
                None => true, // Unknown kind is always considered diverse
                Some(kind) => !used_kinds.contains(kind.as_str()),
            };

            if is_diverse {
                if let Some(state) = self.links.get_mut(id) {
                    state.credits -= packet_cost;
                    selected.push(state.link.clone());
                    if let Some(kind) = link_kind {
                        used_kinds.insert(kind.clone());
                    }
                }
            }
        }

        // Second pass: fill remaining slots with best quality links regardless of diversity
        if selected.len() < n {
            for (id, _score, _) in &scored_links {
                if selected.len() >= n {
                    break;
                }
                if !selected.iter().any(|l| l.id() == *id) {
                    if let Some(state) = self.links.get_mut(id) {
                        state.credits -= packet_cost;
                        selected.push(state.link.clone());
                    }
                }
            }
        }

        selected
    }

    pub fn select_link(&mut self, packet_len: usize) -> Option<Arc<L>> {
        if self.sorted_ids.is_empty() {
            return None;
        }

        let packet_cost = packet_len as f64;
        let now = Instant::now();
        let horizon_s = self.config.prediction_horizon_s;
        let aimd_enabled = self.config.capacity_estimate_enabled;

        // 1. Update Credits
        let any_alive = self.links.values().any(|state| state.metrics.alive);
        for state in self.links.values_mut() {
            if state.metrics.alive || !any_alive {
                let elapsed = now.duration_since(state.last_update).as_secs_f64();

                // Calculate Effective Capacity (Quality Aware)
                // Use estimated_capacity_bps when AIMD is active, else raw capacity_bps.
                let base_capacity = if aimd_enabled && state.aimd_initialized {
                    state.estimated_capacity_bps
                } else {
                    state.metrics.capacity_bps
                };
                let predicted_bw = (base_capacity + state.bw_slope_bps_s * horizon_s).max(0.0);
                let predicted_loss =
                    (state.metrics.loss_rate + state.loss_slope_per_s * horizon_s).clamp(0.0, 1.0);
                let predicted_rtt =
                    (state.metrics.rtt_ms + state.rtt_slope_ms_s * horizon_s).max(0.0);

                let quality_factor = (1.0 - predicted_loss).powi(4);
                let rtt_factor = 1.0 / (1.0 + predicted_rtt / 200.0);

                let phase_factor = match state.metrics.phase {
                    LinkPhase::Probe => 0.2,
                    LinkPhase::Warm => 0.6,
                    LinkPhase::Live => 1.0,
                    LinkPhase::Degrade => 0.7,
                    LinkPhase::Cooldown | LinkPhase::Reset | LinkPhase::Init => 0.1,
                };

                let os_up_factor = if matches!(state.metrics.os_up, Some(false)) {
                    0.2
                } else {
                    1.0
                };

                let effective_bps = predicted_bw
                    * quality_factor
                    * state.penalty_factor
                    * phase_factor
                    * os_up_factor
                    * rtt_factor;
                // Capacity is in bps (bits per sec). Convert to bytes per sec.
                let bytes_per_sec = effective_bps / 8.0;

                // Add credits
                state.credits += bytes_per_sec * elapsed;

                // Cap credits (adaptive burst window based on phase/loss)
                let burst_window_s = compute_burst_window_s(state.metrics.phase, predicted_loss);
                let max_credits = bytes_per_sec * burst_window_s;
                if state.credits > max_credits {
                    state.credits = max_credits;
                }
            }
            state.last_update = now;
        }

        // 2. Select Link (DWRR)
        let start_idx = self.current_rr_idx;
        let count = self.sorted_ids.len();

        for i in 0..count {
            let idx = (start_idx + i) % count;
            let id = self.sorted_ids[idx];

            if let Some(state) = self.links.get_mut(&id) {
                if !state.metrics.alive {
                    continue;
                }

                if state.credits >= packet_cost {
                    state.credits -= packet_cost;
                    self.current_rr_idx = (idx + 1) % count;
                    return Some(state.link.clone());
                }
            }
        }

        // Fallback: Pick link with max credits (best effort)
        let mut best_id = None;
        let mut max_creds = f64::MIN;

        for &id in &self.sorted_ids {
            if let Some(state) = self.links.get(&id) {
                if state.metrics.alive && state.credits > max_creds {
                    max_creds = state.credits;
                    best_id = Some(id);
                }
            }
        }

        if let Some(id) = best_id {
            if let Some(state) = self.links.get_mut(&id) {
                state.credits -= packet_cost; // Goes negative
                return Some(state.link.clone());
            }
        }

        None
    }
}

impl<L: LinkSender + ?Sized> Default for Dwrr<L> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::interface::LinkMetrics;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct MockLink {
        id: usize,
        metrics: Mutex<LinkMetrics>,
    }

    impl MockLink {
        fn new(id: usize, capacity_bps: f64, phase: LinkPhase) -> Self {
            Self {
                id,
                metrics: Mutex::new(LinkMetrics {
                    rtt_ms: 10.0,
                    capacity_bps,
                    loss_rate: 0.0,
                    observed_bps: 0.0,
                    observed_bytes: 0,
                    queue_depth: 0,
                    max_queue: 100,
                    alive: true,
                    phase,
                    os_up: None,
                    mtu: None,
                    iface: None,
                    link_kind: None,
                    estimated_capacity_bps: 0.0,
                    owd_ms: 0.0,
                }),
            }
        }

        fn set_capacity(&self, capacity_bps: f64) {
            if let Ok(mut m) = self.metrics.lock() {
                m.capacity_bps = capacity_bps;
            }
        }

        fn set_rtt(&self, rtt_ms: f64) {
            if let Ok(mut m) = self.metrics.lock() {
                m.rtt_ms = rtt_ms;
            }
        }

        fn set_loss_rate(&self, rate: f64) {
            if let Ok(mut m) = self.metrics.lock() {
                m.loss_rate = rate;
            }
        }

        fn set_phase(&self, phase: LinkPhase) {
            if let Ok(mut m) = self.metrics.lock() {
                m.phase = phase;
            }
        }

        fn set_owd(&self, owd_ms: f64) {
            if let Ok(mut m) = self.metrics.lock() {
                m.owd_ms = owd_ms;
            }
        }

        fn set_link_kind(&self, kind: &str) {
            if let Ok(mut m) = self.metrics.lock() {
                m.link_kind = Some(kind.to_string());
            }
        }
    }

    impl LinkSender for MockLink {
        fn id(&self) -> usize {
            self.id
        }

        fn send(&self, _packet: &[u8]) -> anyhow::Result<usize> {
            Ok(0)
        }

        fn get_metrics(&self) -> LinkMetrics {
            self.metrics.lock().unwrap().clone()
        }
    }

    #[test]
    fn penalty_factor_reacts_to_capacity_drop() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live)); // Start at 10M
        let mut dwrr = Dwrr::new();
        dwrr.add_link(link.clone());

        dwrr.refresh_metrics();
        let penalty = dwrr.links.get(&1).unwrap().penalty_factor;
        assert!((penalty - 1.0).abs() < 1e-6);

        // Drop capacity to 4M (< 50% of 10M, but > 1M so not bootstrapped)
        link.set_capacity(4_000_000.0);
        dwrr.refresh_metrics();
        let penalty = dwrr.links.get(&1).unwrap().penalty_factor;
        assert!((penalty - 0.7).abs() < 0.01); // Penalty should be 1.0 * 0.7 = 0.7
    }

    #[test]
    fn warmup_phase_reduces_credit_growth() {
        let live = Arc::new(MockLink::new(1, 1_000_000.0, LinkPhase::Live));
        let probe = Arc::new(MockLink::new(2, 1_000_000.0, LinkPhase::Probe));

        let mut dwrr = Dwrr::new();
        dwrr.add_link(live.clone());
        dwrr.add_link(probe.clone());

        // Force metrics refresh and provide elapsed time for credit accrual
        dwrr.refresh_metrics();
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.last_update -= Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.last_update -= Duration::from_secs(1);
        }

        let _ = dwrr.select_link(1200);
        let live_credits = dwrr.links.get(&1).unwrap().credits;
        let probe_credits = dwrr.links.get(&2).unwrap().credits;

        assert!(live_credits >= probe_credits);
    }

    #[test]
    fn burst_window_scales_by_phase() {
        let live = Arc::new(MockLink::new(1, 1_000_000.0, LinkPhase::Live));
        let probe = Arc::new(MockLink::new(2, 1_000_000.0, LinkPhase::Probe));

        let mut dwrr = Dwrr::new();
        dwrr.add_link(live.clone());
        dwrr.add_link(probe.clone());

        if let Some(state) = dwrr.links.get_mut(&1) {
            state.last_update -= Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.last_update -= Duration::from_secs(1);
        }

        let _ = dwrr.select_link(1);

        let live_state = dwrr.links.get(&1).unwrap();
        let probe_state = dwrr.links.get(&2).unwrap();

        let live_burst = compute_burst_window_s(LinkPhase::Live, 0.0);
        let probe_burst = compute_burst_window_s(LinkPhase::Probe, 0.0);

        let rtt_factor = 1.0 / (1.0 + 10.0 / 200.0);
        let live_max = (1_000_000.0 * rtt_factor / 8.0) * live_burst;
        let probe_max = (1_000_000.0 * rtt_factor * 0.2 / 8.0) * probe_burst;

        assert!(live_state.credits <= live_max + 1.0);
        assert!(probe_state.credits <= probe_max + 1.0);
    }

    #[test]
    fn test_predictive_scoring_prefers_positive_bw_trend() {
        let link1 = Arc::new(MockLink::new(1, 1_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 1_000_000.0, LinkPhase::Live));

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link1.clone());
        dwrr.add_link(link2.clone());

        let now = Instant::now();
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.prev_capacity_bps = 700_000.0;
            state.prev_rtt_ms = state.metrics.rtt_ms;
            state.prev_loss_rate = state.metrics.loss_rate;
            state.last_metrics_update = now - Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.prev_capacity_bps = 1_300_000.0;
            state.prev_rtt_ms = state.metrics.rtt_ms;
            state.prev_loss_rate = state.metrics.loss_rate;
            state.last_metrics_update = now - Duration::from_secs(1);
        }

        dwrr.refresh_metrics();

        if let Some(state) = dwrr.links.get_mut(&1) {
            state.last_update = Instant::now() - Duration::from_secs(1);
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.last_update = Instant::now() - Duration::from_secs(1);
        }

        let _ = dwrr.select_link(0);
        let link1_credits = dwrr.links.get(&1).unwrap().credits;
        let link2_credits = dwrr.links.get(&2).unwrap().credits;

        assert!(link1_credits > link2_credits);
    }

    #[test]
    fn test_spare_capacity_calculation() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let config = SchedulerConfig {
            capacity_estimate_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut dwrr = Dwrr::with_config(config);
        dwrr.add_link(link.clone());

        // Initial state - no traffic yet, spare should be 0 (not full capacity)
        dwrr.refresh_metrics();
        let state = dwrr.links.get(&1).unwrap();
        assert_eq!(
            state.spare_capacity_bps, 0.0,
            "spare should be 0 before any traffic"
        );

        // Simulate observed traffic at 6 Mbps (mark has_traffic)
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.measured_bps = 6_000_000.0;
            state.has_traffic = true;
        }
        dwrr.refresh_metrics();

        let state = dwrr.links.get(&1).unwrap();
        assert_eq!(state.spare_capacity_bps, 4_000_000.0); // 10M - 6M
    }

    #[test]
    fn test_total_spare_capacity() {
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 5_000_000.0, LinkPhase::Live));
        let link3 = Arc::new(MockLink::new(3, 8_000_000.0, LinkPhase::Probe));

        let config = SchedulerConfig {
            capacity_estimate_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut dwrr = Dwrr::with_config(config);
        dwrr.add_link(link1.clone());
        dwrr.add_link(link2.clone());
        dwrr.add_link(link3.clone());

        // Set observed traffic and mark as having traffic
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.measured_bps = 7_000_000.0; // 3M spare
            state.has_traffic = true;
        }
        if let Some(state) = dwrr.links.get_mut(&2) {
            state.measured_bps = 3_000_000.0; // 2M spare
            state.has_traffic = true;
        }
        if let Some(state) = dwrr.links.get_mut(&3) {
            state.measured_bps = 1_000_000.0; // 7M spare but link is Probe
            state.has_traffic = true;
        }

        dwrr.refresh_metrics();

        let total_spare = dwrr.total_spare_capacity();
        // Only Link1 (3M) + Link2 (2M) = 5M (Link3 excluded as Probe phase)
        assert_eq!(total_spare, 5_000_000.0);
    }

    #[test]
    fn test_diversity_aware_link_selection() {
        // Two high-capacity wired links and one lower-capacity cellular link.
        // With diversity preference, the cellular link should be chosen over
        // the second wired link despite lower capacity.
        let wired1 = Arc::new(MockLink::new(1, 12_000_000.0, LinkPhase::Live));
        let wired2 = Arc::new(MockLink::new(2, 11_000_000.0, LinkPhase::Live));
        let cellular = Arc::new(MockLink::new(3, 8_000_000.0, LinkPhase::Live));

        if let Ok(mut m) = wired1.metrics.lock() {
            m.link_kind = Some("wired".to_string());
        }
        if let Ok(mut m) = wired2.metrics.lock() {
            m.link_kind = Some("wired".to_string());
        }
        if let Ok(mut m) = cellular.metrics.lock() {
            m.link_kind = Some("cellular".to_string());
        }

        let mut dwrr = Dwrr::new();
        dwrr.add_link(wired1.clone());
        dwrr.add_link(wired2.clone());
        dwrr.add_link(cellular.clone());

        dwrr.refresh_metrics();

        // Select best 2 links — diversity should prefer wired + cellular
        // over wired + wired, even though wired2 has higher capacity than cellular.
        let selected = dwrr.select_best_n_links(1000, 2);
        assert_eq!(selected.len(), 2);

        let ids: Vec<usize> = selected.iter().map(|l| l.id()).collect();
        assert!(
            ids.contains(&1),
            "Highest capacity wired link should be selected"
        );
        assert!(
            ids.contains(&3),
            "Cellular link should be preferred over second wired link for diversity: got {:?}",
            ids
        );
    }

    #[test]
    fn select_link_with_zero_links() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        assert!(dwrr.select_link(1200).is_none());
    }

    #[test]
    fn select_link_with_all_dead_links() {
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 5_000_000.0, LinkPhase::Live));

        if let Ok(mut m) = link1.metrics.lock() {
            m.alive = false;
        }
        if let Ok(mut m) = link2.metrics.lock() {
            m.alive = false;
        }

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link1);
        dwrr.add_link(link2);
        dwrr.refresh_metrics();

        // Dead links are skipped in both main loop and fallback
        let result = dwrr.select_link(1200);
        assert!(result.is_none(), "All dead links should return None");
    }

    #[test]
    fn broadcast_links_with_no_alive() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        if let Ok(mut m) = link.metrics.lock() {
            m.alive = false;
        }

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link);
        dwrr.refresh_metrics();

        let links = dwrr.broadcast_links(100);
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn broadcast_links_empty_scheduler() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        let links = dwrr.broadcast_links(100);
        assert!(links.is_empty());
    }

    #[test]
    fn record_send_tracks_bytes() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::new();
        dwrr.add_link(link);

        dwrr.record_send(1, 1500);
        assert_eq!(dwrr.links.get(&1).unwrap().sent_bytes, 1500);

        dwrr.record_send(1, 1000);
        assert_eq!(dwrr.links.get(&1).unwrap().sent_bytes, 2500);
    }

    #[test]
    fn record_send_nonexistent_link() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        dwrr.record_send(999, 1500); // Should not panic
    }

    #[test]
    fn remove_link_resets_rr_index() {
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 5_000_000.0, LinkPhase::Live));
        let link3 = Arc::new(MockLink::new(3, 8_000_000.0, LinkPhase::Live));

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link1);
        dwrr.add_link(link2);
        dwrr.add_link(link3);

        assert_eq!(dwrr.sorted_ids.len(), 3);

        dwrr.remove_link(2);
        assert_eq!(dwrr.sorted_ids.len(), 2);
        assert!(!dwrr.sorted_ids.contains(&2));
        assert!(dwrr.current_rr_idx < dwrr.sorted_ids.len());
    }

    #[test]
    fn remove_nonexistent_link() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        dwrr.remove_link(999); // Should not panic
    }

    #[test]
    fn os_down_reduces_credits() {
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 10_000_000.0, LinkPhase::Live));

        if let Ok(mut m) = link1.metrics.lock() {
            m.os_up = Some(false);
        }

        let mut dwrr = Dwrr::new();
        dwrr.add_link(link1.clone());
        dwrr.add_link(link2.clone());
        dwrr.refresh_metrics();

        for state in dwrr.links.values_mut() {
            state.last_update -= Duration::from_secs(1);
        }

        let _ = dwrr.select_link(0);
        let link1_credits = dwrr.links.get(&1).unwrap().credits;
        let link2_credits = dwrr.links.get(&2).unwrap().credits;

        assert!(
            link2_credits > link1_credits,
            "Link with os_up=false should have fewer credits ({} vs {})",
            link1_credits,
            link2_credits
        );
    }

    #[test]
    fn get_active_links_returns_all() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::new();
        dwrr.add_link(link);

        let links = dwrr.get_active_links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, 1);
    }

    #[test]
    fn update_config_applies() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        assert!(dwrr.config().redundancy_enabled);

        let new_cfg = SchedulerConfig {
            redundancy_enabled: false,
            ..SchedulerConfig::default()
        };
        dwrr.update_config(new_cfg);
        assert!(!dwrr.config().redundancy_enabled);
    }

    // ====== AIMD Capacity Estimator Tests ======

    fn aimd_config() -> SchedulerConfig {
        SchedulerConfig {
            capacity_estimate_enabled: true,
            rtt_congestion_ratio: 1.8,
            md_factor: 0.7,
            ai_step_ratio: 0.08,
            decrease_cooldown_ms: 500,
            rtt_min_fast_window_s: 3.0,
            rtt_min_slow_window_s: 30.0,
            max_capacity_bps: 0.0,
            loss_md_threshold: 0.03,
            ..SchedulerConfig::default()
        }
    }

    /// Helper: initialize AIMD by setting traffic and running a refresh cycle.
    fn init_aimd(dwrr: &mut Dwrr<MockLink>, link_id: usize) {
        if let Some(state) = dwrr.links.get_mut(&link_id) {
            state.has_traffic = true;
            state.measured_bps = 5_000_000.0;
        }
        dwrr.refresh_metrics();
    }

    #[test]
    fn aimd_initializes_from_first_traffic() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());

        // Before traffic, AIMD should not be initialized
        dwrr.refresh_metrics();
        let state = dwrr.links.get(&1).unwrap();
        assert!(!state.aimd_initialized);
        assert_eq!(state.estimated_capacity_bps, 0.0);

        // After traffic, AIMD initializes from capacity_bps
        init_aimd(&mut dwrr, 1);
        let state = dwrr.links.get(&1).unwrap();
        assert!(state.aimd_initialized);
        assert!(state.estimated_capacity_bps > 0.0);
    }

    #[test]
    fn aimd_md_triggers_on_high_rtt_with_rising_slope() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        let initial_estimate = dwrr.links.get(&1).unwrap().estimated_capacity_bps;

        // Set RTT high enough to trigger MD (baseline is ~10ms, so 1.8× = 18ms)
        link.set_rtt(25.0);

        // Clear the min filter so the high RTT propagates immediately
        // (the min filter retains 15 samples and would mask the spike otherwise)
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.rtt_sample_filter.clear();
            state.prev_rtt_ms = 10.0;
            // Set last_decrease_at far in the past to clear cooldown
            state.last_decrease_at = Instant::now() - Duration::from_secs(10);
            // Set last_metrics_update to compute slope correctly
            state.last_metrics_update = Instant::now() - Duration::from_millis(100);
        }
        dwrr.refresh_metrics();

        let state = dwrr.links.get(&1).unwrap();
        // After MD, estimate should be reduced by md_factor (0.7)
        assert!(
            state.estimated_capacity_bps < initial_estimate,
            "MD should reduce estimated capacity: {} should be < {}",
            state.estimated_capacity_bps,
            initial_estimate
        );
    }

    #[test]
    fn aimd_md_does_not_trigger_with_stable_rtt() {
        // High RTT ratio but ZERO slope → cell handover, not congestion
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        let initial_estimate = dwrr.links.get(&1).unwrap().estimated_capacity_bps;

        // Set high RTT but with the same prev_rtt → zero slope
        link.set_rtt(25.0);
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.prev_rtt_ms = 25.0; // same as current → slope = 0
            state.last_decrease_at = Instant::now() - Duration::from_secs(10);
            state.last_metrics_update = Instant::now() - Duration::from_millis(100);
        }
        dwrr.refresh_metrics();

        let state = dwrr.links.get(&1).unwrap();
        // Should NOT have decreased (stable RTT, not congestion)
        assert!(
            state.estimated_capacity_bps >= initial_estimate,
            "Stable high RTT should not trigger MD: {} should be >= {}",
            state.estimated_capacity_bps,
            initial_estimate
        );
    }

    #[test]
    fn aimd_ai_ramps_up_when_rtt_below_headroom() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        // Set RTT low (at or below baseline → ratio ~1.0 < headroom 1.3)
        link.set_rtt(10.0);

        // Record the initial estimate
        let initial_estimate = dwrr.links.get(&1).unwrap().estimated_capacity_bps;

        // Multiple AI cycles
        for _ in 0..5 {
            if let Some(state) = dwrr.links.get_mut(&1) {
                state.measured_bps = initial_estimate * 0.5; // Using 50% → above 0.3× guard
                state.last_metrics_update = Instant::now() - Duration::from_millis(100);
            }
            dwrr.refresh_metrics();
        }

        let state = dwrr.links.get(&1).unwrap();
        assert!(
            state.estimated_capacity_bps > initial_estimate,
            "AI should increase estimated capacity: {} should be > {}",
            state.estimated_capacity_bps,
            initial_estimate
        );
    }

    #[test]
    fn aimd_hold_zone_no_change() {
        // In the NADA controller, there is no binary hold zone — the PI controller
        // always makes a gradual adjustment. Verify that a moderate RTT ratio
        // (between headroom and congestion thresholds) does NOT trigger MD, and
        // only produces a small PI-controller adjustment.
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        let initial_estimate = dwrr.links.get(&1).unwrap().estimated_capacity_bps;

        // Set RTT to be in the moderate zone: ratio ~1.5 (between 1.3 and 1.8)
        // baseline ~10ms, so 15ms → ratio 1.5
        link.set_rtt(15.0);
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.rtt_sample_filter.clear();
            state.prev_rtt_ms = 15.0; // stable
            state.last_metrics_update = Instant::now() - Duration::from_millis(100);
        }
        dwrr.refresh_metrics();

        let state = dwrr.links.get(&1).unwrap();
        // MD should NOT trigger (rtt_ratio 1.5 < 1.8 and stable slope)
        // The PI controller makes a gradual adjustment — confirm it's NOT an MD
        // (MD would decrease by exactly md_factor=0.7, i.e. 30% reduction).
        let is_md_reduction = state.estimated_capacity_bps < initial_estimate * 0.75;
        assert!(
            !is_md_reduction,
            "Moderate RTT should NOT trigger MD: estimate={} vs initial={} (ratio={:.3})",
            state.estimated_capacity_bps,
            initial_estimate,
            state.estimated_capacity_bps / initial_estimate
        );
    }

    #[test]
    fn aimd_cooldown_prevents_rapid_md() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let config = SchedulerConfig {
            decrease_cooldown_ms: 5000, // 5s cooldown
            ..aimd_config()
        };
        let mut dwrr = Dwrr::with_config(config);
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        // First MD
        link.set_rtt(25.0);
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.prev_rtt_ms = 10.0;
            state.last_decrease_at = Instant::now() - Duration::from_secs(10);
            state.last_metrics_update = Instant::now() - Duration::from_millis(100);
        }
        dwrr.refresh_metrics();
        let after_first_md = dwrr.links.get(&1).unwrap().estimated_capacity_bps;

        // Second MD attempt immediately → should NOT trigger due to cooldown
        link.set_rtt(30.0);
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.prev_rtt_ms = 25.0;
            state.last_metrics_update = Instant::now() - Duration::from_millis(100);
        }
        dwrr.refresh_metrics();
        let after_second_attempt = dwrr.links.get(&1).unwrap().estimated_capacity_bps;

        assert!(
            (after_second_attempt - after_first_md).abs() < 1.0,
            "Cooldown should prevent second MD: {} vs {}",
            after_second_attempt,
            after_first_md
        );
    }

    #[test]
    fn aimd_loss_md_triggers_on_high_loss() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        let initial_estimate = dwrr.links.get(&1).unwrap().estimated_capacity_bps;

        // Set high loss (above threshold 0.03) with stable RTT
        link.set_loss_rate(0.05);
        link.set_rtt(10.0);
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.prev_rtt_ms = 10.0;
            state.last_decrease_at = Instant::now() - Duration::from_secs(10);
            state.last_metrics_update = Instant::now() - Duration::from_millis(100);
        }
        dwrr.refresh_metrics();

        let state = dwrr.links.get(&1).unwrap();
        assert!(
            state.estimated_capacity_bps < initial_estimate,
            "Loss-based MD should reduce estimate: {} should be < {}",
            state.estimated_capacity_bps,
            initial_estimate
        );
    }

    #[test]
    fn aimd_suppresses_ai_during_probe_phase() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Probe));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        let initial_estimate = dwrr.links.get(&1).unwrap().estimated_capacity_bps;

        // Low RTT ratio → would normally trigger AI, but suppressed in Probe
        link.set_rtt(10.0);
        for _ in 0..5 {
            if let Some(state) = dwrr.links.get_mut(&1) {
                state.measured_bps = initial_estimate * 0.5;
                state.last_metrics_update = Instant::now() - Duration::from_millis(100);
            }
            dwrr.refresh_metrics();
        }

        let state = dwrr.links.get(&1).unwrap();
        // Estimate should NOT have increased during Probe
        assert!(
            state.estimated_capacity_bps <= initial_estimate + 1.0,
            "AI should be suppressed in Probe: {} should be <= {}",
            state.estimated_capacity_bps,
            initial_estimate
        );
    }

    #[test]
    fn aimd_max_capacity_clamp() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let config = SchedulerConfig {
            max_capacity_bps: 15_000_000.0,
            ..aimd_config()
        };
        let mut dwrr = Dwrr::with_config(config);
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        // Run many AI cycles to push estimate up
        link.set_rtt(10.0);
        for _ in 0..100 {
            if let Some(state) = dwrr.links.get_mut(&1) {
                state.measured_bps = state.estimated_capacity_bps * 0.5;
                state.last_metrics_update = Instant::now() - Duration::from_millis(100);
            }
            dwrr.refresh_metrics();
        }

        let state = dwrr.links.get(&1).unwrap();
        assert!(
            state.estimated_capacity_bps <= 15_000_000.0,
            "Estimate should be clamped at max_capacity_bps: {}",
            state.estimated_capacity_bps
        );
    }

    #[test]
    fn aimd_estimate_clamps_to_floor() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let config = SchedulerConfig {
            capacity_floor_bps: 500_000.0,
            decrease_cooldown_ms: 0, // Remove cooldown for test
            ..aimd_config()
        };
        let mut dwrr = Dwrr::with_config(config);
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        // Repeatedly trigger MD to drive estimate down
        for _ in 0..50 {
            link.set_rtt(30.0);
            if let Some(state) = dwrr.links.get_mut(&1) {
                state.prev_rtt_ms = 10.0;
                state.last_decrease_at = Instant::now() - Duration::from_secs(10);
                state.last_metrics_update = Instant::now() - Duration::from_millis(100);
            }
            dwrr.refresh_metrics();
        }

        let state = dwrr.links.get(&1).unwrap();
        assert!(
            state.estimated_capacity_bps >= 500_000.0,
            "Estimate should not go below capacity_floor: {}",
            state.estimated_capacity_bps
        );
    }

    #[test]
    fn aimd_disabled_keeps_zero_estimate() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let config = SchedulerConfig {
            capacity_estimate_enabled: false,
            ..SchedulerConfig::default()
        };
        let mut dwrr = Dwrr::with_config(config);
        dwrr.add_link(link.clone());

        if let Some(state) = dwrr.links.get_mut(&1) {
            state.has_traffic = true;
            state.measured_bps = 5_000_000.0;
        }
        dwrr.refresh_metrics();

        let state = dwrr.links.get(&1).unwrap();
        assert_eq!(
            state.estimated_capacity_bps, 0.0,
            "AIMD disabled should keep estimate at 0"
        );
        assert!(!state.aimd_initialized);
    }

    #[test]
    fn aimd_estimate_propagated_to_metrics() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        let metrics = dwrr.get_active_links();
        let (_, m) = metrics.iter().find(|(id, _)| *id == 1).unwrap();
        assert!(
            m.estimated_capacity_bps > 0.0,
            "estimated_capacity_bps should propagate to LinkMetrics"
        );
    }

    #[test]
    fn aimd_rtt_baseline_tracks_dual_window() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        let state = dwrr.links.get(&1).unwrap();
        assert!(
            state.rtt_baseline > 0.0,
            "RTT baseline should be set after init"
        );
        assert!(!state.rtt_min_fast_window.is_empty());
        assert!(!state.rtt_min_slow_window.is_empty());
    }

    #[test]
    fn aimd_phase_transition_resets_rtt_windows() {
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link.clone());
        init_aimd(&mut dwrr, 1);

        // Fill windows with some samples
        for _ in 0..10 {
            if let Some(state) = dwrr.links.get_mut(&1) {
                state.last_metrics_update = Instant::now() - Duration::from_millis(100);
            }
            dwrr.refresh_metrics();
        }
        let window_size_before = dwrr.links.get(&1).unwrap().rtt_min_fast_window.len();
        assert!(window_size_before > 1);

        // Transition to Probe phase → should clear windows
        link.set_phase(LinkPhase::Probe);
        dwrr.refresh_metrics();

        let state = dwrr.links.get(&1).unwrap();
        // After reset, windows should have exactly 1 new sample
        assert_eq!(
            state.rtt_min_fast_window.len(),
            1,
            "Fast window should be reset on phase transition"
        );
    }

    #[test]
    fn aimd_credit_computation_uses_estimate() {
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 10_000_000.0, LinkPhase::Live));
        let mut dwrr = Dwrr::with_config(aimd_config());
        dwrr.add_link(link1.clone());
        dwrr.add_link(link2.clone());
        init_aimd(&mut dwrr, 1);
        init_aimd(&mut dwrr, 2);

        // Reduce link1's estimated capacity via MD
        link1.set_rtt(25.0);
        if let Some(state) = dwrr.links.get_mut(&1) {
            state.rtt_sample_filter.clear(); // flush min filter so high RTT propagates
            state.prev_rtt_ms = 10.0;
            state.last_decrease_at = Instant::now() - Duration::from_secs(10);
            state.last_metrics_update = Instant::now() - Duration::from_millis(100);
        }
        dwrr.refresh_metrics();

        // Reset link1 RTT for credit computation
        link1.set_rtt(10.0);

        // Give time for credit accrual
        for state in dwrr.links.values_mut() {
            state.last_update -= Duration::from_secs(1);
        }
        let _ = dwrr.select_link(0);

        let link1_credits = dwrr.links.get(&1).unwrap().credits;
        let link2_credits = dwrr.links.get(&2).unwrap().credits;

        // Link1 with reduced AIMD estimate should get fewer credits
        assert!(
            link2_credits > link1_credits,
            "Link with lower AIMD estimate should get fewer credits: l1={} l2={}",
            link1_credits,
            link2_credits
        );
    }

    // ────────────────────────────────────────────────────────────────
    // SBD ↔ DWRR integration tests
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn sbd_wired_into_dwrr_refresh_metrics() {
        let config = SchedulerConfig {
            sbd_enabled: true,
            sbd_interval_ms: 100,
            sbd_n: 5,
            sbd_c_s: 0.05,
            sbd_c_h: 0.01,
            sbd_p_l: 0.05,
            capacity_estimate_enabled: false,
            ..SchedulerConfig::default()
        };

        let mut dwrr: Dwrr<MockLink> = Dwrr::with_config(config);
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 10_000_000.0, LinkPhase::Live));

        link1.set_owd(5.0);
        link2.set_owd(5.0);

        dwrr.add_link(link1.clone());
        dwrr.add_link(link2.clone());

        // Multiple refresh cycles to feed SBD samples
        for _ in 0..10 {
            dwrr.refresh_metrics();
            std::thread::sleep(Duration::from_millis(20));
        }

        // Skew OWD on link1 and add loss
        link1.set_owd(50.0);
        link1.set_loss_rate(0.1);
        for _ in 0..10 {
            dwrr.refresh_metrics();
            std::thread::sleep(Duration::from_millis(20));
        }

        // SBD should have produced group assignments (the code path ran)
        assert!(
            dwrr.sbd_engine.is_some(),
            "SBD engine should be instantiated when sbd_enabled=true"
        );
    }

    #[test]
    fn sbd_groups_affect_coupled_alpha() {
        let config = SchedulerConfig {
            sbd_enabled: true,
            sbd_interval_ms: 100,
            sbd_n: 5,
            capacity_estimate_enabled: true,
            ..SchedulerConfig::default()
        };

        let mut dwrr: Dwrr<MockLink> = Dwrr::with_config(config);
        let link1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let link2 = Arc::new(MockLink::new(2, 10_000_000.0, LinkPhase::Live));

        dwrr.add_link(link1.clone());
        dwrr.add_link(link2.clone());
        dwrr.mark_has_traffic(1);
        dwrr.mark_has_traffic(2);
        dwrr.refresh_metrics();

        // Manually inject SBD groups to simulate shared bottleneck
        dwrr.sbd_groups.insert(1, 1);
        dwrr.sbd_groups.insert(2, 1);
        // Refresh metrics — coupled alpha should be applied per-group
        for _ in 0..3 {
            dwrr.refresh_metrics();
        }

        // Now test with group 0 (no coupling)
        dwrr.sbd_groups.insert(1, 0);
        dwrr.sbd_groups.insert(2, 0);
        dwrr.refresh_metrics();

        // Both code paths should execute without panicking.
    }

    #[test]
    fn nada_ref_uses_estimated_capacity() {
        let config = SchedulerConfig {
            capacity_estimate_enabled: true,
            ..SchedulerConfig::default()
        };

        let mut dwrr: Dwrr<MockLink> = Dwrr::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        dwrr.add_link(link.clone());
        dwrr.mark_has_traffic(1);
        dwrr.refresh_metrics();

        // After AIMD init, estimated_capacity_bps should be set
        let metrics: HashMap<usize, LinkMetrics> = dwrr.get_active_links().into_iter().collect();
        let m = metrics.get(&1).unwrap();
        assert!(
            m.estimated_capacity_bps > 0.0,
            "estimated_capacity_bps should be set after AIMD init, got {}",
            m.estimated_capacity_bps
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Diversity-aware selection tests
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn diversity_selection_prefers_different_link_kinds() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        let l1 = Arc::new(MockLink::new(1, 50_000_000.0, LinkPhase::Live));
        let l2 = Arc::new(MockLink::new(2, 40_000_000.0, LinkPhase::Live));
        let l3 = Arc::new(MockLink::new(3, 10_000_000.0, LinkPhase::Live));
        l1.set_link_kind("wired");
        l2.set_link_kind("wired");
        l3.set_link_kind("cellular");

        dwrr.add_link(l1);
        dwrr.add_link(l2);
        dwrr.add_link(l3);
        dwrr.refresh_metrics();

        let selected = dwrr.select_best_n_links(1000, 2);
        let ids: Vec<usize> = selected.iter().map(|l| l.id()).collect();

        assert!(
            ids.contains(&1),
            "Best wired link should be selected: {:?}",
            ids
        );
        assert!(
            ids.contains(&3),
            "Cellular link should be preferred over second wired for diversity: {:?}",
            ids
        );
    }

    #[test]
    fn diversity_selection_n1_picks_best() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        let l1 = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        let l2 = Arc::new(MockLink::new(2, 5_000_000.0, LinkPhase::Live));
        l1.set_link_kind("wired");
        l2.set_link_kind("cellular");

        dwrr.add_link(l1);
        dwrr.add_link(l2);
        dwrr.refresh_metrics();

        let selected = dwrr.select_best_n_links(1000, 1);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id(), 1, "n=1 should pick highest-capacity link");
    }

    #[test]
    fn diversity_selection_n_exceeds_links() {
        let mut dwrr: Dwrr<MockLink> = Dwrr::new();
        for id in 1..=3 {
            dwrr.add_link(Arc::new(MockLink::new(id, 10_000_000.0, LinkPhase::Live)));
        }
        dwrr.refresh_metrics();

        let selected = dwrr.select_best_n_links(1000, 5);
        assert_eq!(
            selected.len(),
            3,
            "Should return all 3 links when n=5 > link_count=3"
        );
    }

    // ────────────────────────────────────────────────────────────────
    // AIMD reset tests
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn estimated_capacity_resets_on_phase_reset() {
        let config = SchedulerConfig {
            capacity_estimate_enabled: true,
            capacity_floor_bps: 1_000_000.0,
            ..SchedulerConfig::default()
        };

        let mut dwrr: Dwrr<MockLink> = Dwrr::with_config(config);
        let link = Arc::new(MockLink::new(1, 10_000_000.0, LinkPhase::Live));
        dwrr.add_link(link.clone());
        dwrr.mark_has_traffic(1);
        dwrr.refresh_metrics();

        let est_before = dwrr
            .get_active_links()
            .into_iter()
            .find(|(id, _)| *id == 1)
            .unwrap()
            .1
            .estimated_capacity_bps;
        assert!(
            est_before > 1_000_000.0,
            "Should be initialized above floor"
        );

        // Simulate link death → Reset phase
        link.set_phase(LinkPhase::Reset);
        dwrr.refresh_metrics();

        let est_after = dwrr
            .get_active_links()
            .into_iter()
            .find(|(id, _)| *id == 1)
            .unwrap()
            .1
            .estimated_capacity_bps;
        assert!(
            (est_after - 1_000_000.0).abs() < 1.0,
            "estimated_capacity should reset to floor on phase reset, got {}",
            est_after
        );
    }
}
