//! Transport-agnostic bonding engine for RIST streams.
//!
//! This crate provides the core scheduler, link lifecycle management,
//! receiver reassembly, and librist FFI wrappers used by the GStreamer
//! plugin (`gst-rist-bonding`) and the integration node binary.
//!
//! Key components:
//! - [`scheduler`] — Deficit Weighted Round Robin (DWRR) packet scheduler
//!   with adaptive redundancy and fast-failover
//! - [`net`] — Network link abstraction, lifecycle state machine, and
//!   librist sender/receiver wrappers
//! - [`receiver`] — Bonding receiver with jitter-buffer reassembly
//! - [`config`] — TOML-based configuration with versioned schema
//! - [`runtime`] — Thread-safe runtime that owns the scheduler loop

pub mod config;
pub(crate) mod net;
pub mod protocol;
pub mod receiver;
pub mod runtime;
pub mod scheduler;
pub mod stats;

// Re-export types that downstream crates need from `net`.
pub use net::interface::{LinkMetrics, LinkPhase, LinkSender};
pub use net::wrapper::{RecoveryConfig, RistContext, RistReceiverContext};

/// Initialize the rist-bonding-core library.
///
/// Installs a default `tracing` subscriber (env-filter based) if no subscriber
/// is already set. Safe to call multiple times — subsequent calls are no-ops.
/// Controlled by `RUST_LOG` env var (e.g., `RUST_LOG=rist_bonding_core=debug`).
pub fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Only install if no subscriber is already set (e.g., by the host application).
        if tracing::dispatcher::has_been_set() {
            tracing::info!("Rist Bonding Core: tracing subscriber already set");
            return;
        }
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_target(true)
            .with_thread_names(true)
            .compact()
            .finish();
        if tracing::subscriber::set_global_default(subscriber).is_ok() {
            tracing::info!("Rist Bonding Core initialized");
        }
    });
}
