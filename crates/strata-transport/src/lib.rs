//! # strata-transport
//!
//! Strata pure-Rust transport protocol.
//!
//! Custom wire format with QUIC-style VarInt sequence numbers, hybrid FEC+ARQ
//! reliability, Biscay radio-aware congestion control, and media-aware packet
//! prioritization.
//!
//! ## Crate structure
//!
//! - [`wire`] — Packet header serialization, control packets, VarInt
//! - [`pool`] — Slab-based packet buffer pool
//! - [`session`] — Session handshake, keepalive, RTT tracking
//! - [`codec`] — FEC encoding/decoding (Reed-Solomon)
//! - [`arq`] — NACK-based loss detection and retransmission
//! - [`congestion`] — Biscay congestion control (BBRv3-inspired)
//! - [`stats`] — Per-link and aggregate statistics
//! - [`sender`] — Sender state machine
//! - [`receiver`] — Receiver state machine

pub mod arq;
pub mod codec;
pub mod congestion;
pub mod pool;
pub mod receiver;
pub mod sender;
pub mod session;
pub mod stats;
pub mod wire;
