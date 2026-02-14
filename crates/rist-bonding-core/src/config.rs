use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;

pub const CONFIG_VERSION: u32 = 1;

/// Raw deserialized TOML configuration (pre-resolution).
///
/// All fields use `Option` to support partial overrides; defaults are
/// filled in by [`BondingConfigInput::resolve()`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BondingConfigInput {
    pub version: u32,
    pub links: Vec<LinkConfigInput>,
    pub receiver: ReceiverConfigInput,
    pub lifecycle: LinkLifecycleConfigInput,
    pub scheduler: SchedulerConfigInput,
}

/// Raw link configuration from TOML input.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinkConfigInput {
    pub id: Option<usize>,
    pub uri: String,
    pub interface: Option<String>,
    /// Optional: RIST recovery maxbitrate in kbps (defaults to 100000)
    pub recovery_maxbitrate: Option<u32>,
    /// Optional: RIST recovery RTT max in ms (defaults to 500)
    pub recovery_rtt_max: Option<u32>,
    /// Optional: RIST recovery reorder buffer in ms (defaults to 15)
    pub recovery_reorder_buffer: Option<u32>,
}

/// Raw receiver configuration from TOML input.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReceiverConfigInput {
    pub start_latency_ms: Option<u64>,
    pub buffer_capacity: Option<usize>,
    pub skip_after_ms: Option<u64>,
}

/// Raw lifecycle thresholds from TOML input.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinkLifecycleConfigInput {
    pub good_loss_rate_max: Option<f64>,
    pub good_rtt_ms_min: Option<f64>,
    pub good_capacity_bps_min: Option<f64>,
    pub stats_fresh_ms: Option<u64>,
    pub stats_stale_ms: Option<u64>,
    pub probe_to_warm_good: Option<u32>,
    pub warm_to_live_good: Option<u32>,
    pub warm_to_degrade_bad: Option<u32>,
    pub live_to_degrade_bad: Option<u32>,
    pub degrade_to_warm_good: Option<u32>,
    pub degrade_to_cooldown_bad: Option<u32>,
    pub cooldown_ms: Option<u64>,
}

/// Raw scheduler tuning parameters from TOML input.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerConfigInput {
    /// Master toggle for adaptive packet duplication
    pub redundancy_enabled: Option<bool>,
    /// Spare capacity ratio threshold to trigger duplication (0.0-1.0)
    pub redundancy_spare_ratio: Option<f64>,
    /// Max packet size in bytes eligible for duplication
    pub redundancy_max_packet_bytes: Option<usize>,
    /// Number of links to duplicate to
    pub redundancy_target_links: Option<usize>,
    /// Whether critical packets (keyframes) broadcast to all alive links
    pub critical_broadcast: Option<bool>,
    /// Master toggle for fast-failover mode
    pub failover_enabled: Option<bool>,
    /// Duration of failover broadcast after trigger (ms)
    pub failover_duration_ms: Option<u64>,
    /// RTT multiple to trigger failover
    pub failover_rtt_spike_factor: Option<f64>,
    /// EWMA smoothing factor for link stats (0.0-1.0)
    pub ewma_alpha: Option<f64>,
    /// How far ahead to predict link trends (seconds)
    pub prediction_horizon_s: Option<f64>,
    /// Bootstrap floor for links with unknown capacity (bps)
    pub capacity_floor_bps: Option<f64>,
    /// Penalty factor multiplier on capacity drops (0.0-1.0)
    pub penalty_decay: Option<f64>,
    /// Penalty factor recovery per refresh (0.0-1.0)
    pub penalty_recovery: Option<f64>,
    /// Multiplier for p95 jitter in adaptive latency
    pub jitter_latency_multiplier: Option<f64>,
    /// Hard ceiling on adaptive reassembly latency (ms)
    pub max_latency_ms: Option<u64>,
    /// Stats emission interval for GStreamer bus messages (ms)
    pub stats_interval_ms: Option<u64>,
    /// Runtime packet channel depth
    pub channel_capacity: Option<usize>,

    // --- AIMD Capacity Estimator knobs ---
    /// Master toggle for delay-gradient AIMD capacity estimation
    pub capacity_estimate_enabled: Option<bool>,
    /// RTT / baseline ratio that triggers multiplicative decrease (e.g. 1.8)
    pub rtt_congestion_ratio: Option<f64>,
    /// Multiplicative decrease factor (0.0-1.0, e.g. 0.7)
    pub md_factor: Option<f64>,
    /// Additive increase step as fraction of estimated capacity (e.g. 0.08)
    pub ai_step_ratio: Option<f64>,
    /// Minimum time between consecutive multiplicative decreases (ms)
    pub decrease_cooldown_ms: Option<u64>,
    /// Fast sliding window duration for baseline RTT (seconds)
    pub rtt_min_fast_window_s: Option<f64>,
    /// Slow sliding window duration for baseline RTT (seconds)
    pub rtt_min_slow_window_s: Option<f64>,
    /// Per-link hard ceiling on estimated capacity (0 = disabled)
    pub max_capacity_bps: Option<f64>,
    /// Sustained loss rate threshold triggering independent MD (0.0-1.0)
    pub loss_md_threshold: Option<f64>,

    // --- NADA / RFC 8698 knobs ---
    /// Reference delay penalty for packet loss at PLRREF (ms) (RFC 8698 §4.2)
    pub dloss_ref_ms: Option<f64>,
    /// Reference packet loss ratio for unified signal (RFC 8698 §4.2)
    pub plr_ref: Option<f64>,
    /// Maximum rate increase multiplier in accelerated ramp-up (RFC 8698 §4.3)
    pub gamma_max: Option<f64>,
    /// Upper bound on self-inflicted queuing delay during ramp-up (ms) (RFC 8698 §4.3)
    pub qbound_ms: Option<f64>,
    /// Queuing delay threshold for determining underutilization (ms) (RFC 8698 §4.2)
    pub qeps_ms: Option<f64>,
    /// PI-controller scaling parameter (RFC 8698 §4.3)
    pub nada_kappa: Option<f64>,
    /// PI-controller scaling parameter for proportional term (RFC 8698 §4.3)
    pub nada_eta: Option<f64>,
    /// PI-controller upper bound of RTT (ms) (RFC 8698 §4.3)
    pub nada_tau_ms: Option<f64>,
    /// Reference congestion level (ms) (RFC 8698 §4.3)
    pub nada_xref_ms: Option<f64>,
    /// Priority weight for the flow (RFC 8698 §4.1)
    pub nada_prio: Option<f64>,

    // --- SBD / RFC 8382 knobs ---
    /// Enable shared bottleneck detection (RFC 8382)
    pub sbd_enabled: Option<bool>,
    /// Base measurement interval for SBD statistics (ms) (RFC 8382 §2)
    pub sbd_interval_ms: Option<u64>,
    /// Number of base intervals for SBD calculations (RFC 8382 §2)
    pub sbd_n: Option<usize>,
    /// Skewness threshold for bottleneck detection (RFC 8382 §2)
    pub sbd_c_s: Option<f64>,
    /// Hysteresis threshold for bottleneck detection (RFC 8382 §2)
    pub sbd_c_h: Option<f64>,
    /// Loss threshold for SBD supplementary signal (RFC 8382 §2)
    pub sbd_p_l: Option<f64>,
}

