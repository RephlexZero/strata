//! # Media Awareness — NAL Unit Parsing & Priority Classification
//!
//! Parses H.264/H.265/AV1 bitstreams to classify packets by importance.
//! This enables the scheduler to protect keyframes, broadcast parameter sets,
//! and drop non-reference B-frames under pressure.

pub mod nal;
pub mod priority;
