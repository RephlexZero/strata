use anyhow::Result;

#[derive(Default, Debug, Clone)]
pub struct LinkMetrics {
    pub rtt_ms: f64,
    pub capacity_bps: f64,
    pub loss_rate: f64, // 0.0 - 1.0 (Percentage of bad packets)
    pub queue_depth: usize,
    pub max_queue: usize,
    pub alive: bool,
}

pub trait LinkSender: Send + Sync {
    fn id(&self) -> usize;
    fn send(&self, packet: &[u8]) -> Result<usize>;
    fn get_metrics(&self) -> LinkMetrics;
}
