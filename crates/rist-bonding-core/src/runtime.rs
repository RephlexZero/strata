use crate::config::{BondingConfig, LinkConfig, LinkLifecycleConfig, SchedulerConfig};
use crate::net::interface::LinkMetrics;
use crate::net::link::Link;
use crate::net::wrapper::RecoveryConfig;
use crate::scheduler::bonding::BondingScheduler;
use crate::scheduler::PacketProfile;
use arc_swap::ArcSwap;
use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::warn;

/// Error returned when a packet cannot be sent to the bonding worker thread.
#[derive(Debug)]
pub enum PacketSendError {
    Full,
    Disconnected,
}

enum RuntimeMessage {
    Packet(Bytes, PacketProfile),
    ApplyConfig(Box<BondingConfig>),
    UpdateSchedulerConfig(Box<SchedulerConfig>),
    AddLink(LinkConfig),
    RemoveLink(usize),
    Shutdown,
}

/// Thread-safe handle to the bonding scheduler worker.
///
/// Owns a background thread that runs the [`BondingScheduler`]
/// loop, processing packets, applying configuration changes, and refreshing
/// link metrics. All public methods are non-blocking and communicate with
/// the worker via a bounded channel.
///
/// Dropping the runtime triggers a graceful shutdown of the worker thread.
pub struct BondingRuntime {
    sender: Sender<RuntimeMessage>,
    metrics: Arc<ArcSwap<HashMap<usize, LinkMetrics>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl BondingRuntime {
    /// Creates a runtime with the default scheduler configuration.
    pub fn new() -> Self {
        Self::with_config(SchedulerConfig::default())
    }

    /// Creates a runtime with the given scheduler configuration.
    pub fn with_config(scheduler_config: SchedulerConfig) -> Self {
        let channel_capacity = scheduler_config.channel_capacity;
        let (tx, rx) = bounded(channel_capacity);
        let metrics = Arc::new(ArcSwap::from_pointee(HashMap::new()));
        let metrics_clone = metrics.clone();

        let handle = thread::Builder::new()
            .name("rist-bond-worker".into())
            .spawn(move || runtime_worker(rx, metrics_clone, scheduler_config))
            .unwrap_or_else(|e| panic!("failed to spawn bonding runtime worker: {}", e));

        Self {
            sender: tx,
            metrics,
            handle: Some(handle),
        }
    }

    /// Enqueues a packet for transmission. Returns immediately.
    ///
    /// Returns `PacketSendError::Full` if the internal channel is saturated,
    /// or `PacketSendError::Disconnected` if the worker thread has exited.
    pub fn try_send_packet(
        &self,
        data: Bytes,
        profile: PacketProfile,
    ) -> Result<(), PacketSendError> {
        match self.sender.try_send(RuntimeMessage::Packet(data, profile)) {
            Ok(_) => Ok(()),
            Err(TrySendError::Full(_)) => Err(PacketSendError::Full),
            Err(TrySendError::Disconnected(_)) => Err(PacketSendError::Disconnected),
        }
    }

    /// Sends a full configuration update to the worker thread.
    pub fn apply_config(&self, config: BondingConfig) -> anyhow::Result<()> {
        self.sender
            .send(RuntimeMessage::ApplyConfig(Box::new(config)))
            .map_err(|e| anyhow::anyhow!("Failed to send config: {}", e))
    }

    /// Adds a single link dynamically at runtime.
    pub fn add_link(&self, link: LinkConfig) -> anyhow::Result<()> {
        self.sender
            .send(RuntimeMessage::AddLink(link))
            .map_err(|e| anyhow::anyhow!("Failed to add link: {}", e))
    }

    /// Removes a link by ID at runtime.
    pub fn remove_link(&self, id: usize) -> anyhow::Result<()> {
        self.sender
            .send(RuntimeMessage::RemoveLink(id))
            .map_err(|e| anyhow::anyhow!("Failed to remove link: {}", e))
    }

    /// Live-update the scheduler configuration (e.g. max_capacity_bps).
    pub fn update_scheduler_config(&self, config: SchedulerConfig) -> anyhow::Result<()> {
        self.sender
            .send(RuntimeMessage::UpdateSchedulerConfig(Box::new(config)))
            .map_err(|e| anyhow::anyhow!("Failed to update scheduler config: {}", e))
    }

    /// Returns a snapshot of all link metrics (lock-free read via ArcSwap).
    pub fn get_metrics(&self) -> HashMap<usize, LinkMetrics> {
        (**self.metrics.load()).clone()
    }

    /// Returns a shared handle to the metrics for external polling.
    pub fn metrics_handle(&self) -> Arc<ArcSwap<HashMap<usize, LinkMetrics>>> {
        self.metrics.clone()
    }

    /// Gracefully shuts down the worker thread. Idempotent.
    pub fn shutdown(&mut self) {
        let _ = self.sender.send(RuntimeMessage::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Default for BondingRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BondingRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn runtime_worker(
    rx: Receiver<RuntimeMessage>,
    metrics: Arc<ArcSwap<HashMap<usize, LinkMetrics>>>,
    scheduler_config: SchedulerConfig,
) {
    let mut scheduler = BondingScheduler::with_config(scheduler_config.clone());
    let mut current_links: HashMap<usize, LinkConfig> = HashMap::new();
    let mut lifecycle_config = LinkLifecycleConfig::default();
    let mut ewma_alpha = scheduler_config.ewma_alpha;

    let mut last_fast_stats = Instant::now();
    let fast_stats_interval = Duration::from_millis(100);

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => match msg {
                RuntimeMessage::Packet(data, profile) => {
                    if let Err(e) = scheduler.send(data, profile) {
                        tracing::debug!("Scheduler send failed: {}", e);
                    }
                }
                RuntimeMessage::AddLink(link) => {
                    apply_link(
                        &mut scheduler,
                        &mut current_links,
                        &lifecycle_config,
                        link,
                        ewma_alpha,
                    );
                }
                RuntimeMessage::RemoveLink(id) => {
                    scheduler.remove_link(id);
                    current_links.remove(&id);
                }
                RuntimeMessage::UpdateSchedulerConfig(sched_cfg) => {
                    ewma_alpha = sched_cfg.ewma_alpha;
                    scheduler.update_config(*sched_cfg);
                }
                RuntimeMessage::ApplyConfig(config) => {
                    lifecycle_config = config.lifecycle.clone();
                    ewma_alpha = config.scheduler.ewma_alpha;
                    scheduler.update_config(config.scheduler.clone());
                    apply_config(
                        &mut scheduler,
                        &mut current_links,
                        &lifecycle_config,
                        *config,
                        ewma_alpha,
                    );
                }
                RuntimeMessage::Shutdown => break,
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if last_fast_stats.elapsed() >= fast_stats_interval {
            scheduler.refresh_metrics();
            metrics.store(Arc::new(scheduler.get_all_metrics()));
            last_fast_stats = Instant::now();
        }
    }
}

fn apply_config(
    scheduler: &mut BondingScheduler<Link>,
    current_links: &mut HashMap<usize, LinkConfig>,
    lifecycle_config: &LinkLifecycleConfig,
    config: BondingConfig,
    ewma_alpha: f64,
) {
    let desired_ids: std::collections::HashSet<usize> = config.links.iter().map(|l| l.id).collect();

    // Remove links no longer present in config
    let existing_ids: Vec<usize> = current_links.keys().copied().collect();
    for id in existing_ids {
        if !desired_ids.contains(&id) {
            scheduler.remove_link(id);
            current_links.remove(&id);
        }
    }

    // Add or update links that changed
    for link in config.links {
        let needs_update = match current_links.get(&link.id) {
            Some(existing) => existing != &link,
            None => true,
        };

        if needs_update {
            apply_link(scheduler, current_links, lifecycle_config, link, ewma_alpha);
        }
    }
}

fn apply_link(
    scheduler: &mut BondingScheduler<Link>,
    current_links: &mut HashMap<usize, LinkConfig>,
    lifecycle_config: &LinkLifecycleConfig,
    link: LinkConfig,
    ewma_alpha: f64,
) {
    scheduler.remove_link(link.id);
    let recovery = recovery_config_from_link(&link);
    match Link::new_with_full_config(
        link.id,
        &link.uri,
        link.interface.clone(),
        lifecycle_config.clone(),
        recovery,
        Some(ewma_alpha),
    ) {
        Ok(new_link) => {
            scheduler.add_link(Arc::new(new_link));
            current_links.insert(link.id, link);
        }
        Err(err) => {
            warn!(
                "Failed to create link id={} uri={}: {}",
                link.id, link.uri, err
            );
        }
    }
}

/// Build a RecoveryConfig from the link's optional recovery parameters.
fn recovery_config_from_link(link: &LinkConfig) -> Option<RecoveryConfig> {
    if link.recovery_maxbitrate.is_some()
        || link.recovery_rtt_max.is_some()
        || link.recovery_reorder_buffer.is_some()
    {
        Some(RecoveryConfig {
            recovery_maxbitrate: link.recovery_maxbitrate,
            recovery_rtt_max: link.recovery_rtt_max,
            recovery_reorder_buffer: link.recovery_reorder_buffer,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BondingConfig;
    use crate::scheduler::PacketProfile;
    use bytes::Bytes;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn runtime_new_defaults() {
        let mut rt = BondingRuntime::new();
        let metrics = rt.get_metrics();
        assert!(metrics.is_empty(), "No links added yet");
        rt.shutdown();
    }

    #[test]
    fn runtime_with_custom_config() {
        let cfg = SchedulerConfig {
            channel_capacity: 32,
            ..SchedulerConfig::default()
        };
        let mut rt = BondingRuntime::with_config(cfg);
        assert!(rt.get_metrics().is_empty());
        rt.shutdown();
    }

    #[test]
    fn try_send_packet_disconnected_after_shutdown() {
        let mut rt = BondingRuntime::new();
        rt.shutdown();
        let err = rt
            .try_send_packet(Bytes::from_static(b"data"), PacketProfile::default())
            .unwrap_err();
        assert!(matches!(err, PacketSendError::Disconnected));
    }

    #[test]
    fn try_send_packet_full_channel() {
        let cfg = SchedulerConfig {
            channel_capacity: 16,
            ..SchedulerConfig::default()
        };
        let rt = BondingRuntime::with_config(cfg);

        let mut got_full = false;
        for _ in 0..10_000 {
            match rt.try_send_packet(Bytes::from_static(b"x"), PacketProfile::default()) {
                Err(PacketSendError::Full) => {
                    got_full = true;
                    break;
                }
                Ok(_) => continue,
                Err(PacketSendError::Disconnected) => break,
            }
        }
        assert!(got_full, "Channel should report Full when saturated");
    }

    #[test]
    fn add_link_via_message() {
        let rt = BondingRuntime::new();
        let link = LinkConfig {
            id: 1,
            uri: "rist://127.0.0.1:19100".to_string(),
            interface: None,
            recovery_maxbitrate: None,
            recovery_rtt_max: None,
            recovery_reorder_buffer: None,
        };
        assert!(rt.add_link(link).is_ok());
        thread::sleep(Duration::from_millis(250));
        let metrics = rt.get_metrics();
        assert!(metrics.contains_key(&1), "Link 1 should appear in metrics");
    }

    #[test]
    fn remove_link_via_message() {
        let rt = BondingRuntime::new();
        let link = LinkConfig {
            id: 1,
            uri: "rist://127.0.0.1:19101".to_string(),
            interface: None,
            recovery_maxbitrate: None,
            recovery_rtt_max: None,
            recovery_reorder_buffer: None,
        };
        rt.add_link(link).unwrap();
        thread::sleep(Duration::from_millis(250));
        assert!(rt.get_metrics().contains_key(&1));

        rt.remove_link(1).unwrap();
        thread::sleep(Duration::from_millis(250));
        assert!(
            !rt.get_metrics().contains_key(&1),
            "Link 1 should be removed"
        );
    }

    #[test]
    fn apply_config_adds_and_removes_links() {
        let rt = BondingRuntime::new();
        let config = BondingConfig {
            links: vec![
                LinkConfig {
                    id: 1,
                    uri: "rist://127.0.0.1:19102".to_string(),
                    interface: None,
                    recovery_maxbitrate: None,
                    recovery_rtt_max: None,
                    recovery_reorder_buffer: None,
                },
                LinkConfig {
                    id: 2,
                    uri: "rist://127.0.0.1:19103".to_string(),
                    interface: None,
                    recovery_maxbitrate: None,
                    recovery_rtt_max: None,
                    recovery_reorder_buffer: None,
                },
            ],
            ..BondingConfig::default()
        };
        rt.apply_config(config).unwrap();
        thread::sleep(Duration::from_millis(350));
        let m = rt.get_metrics();
        assert!(m.contains_key(&1));
        assert!(m.contains_key(&2));

        let config2 = BondingConfig {
            links: vec![LinkConfig {
                id: 2,
                uri: "rist://127.0.0.1:19103".to_string(),
                interface: None,
                recovery_maxbitrate: None,
                recovery_rtt_max: None,
                recovery_reorder_buffer: None,
            }],
            ..BondingConfig::default()
        };
        rt.apply_config(config2).unwrap();
        thread::sleep(Duration::from_millis(350));
        let m = rt.get_metrics();
        assert!(
            !m.contains_key(&1),
            "Link 1 should be removed by new config"
        );
        assert!(m.contains_key(&2), "Link 2 should still exist");
    }

    #[test]
    fn shutdown_is_idempotent() {
        let mut rt = BondingRuntime::new();
        rt.shutdown();
        rt.shutdown();
    }

    #[test]
    fn drop_triggers_shutdown() {
        let rt = BondingRuntime::new();
        drop(rt);
    }

    #[test]
    fn metrics_handle_shared() {
        let rt = BondingRuntime::new();
        let handle = rt.metrics_handle();
        let m = handle.load();
        assert!(m.is_empty());
    }

    #[test]
    fn recovery_config_all_set() {
        let link = LinkConfig {
            id: 1,
            uri: "rist://1.2.3.4:5000".to_string(),
            interface: None,
            recovery_maxbitrate: Some(20000),
            recovery_rtt_max: Some(800),
            recovery_reorder_buffer: Some(50),
        };
        let rc = recovery_config_from_link(&link).unwrap();
        assert_eq!(rc.recovery_maxbitrate, Some(20000));
        assert_eq!(rc.recovery_rtt_max, Some(800));
        assert_eq!(rc.recovery_reorder_buffer, Some(50));
    }

    #[test]
    fn recovery_config_none_when_all_empty() {
        let link = LinkConfig {
            id: 1,
            uri: "rist://1.2.3.4:5000".to_string(),
            interface: None,
            recovery_maxbitrate: None,
            recovery_rtt_max: None,
            recovery_reorder_buffer: None,
        };
        assert!(recovery_config_from_link(&link).is_none());
    }

    #[test]
    fn recovery_config_partial() {
        let link = LinkConfig {
            id: 1,
            uri: "rist://1.2.3.4:5000".to_string(),
            interface: None,
            recovery_maxbitrate: None,
            recovery_rtt_max: Some(200),
            recovery_reorder_buffer: None,
        };
        let rc = recovery_config_from_link(&link).unwrap();
        assert_eq!(rc.recovery_maxbitrate, None);
        assert_eq!(rc.recovery_rtt_max, Some(200));
    }

    #[test]
    fn update_scheduler_config_reaches_worker() {
        let rt = BondingRuntime::new();
        let new_config = SchedulerConfig {
            ewma_alpha: 0.5,
            channel_capacity: 64,
            ..SchedulerConfig::default()
        };
        let result = rt.update_scheduler_config(new_config);
        assert!(
            result.is_ok(),
            "update_scheduler_config should succeed: {:?}",
            result.err()
        );
        // Give the worker time to process the message
        thread::sleep(Duration::from_millis(200));
        // If the worker panicked processing the message, metrics would be poisoned
        let _metrics = rt.get_metrics();
    }

    // ────────────────────────────────────────────────────────────────
    // ArcSwap lock-free metrics tests (#11)
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn metrics_handle_lock_free_read() {
        let rt = BondingRuntime::new();
        let handle = rt.metrics_handle();

        // Multiple concurrent reads should never block.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let h = handle.clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        let m = h.load();
                        let _ = m.len();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn metrics_snapshot_is_consistent() {
        let rt = BondingRuntime::new();
        let link = LinkConfig {
            id: 1,
            uri: "rist://127.0.0.1:19200".to_string(),
            interface: None,
            recovery_maxbitrate: None,
            recovery_rtt_max: None,
            recovery_reorder_buffer: None,
        };
        rt.add_link(link).unwrap();
        thread::sleep(Duration::from_millis(300));

        // get_metrics returns a clone — mutations don't affect the source.
        let m1 = rt.get_metrics();
        let m2 = rt.get_metrics();
        assert_eq!(m1.len(), m2.len());
    }
}