/// Resolved link configuration with concrete values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkConfig {
    pub id: usize,
    pub uri: String,
    pub interface: Option<String>,
    pub recovery_maxbitrate: Option<u32>,
    pub recovery_rtt_max: Option<u32>,
    pub recovery_reorder_buffer: Option<u32>,
}

/// Resolved receiver configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverConfig {
    pub start_latency: Duration,
    pub buffer_capacity: usize,
    pub skip_after: Option<Duration>,
}

/// Resolved link lifecycle state-machine thresholds.
///
/// Controls phase transitions (Probe→Warm→Live→Degrade→Cooldown) based
/// on consecutive good/bad stats observations and timeout durations.
#[derive(Debug, Clone)]
pub struct LinkLifecycleConfig {
    pub good_loss_rate_max: f64,
    pub good_rtt_ms_min: f64,
    pub good_capacity_bps_min: f64,
    pub stats_fresh_ms: u64,
    pub stats_stale_ms: u64,
    pub probe_to_warm_good: u32,
    pub warm_to_live_good: u32,
    pub warm_to_degrade_bad: u32,
    pub live_to_degrade_bad: u32,
    pub degrade_to_warm_good: u32,
    pub degrade_to_cooldown_bad: u32,
    pub cooldown_ms: u64,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            start_latency: Duration::from_millis(50),
            buffer_capacity: 2048,
            skip_after: None,
        }
    }
}

impl Default for LinkLifecycleConfig {
    fn default() -> Self {
        Self {
            good_loss_rate_max: 0.2,
            good_rtt_ms_min: 1.0,
            good_capacity_bps_min: 1.0,
            stats_fresh_ms: 1500,
            stats_stale_ms: 3000,
            probe_to_warm_good: 3,
            warm_to_live_good: 10,
            warm_to_degrade_bad: 3,
            live_to_degrade_bad: 3,
            degrade_to_warm_good: 5,
            degrade_to_cooldown_bad: 10,
            cooldown_ms: 2000,
        }
    }
}

/// Resolved scheduler tuning parameters.
///
/// Controls DWRR credit computation, redundancy, failover, congestion
/// feedback, and EWMA smoothing across the bonding scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub redundancy_enabled: bool,
    pub redundancy_spare_ratio: f64,
    pub redundancy_max_packet_bytes: usize,
    pub redundancy_target_links: usize,
    pub critical_broadcast: bool,
    pub failover_enabled: bool,
    pub failover_duration_ms: u64,
    pub failover_rtt_spike_factor: f64,
    pub ewma_alpha: f64,
    pub prediction_horizon_s: f64,
    pub capacity_floor_bps: f64,
    pub penalty_decay: f64,
    pub penalty_recovery: f64,
    pub jitter_latency_multiplier: f64,
    pub max_latency_ms: u64,
    pub stats_interval_ms: u64,
    pub channel_capacity: usize,

    // --- AIMD Capacity Estimator ---
    /// Master toggle for delay-gradient AIMD capacity estimation.
    pub capacity_estimate_enabled: bool,
    /// RTT / baseline ratio that triggers multiplicative decrease.
    pub rtt_congestion_ratio: f64,
    /// Multiplicative decrease factor.
    pub md_factor: f64,
    /// Additive increase step as fraction of estimated capacity.
    pub ai_step_ratio: f64,
    /// Minimum time between consecutive multiplicative decreases (ms).
    pub decrease_cooldown_ms: u64,
    /// Fast sliding window duration for baseline RTT (seconds).
    pub rtt_min_fast_window_s: f64,
    /// Slow sliding window duration for baseline RTT (seconds).
    pub rtt_min_slow_window_s: f64,
    /// Per-link hard ceiling on estimated capacity (0 = disabled).
    pub max_capacity_bps: f64,
    /// Sustained loss rate threshold triggering independent MD.
    pub loss_md_threshold: f64,

    // --- NADA / RFC 8698 ---
    /// Reference delay penalty for loss at `plr_ref` (ms).
    pub dloss_ref_ms: f64,
    /// Reference packet loss ratio for unified congestion signal.
    pub plr_ref: f64,
    /// Maximum rate-increase multiplier in accelerated ramp-up mode.
    pub gamma_max: f64,
    /// Upper bound on self-inflicted queuing delay during ramp-up (ms).
    pub qbound_ms: f64,
    /// Queuing delay threshold below which path is underutilized (ms).
    pub qeps_ms: f64,
    /// PI-controller scaling parameter (overall speed).
    pub nada_kappa: f64,
    /// PI-controller proportional term weight.
    pub nada_eta: f64,
    /// PI-controller upper-bound of RTT (ms).
    pub nada_tau_ms: f64,
    /// Reference congestion level (ms) — equilibrium target.
    pub nada_xref_ms: f64,
    /// Priority weight of the flow.
    pub nada_prio: f64,

    // --- SBD / RFC 8382 ---
    /// Enable shared bottleneck detection.
    pub sbd_enabled: bool,
    /// Base measurement interval for SBD statistics (ms).
    pub sbd_interval_ms: u64,
    /// Number of base intervals for SBD calculations.
    pub sbd_n: usize,
    /// Skewness threshold for bottleneck detection.
    pub sbd_c_s: f64,
    /// Hysteresis threshold for bottleneck detection.
    pub sbd_c_h: f64,
    /// Loss threshold for SBD supplementary signal.
    pub sbd_p_l: f64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            redundancy_enabled: true,
            redundancy_spare_ratio: 0.5,
            redundancy_max_packet_bytes: 10_000,
            redundancy_target_links: 2,
            critical_broadcast: true,
            failover_enabled: true,
            failover_duration_ms: 3000,
            failover_rtt_spike_factor: 3.0,
            ewma_alpha: 0.125,
            prediction_horizon_s: 0.5,
            capacity_floor_bps: 1_000_000.0,
            penalty_decay: 0.7,
            penalty_recovery: 0.05,
            jitter_latency_multiplier: 4.0,
            max_latency_ms: 500,
            stats_interval_ms: 1000,
            channel_capacity: 1000,
            // AIMD defaults
            capacity_estimate_enabled: true,
            rtt_congestion_ratio: 1.8,
            md_factor: 0.7,
            ai_step_ratio: 0.08,
            decrease_cooldown_ms: 500,
            rtt_min_fast_window_s: 3.0,
            rtt_min_slow_window_s: 30.0,
            max_capacity_bps: 0.0,
            loss_md_threshold: 0.03,
            // NADA defaults (RFC 8698)
            dloss_ref_ms: 10.0,
            plr_ref: 0.01,
            gamma_max: 0.5,
            qbound_ms: 50.0,
            qeps_ms: 10.0,
            nada_kappa: 0.5,
            nada_eta: 2.0,
            nada_tau_ms: 500.0,
            nada_xref_ms: 10.0,
            nada_prio: 1.0,
            // SBD defaults (RFC 8382)
            sbd_enabled: false,
            sbd_interval_ms: 350,
            sbd_n: 50,
            sbd_c_s: 0.1,
            sbd_c_h: 0.3,
            sbd_p_l: 0.1,
        }
    }
}

