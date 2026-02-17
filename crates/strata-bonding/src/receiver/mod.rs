//! Bonding receiver and jitter-buffer reassembly.

pub mod aggregator;
pub mod bonding;
pub mod transport;

use anyhow::Result;
use bytes::Bytes;
use crossbeam_channel::Receiver;
use std::net::SocketAddr;
use std::time::Duration;

use self::aggregator::ReassemblyStats;
use self::bonding::BondingReceiver;
use self::transport::TransportBondingReceiver;

/// Unified receiver that dispatches to either the librist (`BondingReceiver`)
/// or pure-Rust transport (`TransportBondingReceiver`) backend.
pub enum ReceiverBackend {
    Rist(BondingReceiver),
    Transport(TransportBondingReceiver),
}

impl ReceiverBackend {
    /// Create a new receiver for the given transport mode.
    pub fn new(latency: Duration, use_transport: bool) -> Self {
        if use_transport {
            ReceiverBackend::Transport(TransportBondingReceiver::new(latency))
        } else {
            ReceiverBackend::Rist(BondingReceiver::new(latency))
        }
    }

    /// Add a link by bind URL/address string.
    ///
    /// For RIST mode, `addr` is a full RIST URL (e.g. `rist://@0.0.0.0:5000`).
    /// For transport mode, `addr` is parsed as a socket address.
    pub fn add_link(&self, addr: &str) -> Result<()> {
        match self {
            ReceiverBackend::Rist(r) => r.add_link(addr),
            ReceiverBackend::Transport(r) => {
                let socket_addr = parse_receiver_addr(addr)?;
                r.add_link(socket_addr)
            }
        }
    }

    /// The output channel for received reassembled payloads.
    pub fn output_rx(&self) -> &Receiver<Bytes> {
        match self {
            ReceiverBackend::Rist(r) => &r.output_rx,
            ReceiverBackend::Transport(r) => &r.output_rx,
        }
    }

    /// Get current reassembly stats.
    pub fn get_stats(&self) -> ReassemblyStats {
        match self {
            ReceiverBackend::Rist(r) => r.get_stats(),
            ReceiverBackend::Transport(r) => r.get_stats(),
        }
    }

    /// Shut down the receiver.
    pub fn shutdown(&mut self) {
        match self {
            ReceiverBackend::Rist(r) => r.shutdown(),
            ReceiverBackend::Transport(r) => r.shutdown(),
        }
    }
}

/// Parse a RIST receiver URL or socket address to a `SocketAddr`.
fn parse_receiver_addr(addr: &str) -> Result<SocketAddr> {
    // Try RIST URL format first (more specific prefix first)
    if let Some(stripped) = addr
        .strip_prefix("rist://@")
        .or_else(|| addr.strip_prefix("rist://"))
    {
        let host_port = stripped.split('?').next().unwrap_or(stripped);
        return host_port
            .parse::<SocketAddr>()
            .map_err(|e| anyhow::anyhow!("Invalid address in RIST URL '{}': {}", addr, e));
    }
    // Try raw socket address
    addr.parse::<SocketAddr>()
        .map_err(|e| anyhow::anyhow!("Invalid receiver address '{}': {}", addr, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_receiver_addr_rist_listener() {
        let addr = parse_receiver_addr("rist://@0.0.0.0:5000").unwrap();
        assert_eq!(addr, "0.0.0.0:5000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_receiver_addr_rist_sender() {
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
    fn receiver_backend_rist_mode() {
        let mut backend = ReceiverBackend::new(Duration::from_millis(50), false);
        assert!(matches!(backend, ReceiverBackend::Rist(_)));
        backend.shutdown();
    }

    #[test]
    fn receiver_backend_transport_mode() {
        let mut backend = ReceiverBackend::new(Duration::from_millis(50), true);
        assert!(matches!(backend, ReceiverBackend::Transport(_)));
        backend.shutdown();
    }
}
