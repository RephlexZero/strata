use crate::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use crate::net::state::LinkStats;
use crate::net::wrapper::RistContext;
use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const OS_POLL_INTERVAL_MS: u64 = 500;

pub struct Link {
    pub id: usize,
    ctx: RistContext,
    stats: Arc<LinkStats>,
    created_at: std::time::Instant,
    iface: Option<String>,
    link_kind: Option<String>,
}

impl Link {
    pub fn new(id: usize, url: &str) -> Result<Self> {
        Self::new_with_iface(id, url, None, crate::config::LinkLifecycleConfig::default())
    }

    pub fn new_with_iface(
        id: usize,
        url: &str,
        iface: Option<String>,
        lifecycle_config: crate::config::LinkLifecycleConfig,
    ) -> Result<Self> {
        let mut ctx = RistContext::new(crate::net::wrapper::RIST_PROFILE_SIMPLE)?;
        ctx.peer_add(url)?;

        let stats = Arc::new(LinkStats::new(lifecycle_config));
        // Register stats callback (e.g. every 100ms)
        ctx.register_stats(stats.clone(), 100)?;

        ctx.start()?;
        let link_kind = iface
            .as_deref()
            .and_then(infer_kind_from_iface_name)
            .map(|v| v.to_string());
        Ok(Self {
            id,
            ctx,
            stats,
            created_at: std::time::Instant::now(),
            iface,
            link_kind,
        })
    }
}

fn infer_kind_from_iface_name(iface: &str) -> Option<&'static str> {
    let name = iface.to_ascii_lowercase();
    if name == "lo" {
        return Some("loopback");
    }
    if name.starts_with("wlan") || name.starts_with("wifi") {
        return Some("wifi");
    }
    if name.starts_with("wwan") || name.starts_with("rmnet") || name.starts_with("cdc") {
        return Some("cellular");
    }
    if name.starts_with("eth") || name.starts_with("en") {
        return Some("wired");
    }
    None
}

fn parse_operstate(contents: &str) -> Option<bool> {
    match contents.trim() {
        "up" => Some(true),
        "down" => Some(false),
        _ => None,
    }
}

fn parse_mtu(contents: &str) -> Option<u32> {
    contents.trim().parse::<u32>().ok()
}

fn should_poll_os(now_ms: u64, last_poll_ms: u64) -> bool {
    last_poll_ms == 0 || now_ms.saturating_sub(last_poll_ms) >= OS_POLL_INTERVAL_MS
}

impl LinkSender for Link {
    fn id(&self) -> usize {
        self.id
    }

    fn send(&self, packet: &[u8]) -> Result<usize> {
        self.ctx.send_data(packet)
    }

