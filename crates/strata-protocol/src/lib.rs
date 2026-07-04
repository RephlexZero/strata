//! Single source of truth for the Strata platform wire protocol.
//!
//! Everything that crosses a machine boundary lives here, exactly once:
//!
//! - [`Envelope`] — the outer wrapper for every WebSocket message
//! - [`AgentMessage`] / [`ControlMessage`] — agent ⇄ control plane
//! - [`ReceiverMessage`] / [`ReceiverControlMessage`] — receiver ⇄ control plane
//! - [`DashboardEvent`] — control plane → browser
//! - [`api`] — REST request/response types shared by control plane and dashboard
//! - [`models`] — data models embedded in messages (interfaces, streams, stats)
//! - [`profiles`] — bitrate profile presets
//!
//! This crate is wasm-safe (serde types only — no argon2/tokio/sqlx), so the
//! Leptos dashboard imports it directly instead of hand-copying types.
//! Hubs and daemons dispatch by parsing an [`Envelope`] into the appropriate
//! direction enum ([`Envelope::parse_message`]) and matching exhaustively —
//! adding a message type is a compile error until every dispatch site
//! handles it.

pub mod api;
mod envelope;
mod messages;
pub mod models;
mod payloads;
pub mod profiles;

pub use envelope::{Envelope, PROTOCOL_VERSION};
pub use messages::*;
pub use payloads::*;
