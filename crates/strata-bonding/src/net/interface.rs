use anyhow::Result;
use std::net::IpAddr;

/// Resolve a network interface name (e.g., "eth0") to its first IPv4 address.
/// Returns `None` if the interface doesn't exist or has no IPv4 address.
pub fn resolve_iface_ipv4(iface: &str) -> Option<IpAddr> {
    let path = format!("/sys/class/net/{}/", iface);
    if !std::path::Path::new(&path).exists() {
        return None;
    }

    // Use libc getifaddrs for reliable interface address resolution.
    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifaddrs) != 0 {
            return None;
        }

        let mut current = ifaddrs;
        let mut result = None;

        while !current.is_null() {
            let ifa = &*current;
            if !ifa.ifa_addr.is_null() {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                if name == iface && (*ifa.ifa_addr).sa_family == libc::AF_INET as u16 {
                    let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    let ip =
                        IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)));
                    result = Some(ip);
                    break;
                }
            }
            current = ifa.ifa_next;
        }

        libc::freeifaddrs(ifaddrs);
        result
    }
}

/// Lifecycle phase of a network link.
///
/// Links progress through these phases based on observed statistics:
///
/// ```text
/// Init → Probe → Warm → Live ⇄ Degrade → Cooldown → Probe → …
///                                    ↓
///                                  Reset → Probe
/// ```
///
/// The scheduler uses the phase to weight link credit accrual —
/// `Live` links get full credit, `Probe` links are rate-limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkPhase {
    #[default]
    Init,
    Probe,
    Warm,
    Live,
    Degrade,
    Cooldown,
    Reset,
}

impl LinkPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkPhase::Init => "init",
            LinkPhase::Probe => "probe",
            LinkPhase::Warm => "warm",
            LinkPhase::Live => "live",
            LinkPhase::Degrade => "degrade",
            LinkPhase::Cooldown => "cooldown",
            LinkPhase::Reset => "reset",
        }
    }
}

/// Snapshot of a link's current telemetry.
///
/// Populated by [`LinkSender::get_metrics()`] from smoothed EWMA values
/// and OS-level interface state (operstate, MTU). The scheduler uses these
/// to compute effective capacity and credit accrual rates.
#[derive(Default, Debug, Clone)]
pub struct LinkMetrics {
    pub rtt_ms: f64,
    pub capacity_bps: f64,
    pub loss_rate: f64, // 0.0 - 1.0 (Percentage of bad packets)
    pub observed_bps: f64,
    pub observed_bytes: u64,
    pub queue_depth: usize,
    pub max_queue: usize,
    pub alive: bool,
    pub phase: LinkPhase,
    pub os_up: Option<bool>,
    pub mtu: Option<u32>,
    pub iface: Option<String>,
    pub link_kind: Option<String>,
    /// Transport-layer statistics (FEC, ARQ, retransmissions).
    pub transport: Option<TransportMetrics>,
    /// AIMD delay-gradient capacity estimate (0.0 if estimator disabled).
    pub estimated_capacity_bps: f64,
    /// One-way delay estimate in milliseconds (0.0 if not available).
    pub owd_ms: f64,
    /// Latest receiver report from the remote receiver (if any).
    pub receiver_report: Option<ReceiverReportMetrics>,
}

/// Receiver report metrics forwarded from the remote receiver.
#[derive(Debug, Clone, Default)]
pub struct ReceiverReportMetrics {
    /// Total recovered goodput (bits/sec).
    pub goodput_bps: u64,
    /// Fraction of packets recovered by FEC (0.0–1.0).
    pub fec_repair_rate: f32,
    /// Current jitter buffer depth in milliseconds.
    pub jitter_buffer_ms: u32,
    /// Residual loss after FEC recovery (0.0–1.0).
    pub loss_after_fec: f32,
}

/// Transport-layer statistics from `strata-transport`.
///
/// Captures FEC, ARQ, and retransmission counters that are not visible
/// at the bonding-scheduler level.
#[derive(Debug, Clone, Default)]
pub struct TransportMetrics {
    /// Total packets sent (including retransmissions and FEC repairs).
    pub packets_sent: u64,
    /// Packets acknowledged by the receiver.
    pub packets_acked: u64,
    /// NACK-triggered retransmissions.
    pub retransmissions: u64,
    /// FEC repair packets sent.
    pub fec_repairs_sent: u64,
    /// Packets expired from send buffer without ACK.
    pub packets_expired: u64,
}

/// Abstraction for a network link capable of sending packets and reporting metrics.
///
/// Implemented by [`crate::net::transport::TransportLink`] and by
/// mock links in tests.
pub trait LinkSender: Send + Sync {
    /// Returns the unique identifier of this link.
    fn id(&self) -> usize;
    /// Sends raw bytes over this link. Returns the number of bytes written.
    fn send(&self, packet: &[u8]) -> Result<usize>;
    /// Returns a snapshot of the link's current metrics.
    fn get_metrics(&self) -> LinkMetrics;
    /// Read and process any pending feedback (ACKs, NACKs, Pongs) from the
    /// receiver. Also sends periodic Ping probes for RTT measurement.
    /// Returns the number of feedback packets processed.
    fn recv_feedback(&self) -> usize {
        0
    }

    /// Forward RF metrics from the modem supervisor to this link's congestion
    /// controller. Called whenever the modem poller produces updated
    /// CQI/RSRP/SINR readings.
    ///
    /// The default is a no-op — mock links and non-cellular transports silently
    /// ignore it. [`crate::net::transport::TransportLink`] overrides this to
    /// feed [`strata_transport::congestion::BiscayController::on_radio_metrics`].
    /// Without data, Biscay stays in `Normal` state with no SINR ceiling, so
    /// Docker/CI environments are unaffected.
    fn on_rf_metrics(&self, _rf: &crate::modem::health::RfMetrics) {}

    /// Allow or inhibit the BBR UP-probe gain on this link.
    ///
    /// Called by the bonding scheduler's probe coordinator to ensure only one
    /// link at a time actively probes for spare bandwidth. The default no-op
    /// implementation is sufficient for mock links and simulation links; real
    /// [`crate::net::transport::TransportLink`] instances forward this to their
    /// [`strata_transport::congestion::BiscayController`].
    fn set_probe_allowed(&self, _allowed: bool) {}
}
