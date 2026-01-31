use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPhase {
    Init,
    Probe,
    Warm,
    Live,
    Degrade,
    Cooldown,
    Reset,
}

impl Default for LinkPhase {
    fn default() -> Self {
        LinkPhase::Init
    }
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
}

pub trait LinkSender: Send + Sync {
    fn id(&self) -> usize;
    fn send(&self, packet: &[u8]) -> Result<usize>;
    fn get_metrics(&self) -> LinkMetrics;
}
