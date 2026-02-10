use anyhow::Result;

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
    /// AIMD delay-gradient capacity estimate (0.0 if estimator disabled).
    pub estimated_capacity_bps: f64,
}

/// Abstraction for a network link capable of sending packets and reporting metrics.
///
/// Implemented by [`crate::net::link::Link`] (backed by librist) and by
/// mock links in tests.
pub trait LinkSender: Send + Sync {
    /// Returns the unique identifier of this link.
    fn id(&self) -> usize;
    /// Sends raw bytes over this link. Returns the number of bytes written.
    fn send(&self, packet: &[u8]) -> Result<usize>;
    /// Returns a snapshot of the link's current metrics.
    fn get_metrics(&self) -> LinkMetrics;
}
