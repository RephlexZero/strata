//! # Modem RF Metrics Seam
//!
//! Defines [`health::RfMetrics`], the raw radio readings forwarded to each
//! link's Biscay congestion controller. The radio feed-forward logic itself
//! (SINR ceiling, CQI-derivative, RSRP-slope handover detection) lives in
//! `strata-transport`'s `BiscayController::on_radio_metrics`; this module only
//! carries the metric type across the crate boundary.
//!
//! No field producer exists yet — see [`health`] for why.

pub mod health;