    fn get_metrics(&self) -> LinkMetrics {
        let now = std::time::Instant::now();
        // Use smoothed values if available, else fallback to raw
        let rtt_us = self.stats.smoothed_rtt_us.load(Ordering::Relaxed);
        let raw_rtt_ms = self.stats.rtt.load(Ordering::Relaxed) as f64;
        let raw_bw = self.stats.bandwidth.load(Ordering::Relaxed) as f64;
        let bw = self.stats.smoothed_bw_bps.load(Ordering::Relaxed) as f64;
        let loss_pm = self.stats.smoothed_loss_permille.load(Ordering::Relaxed);

        let rtt_ms = if rtt_us > 0 {
            rtt_us as f64 / 1000.0
        } else {
            raw_rtt_ms
        };

        let bw = if bw > 0.0 { bw } else { raw_bw };

        let loss_rate = loss_pm as f64 / 1000.0;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last_stats_ms = self.stats.last_stats_ms.load(Ordering::Relaxed);
        let stats_age = if last_stats_ms == 0 {
            Duration::from_secs(9999)
        } else {
            Duration::from_millis(now_ms.saturating_sub(last_stats_ms))
        };

        let mut phase = LinkPhase::Init;
        if let Ok(mut lifecycle) = self.stats.lifecycle.lock() {
            phase = lifecycle.update(now, rtt_ms, loss_rate, bw, stats_age);
        }

        let alive = matches!(
            phase,
            LinkPhase::Probe | LinkPhase::Warm | LinkPhase::Live | LinkPhase::Degrade
        ) || self.created_at.elapsed().as_secs() < 5;

        let (os_up, mtu, iface_name, link_kind) = if let Some(iface) = &self.iface {
            let last_poll_ms = self.stats.os_last_poll_ms.load(Ordering::Relaxed);
            if should_poll_os(now_ms, last_poll_ms) {
                let operstate_path = format!("/sys/class/net/{}/operstate", iface);
                let mtu_path = format!("/sys/class/net/{}/mtu", iface);
                let os_up = std::fs::read_to_string(operstate_path)
                    .ok()
                    .and_then(|s| parse_operstate(&s));
                let mtu = std::fs::read_to_string(mtu_path)
                    .ok()
                    .and_then(|s| parse_mtu(&s));
                let os_up_i32 = match os_up {
                    Some(true) => 1,
                    Some(false) => 0,
                    None => -1,
                };
                let mtu_i32 = mtu.map(|v| v as i32).unwrap_or(-1);
                self.stats.os_up_i32.store(os_up_i32, Ordering::Relaxed);
                self.stats.mtu_i32.store(mtu_i32, Ordering::Relaxed);
                self.stats.os_last_poll_ms.store(now_ms, Ordering::Relaxed);
            }

            let os_up = match self.stats.os_up_i32.load(Ordering::Relaxed) {
                1 => Some(true),
                0 => Some(false),
                _ => None,
            };
            let mtu = match self.stats.mtu_i32.load(Ordering::Relaxed) {
                v if v > 0 => Some(v as u32),
                _ => None,
            };
            let link_kind = self
                .link_kind
                .as_ref()
                .cloned()
                .or_else(|| infer_kind_from_iface_name(iface).map(|v| v.to_string()));
            (os_up, mtu, Some(iface.clone()), link_kind)
        } else {
            (None, None, None, None)
        };

        LinkMetrics {
            rtt_ms,
            capacity_bps: bw,
            loss_rate,
            observed_bps: 0.0,
            observed_bytes: 0,
            queue_depth: 0, // Need to implement if possible via stats or wrapper tracking
            max_queue: 1000,
            alive,
            phase,
            os_up,
            mtu,
            iface: iface_name,
            link_kind,
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

    #[test]
    fn test_parse_operstate() {
        assert_eq!(parse_operstate("up\n"), Some(true));
        assert_eq!(parse_operstate("down"), Some(false));
        assert_eq!(parse_operstate("unknown"), None);
        assert_eq!(parse_operstate(""), None);
    }

    #[test]
    fn test_parse_mtu() {
        assert_eq!(parse_mtu("1500\n"), Some(1500));
        assert_eq!(parse_mtu("9000"), Some(9000));
        assert_eq!(parse_mtu("not-a-number"), None);
    }

    #[test]
    fn test_infer_kind_from_iface_name() {
        assert_eq!(infer_kind_from_iface_name("lo"), Some("loopback"));
        assert_eq!(infer_kind_from_iface_name("wlan0"), Some("wifi"));
        assert_eq!(infer_kind_from_iface_name("wwan0"), Some("cellular"));
        assert_eq!(infer_kind_from_iface_name("rmnet0"), Some("cellular"));
        assert_eq!(infer_kind_from_iface_name("eth0"), Some("wired"));
        assert_eq!(infer_kind_from_iface_name("enp3s0"), Some("wired"));
        assert_eq!(infer_kind_from_iface_name("foo0"), None);
    }

    #[test]
    fn test_should_poll_os() {
        assert!(should_poll_os(1000, 0));
        assert!(!should_poll_os(1000, 900));
        assert!(should_poll_os(1500, 900));
        assert!(should_poll_os(2000, 1499));
    }
}
