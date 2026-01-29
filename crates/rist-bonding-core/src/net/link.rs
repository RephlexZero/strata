use crate::net::interface::{LinkMetrics, LinkSender};
use crate::net::state::LinkStats;
use crate::net::wrapper::RistContext;
use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct Link {
    pub id: usize,
    ctx: RistContext,
    stats: Arc<LinkStats>,
    created_at: std::time::Instant,
}

impl Link {
    pub fn new(id: usize, url: &str) -> Result<Self> {
        let mut ctx = RistContext::new(crate::net::wrapper::RIST_PROFILE_SIMPLE)?;
        ctx.peer_add(url)?;

        let stats = Arc::new(LinkStats::default());
        // Register stats callback (e.g. every 100ms)
        ctx.register_stats(stats.clone(), 100)?;

        ctx.start()?;
        Ok(Self {
            id,
            ctx,
            stats,
            created_at: std::time::Instant::now(),
        })
    }
}

impl LinkSender for Link {
    fn id(&self) -> usize {
        self.id
    }

    fn send(&self, packet: &[u8]) -> Result<usize> {
        self.ctx.send_data(packet)
    }

    fn get_metrics(&self) -> LinkMetrics {
        // Use smoothed values if available, else fallback to raw or 0
        let rtt_us = self.stats.smoothed_rtt_us.load(Ordering::Relaxed);
        let bw = self.stats.smoothed_bw_bps.load(Ordering::Relaxed) as f64;
        let loss_pm = self.stats.smoothed_loss_permille.load(Ordering::Relaxed);

        let rtt_ms = if rtt_us > 0 {
            rtt_us as f64 / 1000.0
        } else {
            0.0
        };

        let loss_rate = loss_pm as f64 / 1000.0;

        // Assume alive if we have RTT or if we are in startup phase (first 5 seconds)
        let alive = rtt_ms > 0.0 || self.created_at.elapsed().as_secs() < 5;

        LinkMetrics {
            rtt_ms,
            capacity_bps: bw,
            loss_rate,
            queue_depth: 0, // Need to implement if possible via stats or wrapper tracking
            max_queue: 1000,
            alive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_link_lifecycle() {
        let link = Link::new(1, "rist://127.0.0.1:5000");
        assert!(link.is_ok());
        let link = link.unwrap();

        // Test send
        let res = link.send(b"Test");
        assert!(res.is_ok());
    }
}
