//! # Cellular RF Metrics
//!
//! Defines [`RfMetrics`] — the raw radio readings a modem poller produces.
//! These are forwarded to a link's Biscay congestion controller via
//! [`crate::scheduler::bonding::BondingScheduler::notify_rf_metrics`], where
//! the SINR→capacity ceiling, CQI-derivative tracking and RSRP-slope handover
//! detection actually live (see
//! [`strata_transport::congestion::BiscayController::on_radio_metrics`]).
//!
//! There is no field producer yet: the in-use USB dongles run in NCM/ECM mode
//! and expose no QMI/MBIM metric interface. This type is the integration seam
//! kept ready for a QMI/MBIM-capable modem + poller. Until then Biscay stays in
//! its `Normal` state with no SINR ceiling, which is the correct default for
//! Docker/CI and for radio-blind operation.

/// Raw RF metrics from a cellular modem (via QMI/MBIM).
#[derive(Debug, Clone, Copy, Default)]
pub struct RfMetrics {
    /// Reference Signal Received Power in dBm. Range: -140 to -44.
    pub rsrp_dbm: f64,
    /// Reference Signal Received Quality in dB. Range: -20 to -3.
    pub rsrq_db: f64,
    /// Signal-to-Interference-plus-Noise Ratio in dB. Range: -20 to 30.
    pub sinr_db: f64,
    /// Channel Quality Indicator. Range: 0–15.
    pub cqi: u8,
}
