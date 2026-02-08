//! Network simulation toolkit for integration testing.
//!
//! Provides Linux network namespace management, `tc netem` impairment
//! application, and deterministic scenario generation for testing
//! bonding behaviour under controlled network conditions.

pub mod impairment;
pub mod scenario;
pub mod topology;

#[cfg(test)]
pub(crate) mod test_util;
