//! Network simulation toolkit for integration testing.
//!
//! Provides Linux network namespace management, `tc netem` impairment
//! application, and deterministic scenario generation for testing
//! bonding behaviour under controlled network conditions.

pub mod bonding_scenarios;
pub mod impairment;
pub mod scenario;
pub mod topology;

pub mod test_util;