/// Fully resolved bonding configuration ready for use by the runtime.
#[derive(Debug, Clone)]
pub struct BondingConfig {
    pub version: u32,
    pub links: Vec<LinkConfig>,
    pub receiver: ReceiverConfig,
    pub lifecycle: LinkLifecycleConfig,
    pub scheduler: SchedulerConfig,
}

impl Default for BondingConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            links: Vec::new(),
            receiver: ReceiverConfig::default(),
            lifecycle: LinkLifecycleConfig::default(),
            scheduler: SchedulerConfig::default(),
        }
    }
}

impl LinkLifecycleConfigInput {
    pub fn resolve(self) -> LinkLifecycleConfig {
        let defaults = LinkLifecycleConfig::default();
        LinkLifecycleConfig {
            good_loss_rate_max: self
                .good_loss_rate_max
                .unwrap_or(defaults.good_loss_rate_max)
                .clamp(0.0, 1.0),
            good_rtt_ms_min: self.good_rtt_ms_min.unwrap_or(defaults.good_rtt_ms_min),
            good_capacity_bps_min: self
                .good_capacity_bps_min
                .unwrap_or(defaults.good_capacity_bps_min),
            stats_fresh_ms: self.stats_fresh_ms.unwrap_or(defaults.stats_fresh_ms),
            stats_stale_ms: self.stats_stale_ms.unwrap_or(defaults.stats_stale_ms),
            probe_to_warm_good: self
                .probe_to_warm_good
                .unwrap_or(defaults.probe_to_warm_good),
            warm_to_live_good: self.warm_to_live_good.unwrap_or(defaults.warm_to_live_good),
            warm_to_degrade_bad: self
                .warm_to_degrade_bad
                .unwrap_or(defaults.warm_to_degrade_bad),
            live_to_degrade_bad: self
                .live_to_degrade_bad
                .unwrap_or(defaults.live_to_degrade_bad),
            degrade_to_warm_good: self
                .degrade_to_warm_good
                .unwrap_or(defaults.degrade_to_warm_good),
            degrade_to_cooldown_bad: self
                .degrade_to_cooldown_bad
                .unwrap_or(defaults.degrade_to_cooldown_bad),
            cooldown_ms: self.cooldown_ms.unwrap_or(defaults.cooldown_ms),
        }
    }
}

impl SchedulerConfigInput {
    pub fn resolve(self) -> SchedulerConfig {
        let defaults = SchedulerConfig::default();
        SchedulerConfig {
            redundancy_enabled: self
                .redundancy_enabled
                .unwrap_or(defaults.redundancy_enabled),
            redundancy_spare_ratio: self
                .redundancy_spare_ratio
                .unwrap_or(defaults.redundancy_spare_ratio)
                .clamp(0.0, 1.0),
            redundancy_max_packet_bytes: self
                .redundancy_max_packet_bytes
                .unwrap_or(defaults.redundancy_max_packet_bytes),
            redundancy_target_links: self
                .redundancy_target_links
                .unwrap_or(defaults.redundancy_target_links)
                .max(1),
            critical_broadcast: self
                .critical_broadcast
                .unwrap_or(defaults.critical_broadcast),
            failover_enabled: self.failover_enabled.unwrap_or(defaults.failover_enabled),
            failover_duration_ms: self
                .failover_duration_ms
                .unwrap_or(defaults.failover_duration_ms),
            failover_rtt_spike_factor: self
                .failover_rtt_spike_factor
                .unwrap_or(defaults.failover_rtt_spike_factor)
                .clamp(1.0, 100.0),
            ewma_alpha: self
                .ewma_alpha
                .unwrap_or(defaults.ewma_alpha)
                .clamp(0.001, 1.0),
            prediction_horizon_s: self
                .prediction_horizon_s
                .unwrap_or(defaults.prediction_horizon_s),
            capacity_floor_bps: self
                .capacity_floor_bps
                .unwrap_or(defaults.capacity_floor_bps),
            penalty_decay: self
                .penalty_decay
                .unwrap_or(defaults.penalty_decay)
                .clamp(0.0, 1.0),
            penalty_recovery: self
                .penalty_recovery
                .unwrap_or(defaults.penalty_recovery)
                .clamp(0.0, 1.0),
            jitter_latency_multiplier: self
                .jitter_latency_multiplier
                .unwrap_or(defaults.jitter_latency_multiplier)
                .clamp(0.0, 100.0),
            max_latency_ms: self.max_latency_ms.unwrap_or(defaults.max_latency_ms),
            stats_interval_ms: self
                .stats_interval_ms
                .unwrap_or(defaults.stats_interval_ms)
                .max(100),
            channel_capacity: self
                .channel_capacity
                .unwrap_or(defaults.channel_capacity)
                .max(16),
            // AIMD knobs
            capacity_estimate_enabled: self
                .capacity_estimate_enabled
                .unwrap_or(defaults.capacity_estimate_enabled),
            rtt_congestion_ratio: self
                .rtt_congestion_ratio
                .unwrap_or(defaults.rtt_congestion_ratio)
                .clamp(1.0, 10.0),
            md_factor: self.md_factor.unwrap_or(defaults.md_factor).clamp(0.1, 1.0),
            ai_step_ratio: self
                .ai_step_ratio
                .unwrap_or(defaults.ai_step_ratio)
                .clamp(0.001, 1.0),
            decrease_cooldown_ms: self
                .decrease_cooldown_ms
                .unwrap_or(defaults.decrease_cooldown_ms)
                .max(50),
            rtt_min_fast_window_s: self
                .rtt_min_fast_window_s
                .unwrap_or(defaults.rtt_min_fast_window_s)
                .clamp(0.5, 60.0),
            rtt_min_slow_window_s: self
                .rtt_min_slow_window_s
                .unwrap_or(defaults.rtt_min_slow_window_s)
                .clamp(5.0, 300.0),
            max_capacity_bps: self
                .max_capacity_bps
                .unwrap_or(defaults.max_capacity_bps)
                .max(0.0),
            loss_md_threshold: self
                .loss_md_threshold
                .unwrap_or(defaults.loss_md_threshold)
                .clamp(0.0, 1.0),
            // NADA knobs
            dloss_ref_ms: self
                .dloss_ref_ms
                .unwrap_or(defaults.dloss_ref_ms)
                .clamp(0.0, 1000.0),
            plr_ref: self.plr_ref.unwrap_or(defaults.plr_ref).clamp(0.0001, 1.0),
            gamma_max: self
                .gamma_max
                .unwrap_or(defaults.gamma_max)
                .clamp(0.01, 2.0),
            qbound_ms: self
                .qbound_ms
                .unwrap_or(defaults.qbound_ms)
                .clamp(1.0, 500.0),
            qeps_ms: self.qeps_ms.unwrap_or(defaults.qeps_ms).clamp(0.0, 200.0),
            nada_kappa: self
                .nada_kappa
                .unwrap_or(defaults.nada_kappa)
                .clamp(0.01, 5.0),
            nada_eta: self.nada_eta.unwrap_or(defaults.nada_eta).clamp(0.1, 10.0),
            nada_tau_ms: self
                .nada_tau_ms
                .unwrap_or(defaults.nada_tau_ms)
                .clamp(50.0, 5000.0),
            nada_xref_ms: self
                .nada_xref_ms
                .unwrap_or(defaults.nada_xref_ms)
                .clamp(1.0, 500.0),
            nada_prio: self
                .nada_prio
                .unwrap_or(defaults.nada_prio)
                .clamp(0.1, 10.0),
            // SBD knobs
            sbd_enabled: self.sbd_enabled.unwrap_or(defaults.sbd_enabled),
            sbd_interval_ms: self
                .sbd_interval_ms
                .unwrap_or(defaults.sbd_interval_ms)
                .clamp(100, 2000),
            sbd_n: self.sbd_n.unwrap_or(defaults.sbd_n).clamp(5, 200),
            sbd_c_s: self.sbd_c_s.unwrap_or(defaults.sbd_c_s).clamp(-1.0, 1.0),
            sbd_c_h: self.sbd_c_h.unwrap_or(defaults.sbd_c_h).clamp(-1.0, 1.0),
            sbd_p_l: self.sbd_p_l.unwrap_or(defaults.sbd_p_l).clamp(0.0, 1.0),
        }
    }
}

