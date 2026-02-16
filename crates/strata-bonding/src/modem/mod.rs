//! # Modem Intelligence Layer
//!
//! Link health scoring from RF metrics (RSRP, RSRQ, SINR, CQI) and
//! transport-level feedback (loss rate, jitter). Uses Kalman-smoothed
//! metrics for stable scoring.

pub mod health;
pub mod supervisor;
