//! Bonding receiver and jitter-buffer reassembly.

pub mod aggregator;
pub mod transport;

use anyhow::Result;
use bytes::Bytes;
use crossbeam_channel::Receiver;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use self::aggregator::ReassemblyStats;
use self::transport::TransportBondingReceiver;

/// Bonding receiver backed by the pure-Rust strata-transport layer.
///
/// Wraps [`TransportBondingReceiver`] and provides URI parsing for
/// backward compatibility with both `host:port` and legacy `rist://` formats.
pub struct ReceiverBackend {
    inner: TransportBondingReceiver,
}

impl ReceiverBackend {
    /// Create a new receiver with the given jitter-buffer latency.
    pub fn new(latency: Duration) -> Self {
        ReceiverBackend {
            inner: TransportBondingReceiver::new(latency),
        }
    }

    /// Add a link by address string.
    ///
    /// Accepts both plain socket addresses (e.g. `0.0.0.0:5000`) and
    /// legacy `rist://` URIs for backward compatibility.
    pub fn add_link(&self, addr: &str) -> Result<()> {
        let socket_addr = parse_receiver_addr(addr)?;
        self.inner.add_link(socket_addr)
    }

    /// The output channel for received reassembled payloads.
    pub fn output_rx(&self) -> &Receiver<Bytes> {
        &self.inner.output_rx
    }

    /// Get current reassembly stats.
    pub fn get_stats(&self) -> ReassemblyStats {
        self.inner.get_stats()
    }

    /// Returns a shared handle to the reassembly stats for external polling.
    pub fn stats_handle(&self) -> Arc<Mutex<ReassemblyStats>> {
        self.inner.stats_handle()
    }

    /// Shut down the receiver.
    pub fn shutdown(&mut self) {
        self.inner.shutdown();
    }
}

/// Parse a receiver address string to a `SocketAddr`.
///
/// Supports plain `host:port` format and legacy `rist://` URIs for
/// backward compatibility.
fn parse_receiver_addr(addr: &str) -> Result<SocketAddr> {
    // Strip legacy rist:// prefix if present
    if let Some(stripped) = addr
        .strip_prefix("rist://@")
        .or_else(|| addr.strip_prefix("rist://"))
    {
        let host_port = stripped.split('?').next().unwrap_or(stripped);
        return host_port
            .parse::<SocketAddr>()
            .map_err(|e| anyhow::anyhow!("Invalid address in URI '{}': {}", addr, e));
    }
    // Try raw socket address
    addr.parse::<SocketAddr>()
        .map_err(|e| anyhow::anyhow!("Invalid receiver address '{}': {}", addr, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_receiver_addr_listener() {
        let addr = parse_receiver_addr("rist://@0.0.0.0:5000").unwrap();
        assert_eq!(addr, "0.0.0.0:5000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_receiver_addr_sender() {
        let addr = parse_receiver_addr("rist://127.0.0.1:6000").unwrap();
        assert_eq!(addr, "127.0.0.1:6000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_receiver_addr_raw() {
        let addr = parse_receiver_addr("0.0.0.0:7000").unwrap();
        assert_eq!(addr, "0.0.0.0:7000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_receiver_addr_with_query() {
        let addr = parse_receiver_addr("rist://@0.0.0.0:5000?miface=eth0").unwrap();
        assert_eq!(addr, "0.0.0.0:5000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_receiver_addr_invalid() {
        assert!(parse_receiver_addr("bogus").is_err());
    }

    #[test]
    fn receiver_backend_creates() {
        let mut backend = ReceiverBackend::new(Duration::from_millis(50));
        backend.shutdown();
    }
}
