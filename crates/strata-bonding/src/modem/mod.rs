//! # Modem Intelligence Layer
//!
//! Link health scoring from RF metrics (RSRP, RSRQ, SINR, CQI) and
//! transport-level feedback (loss rate, jitter). Uses Kalman-smoothed
//! metrics for stable scoring. Includes band locking automation for
//! frequency diversity across multiple modems.

pub mod band;
pub mod health;
pub mod supervisor;