impl BondingConfigInput {
    pub fn resolve(self) -> Result<BondingConfig, String> {
        let version = if self.version == 0 {
            CONFIG_VERSION
        } else {
            self.version
        };
        if version != CONFIG_VERSION {
            return Err(format!("Unsupported config version {}", version));
        }

        let receiver = ReceiverConfig {
            start_latency: Duration::from_millis(
                self.receiver
                    .start_latency_ms
                    .unwrap_or(ReceiverConfig::default().start_latency.as_millis() as u64),
            ),
            buffer_capacity: self
                .receiver
                .buffer_capacity
                .unwrap_or(ReceiverConfig::default().buffer_capacity)
                .max(16),
            skip_after: self.receiver.skip_after_ms.map(Duration::from_millis),
        };

        let lifecycle = self.lifecycle.resolve();
        let scheduler = self.scheduler.resolve();

        let mut out = Vec::new();
        let mut seen_ids = HashSet::new();
        for (idx, link) in self.links.into_iter().enumerate() {
            let id = link.id.unwrap_or(idx);
            if !seen_ids.insert(id) {
                continue;
            }
            let iface = link.interface.filter(|s| !s.is_empty());
            out.push(LinkConfig {
                id,
                uri: link.uri,
                interface: iface,
                recovery_maxbitrate: link.recovery_maxbitrate,
                recovery_rtt_max: link.recovery_rtt_max,
                recovery_reorder_buffer: link.recovery_reorder_buffer,
            });
        }

        Ok(BondingConfig {
            version,
            links: out,
            receiver,
            lifecycle,
            scheduler,
        })
    }
}

impl BondingConfig {
    pub fn from_toml_str(input: &str) -> Result<Self, String> {
        if input.trim().is_empty() {
            return Ok(BondingConfig::default());
        }
        let parsed: BondingConfigInput =
            toml::from_str(input).map_err(|e| format!("Invalid config TOML: {}", e))?;
        parsed.resolve()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_toml_config_basic() {
        let toml = r#"
            version = 1

            [[links]]
            id = 10
            uri = "rist://1.2.3.4:5000"

            [[links]]
            uri = "rist://5.6.7.8:5000"
            interface = "eth0"

            [receiver]
            start_latency_ms = 80
            buffer_capacity = 1024
            skip_after_ms = 40

            [lifecycle]
            good_loss_rate_max = 0.15
            stats_fresh_ms = 1200
            cooldown_ms = 1500
        "#;

        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.links.len(), 2);
        assert_eq!(cfg.links[0].id, 10);
        assert_eq!(cfg.links[0].uri, "rist://1.2.3.4:5000");
        assert!(cfg.links[0].interface.is_none());
        assert_eq!(cfg.links[1].id, 1);
        assert_eq!(cfg.links[1].interface.as_deref(), Some("eth0"));
        assert_eq!(cfg.receiver.start_latency, Duration::from_millis(80));
        assert_eq!(cfg.receiver.buffer_capacity, 1024);
        assert_eq!(cfg.receiver.skip_after, Some(Duration::from_millis(40)));
        assert_eq!(cfg.lifecycle.good_loss_rate_max, 0.15);
        assert_eq!(cfg.lifecycle.stats_fresh_ms, 1200);
        assert_eq!(cfg.lifecycle.cooldown_ms, 1500);
    }

