//! Shared types for the Strata platform.
//!
//! This crate contains:
//! - **Protocol messages** — WebSocket message types between agent and control plane
//! - **Auth primitives** — JWT creation/validation, Argon2id password hashing
//! - **Data models** — User, Sender, Destination, Stream types
//! - **ID generation** — Prefixed nanoid helpers (`usr_`, `snd_`, `str_`, `dst_`)

pub mod auth;
pub mod ids;
pub mod models;
pub mod protocol;
