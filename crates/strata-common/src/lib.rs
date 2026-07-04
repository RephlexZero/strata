//! Shared server-side helpers for the Strata platform.
//!
//! This crate contains:
//! - **Auth primitives** — JWT creation/validation, Argon2id password hashing
//! - **ID generation** — Prefixed nanoid helpers (`usr_`, `snd_`, `str_`, `dst_`)
//! - **Metrics rendering** — Prometheus text exposition
//!
//! Wire types (protocol messages, data models, REST API types, profiles)
//! live in `strata-protocol` — the wasm-safe single source of truth.

pub mod auth;
pub mod identity;
pub mod ids;
pub mod metrics;
