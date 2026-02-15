//! Strata Control Plane library.
//!
//! Re-exports the API router, shared state, and database utilities so they
//! can be used by integration tests (and potentially embedded in other
//! binaries).

pub mod api;
pub mod db;
pub mod state;
pub mod ws_agent;
pub mod ws_dashboard;