    #[test]
    fn parse_toml_config_dedup() {
        let toml = r#"
            version = 1
            [[links]]
            id = 1
            uri = "a"
            [[links]]
            id = 1
            uri = "b"
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.links.len(), 1);
        assert_eq!(cfg.links[0].uri, "a");
    }

    #[test]
    fn parse_toml_config_version_zero_defaults() {
        let toml = r#"
            version = 0
            [[links]]
            uri = "rist://1.2.3.4:5000"
        "#;

        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(cfg.links.len(), 1);
        assert_eq!(cfg.links[0].uri, "rist://1.2.3.4:5000");
    }

    #[test]
    fn parse_toml_config_empty_defaults() {
        let cfg = BondingConfig::from_toml_str("").unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.links.is_empty());
        assert_eq!(cfg.receiver, ReceiverConfig::default());
        assert_eq!(
            cfg.lifecycle.good_loss_rate_max,
            LinkLifecycleConfig::default().good_loss_rate_max
        );
    }

    #[test]
    fn parse_toml_config_with_recovery_params() {
        let toml = r#"
            version = 1

            [[links]]
            id = 1
            uri = "rist://10.0.0.1:5000"
            interface = "wwan0"
            recovery_maxbitrate = 20000
            recovery_rtt_max = 800
            recovery_reorder_buffer = 50

            [[links]]
            id = 2
            uri = "rist://10.0.0.2:5000"
            interface = "wlan0"
            recovery_rtt_max = 200
        "#;

        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.links.len(), 2);

        // Link 1 with all recovery params
        assert_eq!(cfg.links[0].id, 1);
        assert_eq!(cfg.links[0].recovery_maxbitrate, Some(20000));
        assert_eq!(cfg.links[0].recovery_rtt_max, Some(800));
        assert_eq!(cfg.links[0].recovery_reorder_buffer, Some(50));

        // Link 2 with partial recovery params
        assert_eq!(cfg.links[1].id, 2);
        assert_eq!(cfg.links[1].recovery_maxbitrate, None);
        assert_eq!(cfg.links[1].recovery_rtt_max, Some(200));
        assert_eq!(cfg.links[1].recovery_reorder_buffer, None);
    }

    #[test]
    fn parse_toml_config_without_recovery_params() {
        let toml = r#"
            version = 1

            [[links]]
            id = 1
            uri = "rist://10.0.0.1:5000"
        "#;

        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.links.len(), 1);

        // Recovery params should be None when not specified
        assert_eq!(cfg.links[0].recovery_maxbitrate, None);
        assert_eq!(cfg.links[0].recovery_rtt_max, None);
        assert_eq!(cfg.links[0].recovery_reorder_buffer, None);
    }

    #[test]
    fn parse_toml_scheduler_config() {
        let toml = r#"
            version = 1

            [[links]]
            uri = "rist://10.0.0.1:5000"

            [scheduler]
            redundancy_enabled = false
            redundancy_spare_ratio = 0.6
            redundancy_max_packet_bytes = 8000
            redundancy_target_links = 3
            critical_broadcast = false
            failover_enabled = false
            failover_duration_ms = 5000
            failover_rtt_spike_factor = 4.0
            ewma_alpha = 0.2
            prediction_horizon_s = 1.0
            capacity_floor_bps = 2000000.0
            penalty_decay = 0.5
            penalty_recovery = 0.1
            jitter_latency_multiplier = 3.0
            max_latency_ms = 300
            stats_interval_ms = 500
            channel_capacity = 2000
        "#;

        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!(!cfg.scheduler.redundancy_enabled);
        assert!((cfg.scheduler.redundancy_spare_ratio - 0.6).abs() < 1e-6);
        assert_eq!(cfg.scheduler.redundancy_max_packet_bytes, 8000);
        assert_eq!(cfg.scheduler.redundancy_target_links, 3);
        assert!(!cfg.scheduler.critical_broadcast);
        assert!(!cfg.scheduler.failover_enabled);
        assert_eq!(cfg.scheduler.failover_duration_ms, 5000);
        assert!((cfg.scheduler.failover_rtt_spike_factor - 4.0).abs() < 1e-6);
        assert!((cfg.scheduler.ewma_alpha - 0.2).abs() < 1e-6);
        assert!((cfg.scheduler.prediction_horizon_s - 1.0).abs() < 1e-6);
        assert!((cfg.scheduler.capacity_floor_bps - 2_000_000.0).abs() < 1e-6);
        assert!((cfg.scheduler.penalty_decay - 0.5).abs() < 1e-6);
        assert!((cfg.scheduler.penalty_recovery - 0.1).abs() < 1e-6);
        assert!((cfg.scheduler.jitter_latency_multiplier - 3.0).abs() < 1e-6);
        assert_eq!(cfg.scheduler.max_latency_ms, 300);
        assert_eq!(cfg.scheduler.stats_interval_ms, 500);
        assert_eq!(cfg.scheduler.channel_capacity, 2000);
    }

    #[test]
    fn parse_toml_scheduler_defaults() {
        let toml = r#"
            version = 1
            [[links]]
            uri = "rist://10.0.0.1:5000"
        "#;

        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        let defaults = SchedulerConfig::default();
        assert_eq!(
            cfg.scheduler.redundancy_enabled,
            defaults.redundancy_enabled
        );
        assert_eq!(cfg.scheduler.failover_enabled, defaults.failover_enabled);
        assert_eq!(cfg.scheduler.channel_capacity, defaults.channel_capacity);
        assert!((cfg.scheduler.ewma_alpha - defaults.ewma_alpha).abs() < 1e-6);
    }

    #[test]
    fn parse_toml_config_invalid_syntax() {
        let bad_toml = r#"
            version = 1
            [[links
        "#;
        let result = BondingConfig::from_toml_str(bad_toml);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("Invalid config TOML"),
            "Error should mention invalid TOML, got: {}",
            msg
        );
    }

    #[test]
    fn parse_toml_config_unsupported_version() {
        let toml = r#"
            version = 99
            [[links]]
            uri = "rist://1.2.3.4:5000"
        "#;
        let result = BondingConfig::from_toml_str(toml);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("Unsupported config version"),
            "Expected unsupported version error, got: {}",
            msg
        );
    }

    #[test]
    fn ewma_alpha_clamped_to_minimum() {
        let toml = r#"
            version = 1
            [scheduler]
            ewma_alpha = 0.0
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!(
            (cfg.scheduler.ewma_alpha - 0.001).abs() < 1e-6,
            "ewma_alpha should be clamped to 0.001, got: {}",
            cfg.scheduler.ewma_alpha
        );
    }

    #[test]
    fn ewma_alpha_clamped_to_maximum() {
        let toml = r#"
            version = 1
            [scheduler]
            ewma_alpha = 5.0
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!(
            (cfg.scheduler.ewma_alpha - 1.0).abs() < 1e-6,
            "ewma_alpha should be clamped to 1.0, got: {}",
            cfg.scheduler.ewma_alpha
        );
    }

    #[test]
    fn channel_capacity_clamped_to_minimum() {
        let toml = r#"
            version = 1
            [scheduler]
            channel_capacity = 0
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.scheduler.channel_capacity, 16);
    }

    #[test]
    fn buffer_capacity_clamped_to_minimum() {
        let toml = r#"
            version = 1
            [receiver]
            buffer_capacity = 1
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.receiver.buffer_capacity, 16);
    }

    #[test]
    fn stats_interval_clamped_to_minimum() {
        let toml = r#"
            version = 1
            [scheduler]
            stats_interval_ms = 10
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.scheduler.stats_interval_ms, 100);
    }

    #[test]
    fn penalty_decay_clamped() {
        let toml = r#"
            version = 1
            [scheduler]
            penalty_decay = -0.5
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.scheduler.penalty_decay - 0.0).abs() < 1e-6);

        let toml2 = r#"
            version = 1
            [scheduler]
            penalty_decay = 2.0
        "#;
        let cfg2 = BondingConfig::from_toml_str(toml2).unwrap();
        assert!((cfg2.scheduler.penalty_decay - 1.0).abs() < 1e-6);
    }

    #[test]
    fn redundancy_target_links_clamped_to_one() {
        let toml = r#"
            version = 1
            [scheduler]
            redundancy_target_links = 0
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.scheduler.redundancy_target_links, 1);
    }

    #[test]
    fn link_with_empty_interface_becomes_none() {
        let toml = r#"
            version = 1
            [[links]]
            id = 1
            uri = "rist://1.2.3.4:5000"
            interface = ""
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!(cfg.links[0].interface.is_none());
    }

    #[test]
    fn link_ids_auto_assigned_from_index() {
        let toml = r#"
            version = 1
            [[links]]
            uri = "rist://1.2.3.4:5000"
            [[links]]
            uri = "rist://5.6.7.8:5000"
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.links[0].id, 0);
        assert_eq!(cfg.links[1].id, 1);
    }

    #[test]
    fn unknown_toml_keys_rejected() {
        let toml = r#"
            version = 1
            [scheduler]
            ewma_alpha = 0.2
            typo_field = 42
        "#;
        let result = BondingConfig::from_toml_str(toml);
        assert!(result.is_err(), "Unknown keys should be rejected");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("typo_field"),
            "Error should mention the unknown field, got: {}",
            msg
        );
    }

    #[test]
    fn redundancy_spare_ratio_clamped() {
        let toml = r#"
            version = 1
            [scheduler]
            redundancy_spare_ratio = 1.5
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.scheduler.redundancy_spare_ratio - 1.0).abs() < 1e-6);

        let toml2 = r#"
            version = 1
            [scheduler]
            redundancy_spare_ratio = -0.5
        "#;
        let cfg2 = BondingConfig::from_toml_str(toml2).unwrap();
        assert!((cfg2.scheduler.redundancy_spare_ratio - 0.0).abs() < 1e-6);
    }

    #[test]
    fn failover_rtt_spike_factor_clamped() {
        let toml = r#"
            version = 1
            [scheduler]
            failover_rtt_spike_factor = 0.5
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.scheduler.failover_rtt_spike_factor - 1.0).abs() < 1e-6);
    }

    #[test]
    fn jitter_latency_multiplier_clamped() {
        let toml = r#"
            version = 1
            [scheduler]
            jitter_latency_multiplier = -2.0
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.scheduler.jitter_latency_multiplier - 0.0).abs() < 1e-6);
    }

    #[test]
    fn good_loss_rate_max_clamped() {
        let toml = r#"
            version = 1
            [lifecycle]
            good_loss_rate_max = 5.0
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.lifecycle.good_loss_rate_max - 1.0).abs() < 1e-6);
    }

    // ====== AIMD Config Tests ======

    #[test]
    fn aimd_config_defaults() {
        let cfg = BondingConfig::from_toml_str("").unwrap();
        assert!(cfg.scheduler.capacity_estimate_enabled);
        assert!((cfg.scheduler.rtt_congestion_ratio - 1.8).abs() < 1e-6);
        assert!((cfg.scheduler.md_factor - 0.7).abs() < 1e-6);
        assert!((cfg.scheduler.ai_step_ratio - 0.08).abs() < 1e-6);
        assert_eq!(cfg.scheduler.decrease_cooldown_ms, 500);
        assert!((cfg.scheduler.rtt_min_fast_window_s - 3.0).abs() < 1e-6);
        assert!((cfg.scheduler.rtt_min_slow_window_s - 30.0).abs() < 1e-6);
        assert!((cfg.scheduler.max_capacity_bps - 0.0).abs() < 1e-6);
        assert!((cfg.scheduler.loss_md_threshold - 0.03).abs() < 1e-6);
    }

    #[test]
    fn aimd_config_custom_values() {
        let toml = r#"
            version = 1
            [scheduler]
            capacity_estimate_enabled = false
            rtt_congestion_ratio = 2.5
            md_factor = 0.5
            ai_step_ratio = 0.1
            decrease_cooldown_ms = 1000
            rtt_min_fast_window_s = 5.0
            rtt_min_slow_window_s = 60.0
            max_capacity_bps = 150000000.0
            loss_md_threshold = 0.05
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!(!cfg.scheduler.capacity_estimate_enabled);
        assert!((cfg.scheduler.rtt_congestion_ratio - 2.5).abs() < 1e-6);
        assert!((cfg.scheduler.md_factor - 0.5).abs() < 1e-6);
        assert!((cfg.scheduler.ai_step_ratio - 0.1).abs() < 1e-6);
        assert_eq!(cfg.scheduler.decrease_cooldown_ms, 1000);
        assert!((cfg.scheduler.rtt_min_fast_window_s - 5.0).abs() < 1e-6);
        assert!((cfg.scheduler.rtt_min_slow_window_s - 60.0).abs() < 1e-6);
        assert!((cfg.scheduler.max_capacity_bps - 150_000_000.0).abs() < 1e-3);
        assert!((cfg.scheduler.loss_md_threshold - 0.05).abs() < 1e-6);
    }

    #[test]
    fn aimd_config_clamping() {
        let toml = r#"
            version = 1
            [scheduler]
            rtt_congestion_ratio = 0.5
            md_factor = 0.01
            ai_step_ratio = 5.0
            decrease_cooldown_ms = 10
            rtt_min_fast_window_s = 0.01
            rtt_min_slow_window_s = 1.0
            max_capacity_bps = -100.0
            loss_md_threshold = 2.0
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.scheduler.rtt_congestion_ratio - 1.0).abs() < 1e-6);
        assert!((cfg.scheduler.md_factor - 0.1).abs() < 1e-6);
        assert!((cfg.scheduler.ai_step_ratio - 1.0).abs() < 1e-6);
        assert_eq!(cfg.scheduler.decrease_cooldown_ms, 50);
        assert!((cfg.scheduler.rtt_min_fast_window_s - 0.5).abs() < 1e-6);
        assert!((cfg.scheduler.rtt_min_slow_window_s - 5.0).abs() < 1e-6);
        assert!((cfg.scheduler.max_capacity_bps - 0.0).abs() < 1e-6);
        assert!((cfg.scheduler.loss_md_threshold - 1.0).abs() < 1e-6);
    }

    // ────────────────────────────────────────────────────────────────
    // Config-to-runtime wiring tests
    //
    // These verify that parsed TOML values actually reach their
    // destination structs (ReassemblyConfig, SchedulerConfig, etc.).
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn buffer_capacity_reaches_reassembly_config() {
        use crate::receiver::aggregator::ReassemblyConfig;
        let toml = r#"
            version = 1
            [receiver]
            buffer_capacity = 512
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.receiver.buffer_capacity, 512);

        // Construct ReassemblyConfig the same way src.rs does
        let rc = ReassemblyConfig {
            start_latency: cfg.receiver.start_latency,
            buffer_capacity: cfg.receiver.buffer_capacity,
            skip_after: cfg.receiver.skip_after,
            jitter_latency_multiplier: cfg.scheduler.jitter_latency_multiplier,
            max_latency_ms: cfg.scheduler.max_latency_ms,
        };
        assert_eq!(rc.buffer_capacity, 512);
    }

    #[test]
    fn skip_after_ms_reaches_reassembly_config() {
        use crate::receiver::aggregator::{ReassemblyBuffer, ReassemblyConfig};
        use std::time::{Duration, Instant};

        let toml = r#"
            version = 1
            [receiver]
            skip_after_ms = 25
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.receiver.skip_after, Some(Duration::from_millis(25)));

        let rc = ReassemblyConfig {
            start_latency: Duration::from_millis(100),
            skip_after: cfg.receiver.skip_after,
            ..ReassemblyConfig::default()
        };
        let mut buf = ReassemblyBuffer::with_config(0, rc);
        let now = Instant::now();

        // Push seq 1, missing seq 0
        buf.push(1, bytes::Bytes::from_static(b"P1"), now);

        // At 24ms, should NOT release (skip_after is 25ms)
        let out = buf.tick(now + Duration::from_millis(24));
        assert!(out.is_empty(), "Should not release before skip_after");

        // At 25ms, SHOULD release (gap waited long enough)
        let out = buf.tick(now + Duration::from_millis(25));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], bytes::Bytes::from_static(b"P1"));
    }

    #[test]
    fn jitter_latency_multiplier_reaches_reassembly() {
        let toml = r#"
            version = 1
            [scheduler]
            jitter_latency_multiplier = 8.0
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.scheduler.jitter_latency_multiplier - 8.0).abs() < 1e-6);
    }

    #[test]
    fn max_latency_ms_reaches_reassembly() {
        let toml = r#"
            version = 1
            [scheduler]
            max_latency_ms = 100
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.scheduler.max_latency_ms, 100);
    }

    #[test]
    fn start_latency_reaches_receiver_config() {
        let toml = r#"
            version = 1
            [receiver]
            start_latency_ms = 75
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.receiver.start_latency, Duration::from_millis(75));
    }

    #[test]
    fn channel_capacity_reaches_scheduler_config() {
        let toml = r#"
            version = 1
            [scheduler]
            channel_capacity = 500
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.scheduler.channel_capacity, 500);
    }

    #[test]
    fn ewma_alpha_reaches_scheduler_config() {
        let toml = r#"
            version = 1
            [scheduler]
            ewma_alpha = 0.25
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.scheduler.ewma_alpha - 0.25).abs() < 1e-6);
    }

    #[test]
    fn nada_config_reaches_scheduler() {
        let toml = r#"
            version = 1
            [scheduler]
            dloss_ref_ms = 20.0
            plr_ref = 0.02
            gamma_max = 0.3
            qbound_ms = 100.0
            nada_kappa = 0.8
            nada_eta = 3.0
            nada_tau_ms = 600.0
            nada_xref_ms = 15.0
            nada_prio = 2.0
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.scheduler.dloss_ref_ms - 20.0).abs() < 1e-6);
        assert!((cfg.scheduler.plr_ref - 0.02).abs() < 1e-6);
        assert!((cfg.scheduler.gamma_max - 0.3).abs() < 1e-6);
        assert!((cfg.scheduler.qbound_ms - 100.0).abs() < 1e-6);
        assert!((cfg.scheduler.nada_kappa - 0.8).abs() < 1e-6);
        assert!((cfg.scheduler.nada_eta - 3.0).abs() < 1e-6);
        assert!((cfg.scheduler.nada_tau_ms - 600.0).abs() < 1e-6);
        assert!((cfg.scheduler.nada_xref_ms - 15.0).abs() < 1e-6);
        assert!((cfg.scheduler.nada_prio - 2.0).abs() < 1e-6);
    }

    #[test]
    fn sbd_config_reaches_scheduler() {
        let toml = r#"
            version = 1
            [scheduler]
            sbd_enabled = true
            sbd_interval_ms = 500
            sbd_n = 100
            sbd_c_s = 0.2
            sbd_c_h = 0.5
            sbd_p_l = 0.15
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!(cfg.scheduler.sbd_enabled);
        assert_eq!(cfg.scheduler.sbd_interval_ms, 500);
        assert_eq!(cfg.scheduler.sbd_n, 100);
        assert!((cfg.scheduler.sbd_c_s - 0.2).abs() < 1e-6);
        assert!((cfg.scheduler.sbd_c_h - 0.5).abs() < 1e-6);
        assert!((cfg.scheduler.sbd_p_l - 0.15).abs() < 1e-6);
    }

    #[test]
    fn aimd_config_reaches_scheduler() {
        let toml = r#"
            version = 1
            [scheduler]
            capacity_estimate_enabled = false
            rtt_congestion_ratio = 2.5
            md_factor = 0.5
            ai_step_ratio = 0.1
            decrease_cooldown_ms = 1000
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!(!cfg.scheduler.capacity_estimate_enabled);
        assert!((cfg.scheduler.rtt_congestion_ratio - 2.5).abs() < 1e-6);
        assert!((cfg.scheduler.md_factor - 0.5).abs() < 1e-6);
        assert!((cfg.scheduler.ai_step_ratio - 0.1).abs() < 1e-6);
        assert_eq!(cfg.scheduler.decrease_cooldown_ms, 1000);
    }

    #[test]
    fn lifecycle_config_reaches_resolved() {
        let toml = r#"
            version = 1
            [lifecycle]
            good_loss_rate_max = 0.1
            cooldown_ms = 5000
            probe_to_warm_good = 5
            warm_to_live_good = 15
            live_to_degrade_bad = 5
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert!((cfg.lifecycle.good_loss_rate_max - 0.1).abs() < 1e-6);
        assert_eq!(cfg.lifecycle.cooldown_ms, 5000);
        assert_eq!(cfg.lifecycle.probe_to_warm_good, 5);
        assert_eq!(cfg.lifecycle.warm_to_live_good, 15);
        assert_eq!(cfg.lifecycle.live_to_degrade_bad, 5);
    }

    #[test]
    fn recovery_params_reach_link_config() {
        let toml = r#"
            version = 1
            [[links]]
            uri = "rist://10.0.0.1:5000"
            recovery_maxbitrate = 50000
            recovery_rtt_max = 1000
            recovery_reorder_buffer = 30
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.links[0].recovery_maxbitrate, Some(50000));
        assert_eq!(cfg.links[0].recovery_rtt_max, Some(1000));
        assert_eq!(cfg.links[0].recovery_reorder_buffer, Some(30));
    }

    /// Verify that ALL scheduler config fields parsed from TOML arrive in
    /// the resolved SchedulerConfig.
    #[test]
    fn all_scheduler_fields_wired() {
        let toml = r#"
            version = 1
            [scheduler]
            redundancy_enabled = false
            redundancy_spare_ratio = 0.7
            redundancy_max_packet_bytes = 5000
            redundancy_target_links = 4
            critical_broadcast = false
            failover_enabled = false
            failover_duration_ms = 8000
            failover_rtt_spike_factor = 5.0
            ewma_alpha = 0.3
            prediction_horizon_s = 2.0
            capacity_floor_bps = 3000000.0
            penalty_decay = 0.8
            penalty_recovery = 0.03
            jitter_latency_multiplier = 6.0
            max_latency_ms = 400
            stats_interval_ms = 750
            channel_capacity = 3000
            capacity_estimate_enabled = false
            rtt_congestion_ratio = 2.0
            md_factor = 0.6
            ai_step_ratio = 0.05
            decrease_cooldown_ms = 800
            rtt_min_fast_window_s = 4.0
            rtt_min_slow_window_s = 45.0
            max_capacity_bps = 200000000.0
            loss_md_threshold = 0.04
            dloss_ref_ms = 15.0
            plr_ref = 0.005
            gamma_max = 0.4
            qbound_ms = 80.0
            qeps_ms = 5.0
            nada_kappa = 0.3
            nada_eta = 1.5
            nada_tau_ms = 400.0
            nada_xref_ms = 8.0
            nada_prio = 1.5
            sbd_enabled = true
            sbd_interval_ms = 400
            sbd_n = 80
            sbd_c_s = 0.15
            sbd_c_h = 0.25
            sbd_p_l = 0.08
        "#;
        let cfg = BondingConfig::from_toml_str(toml).unwrap();
        let s = &cfg.scheduler;

        assert!(!s.redundancy_enabled);
        assert!((s.redundancy_spare_ratio - 0.7).abs() < 1e-6);
        assert_eq!(s.redundancy_max_packet_bytes, 5000);
        assert_eq!(s.redundancy_target_links, 4);
        assert!(!s.critical_broadcast);
        assert!(!s.failover_enabled);
        assert_eq!(s.failover_duration_ms, 8000);
        assert!((s.failover_rtt_spike_factor - 5.0).abs() < 1e-6);
        assert!((s.ewma_alpha - 0.3).abs() < 1e-6);
        assert!((s.prediction_horizon_s - 2.0).abs() < 1e-6);
        assert!((s.capacity_floor_bps - 3_000_000.0).abs() < 1e-6);
        assert!((s.penalty_decay - 0.8).abs() < 1e-6);
        assert!((s.penalty_recovery - 0.03).abs() < 1e-6);
        assert!((s.jitter_latency_multiplier - 6.0).abs() < 1e-6);
        assert_eq!(s.max_latency_ms, 400);
        assert_eq!(s.stats_interval_ms, 750);
        assert_eq!(s.channel_capacity, 3000);
        assert!(!s.capacity_estimate_enabled);
        assert!((s.rtt_congestion_ratio - 2.0).abs() < 1e-6);
        assert!((s.md_factor - 0.6).abs() < 1e-6);
        assert!((s.ai_step_ratio - 0.05).abs() < 1e-6);
        assert_eq!(s.decrease_cooldown_ms, 800);
        assert!((s.rtt_min_fast_window_s - 4.0).abs() < 1e-6);
        assert!((s.rtt_min_slow_window_s - 45.0).abs() < 1e-6);
        assert!((s.max_capacity_bps - 200_000_000.0).abs() < 1e-3);
        assert!((s.loss_md_threshold - 0.04).abs() < 1e-6);
        assert!((s.dloss_ref_ms - 15.0).abs() < 1e-6);
        assert!((s.plr_ref - 0.005).abs() < 1e-6);
        assert!((s.gamma_max - 0.4).abs() < 1e-6);
        assert!((s.qbound_ms - 80.0).abs() < 1e-6);
        assert!((s.qeps_ms - 5.0).abs() < 1e-6);
        assert!((s.nada_kappa - 0.3).abs() < 1e-6);
        assert!((s.nada_eta - 1.5).abs() < 1e-6);
        assert!((s.nada_tau_ms - 400.0).abs() < 1e-6);
        assert!((s.nada_xref_ms - 8.0).abs() < 1e-6);
        assert!((s.nada_prio - 1.5).abs() < 1e-6);
        assert!(s.sbd_enabled);
        assert_eq!(s.sbd_interval_ms, 400);
        assert_eq!(s.sbd_n, 80);
        assert!((s.sbd_c_s - 0.15).abs() < 1e-6);
        assert!((s.sbd_c_h - 0.25).abs() < 1e-6);
        assert!((s.sbd_p_l - 0.08).abs() < 1e-6);
    }

    /// Verify that config defaults match between parsed defaults and
    /// the struct defaults (guards against divergence).
    #[test]
    fn config_defaults_match_struct_defaults() {
        let cfg = BondingConfig::from_toml_str("").unwrap();
        let sched_default = SchedulerConfig::default();
        let rcv_default = ReceiverConfig::default();

        assert_eq!(
            cfg.scheduler.redundancy_enabled,
            sched_default.redundancy_enabled
        );
        assert!(
            (cfg.scheduler.ewma_alpha - sched_default.ewma_alpha).abs() < 1e-6,
            "ewma_alpha default mismatch: parsed={} struct={}",
            cfg.scheduler.ewma_alpha,
            sched_default.ewma_alpha
        );
        assert_eq!(
            cfg.scheduler.channel_capacity,
            sched_default.channel_capacity
        );
        assert_eq!(
            cfg.scheduler.stats_interval_ms,
            sched_default.stats_interval_ms
        );
        assert_eq!(
            cfg.scheduler.capacity_estimate_enabled,
            sched_default.capacity_estimate_enabled
        );
        assert!(!cfg.scheduler.sbd_enabled);
        assert_eq!(cfg.receiver.start_latency, rcv_default.start_latency);
        assert_eq!(cfg.receiver.buffer_capacity, rcv_default.buffer_capacity);
        assert_eq!(cfg.receiver.skip_after, rcv_default.skip_after);
    }

    // ────────────────────────────────────────────────────────────────
    // deny_unknown_fields coverage for nested sections
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn unknown_key_in_links_rejected() {
        let toml = r#"
            version = 1
            [[links]]
            uri = "rist://1.2.3.4:5000"
            bogus_field = 42
        "#;
        let result = BondingConfig::from_toml_str(toml);
        assert!(
            result.is_err(),
            "Unknown key in [[links]] should be rejected"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("bogus_field"),
            "Error should mention the unknown field, got: {}",
            msg
        );
    }

    #[test]
    fn unknown_key_in_lifecycle_rejected() {
        let toml = r#"
            version = 1
            [lifecycle]
            cooldown_ms = 5000
            nonexistent_knob = true
        "#;
        let result = BondingConfig::from_toml_str(toml);
        assert!(
            result.is_err(),
            "Unknown key in [lifecycle] should be rejected"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("nonexistent_knob"),
            "Error should mention the unknown field, got: {}",
            msg
        );
    }
}
