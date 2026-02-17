use crate::config::{BondingConfig, LinkConfig, LinkLifecycleConfig, SchedulerConfig};
use crate::net::interface::{LinkMetrics, LinkSender};
use crate::net::link::Link;
use crate::net::transport::TransportLink;
use crate::net::wrapper::RecoveryConfig;
use crate::scheduler::bonding::BondingScheduler;
use crate::scheduler::PacketProfile;
use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use strata_transport::sender::SenderConfig;
use tracing::warn;

/// Error returned when a packet cannot be sent to the bonding worker thread.
#[derive(Debug)]
pub enum PacketSendError {
    Full,
    Disconnected,
}

enum RuntimeMessage {
    Packet(Bytes, PacketProfile),
    ApplyConfig(BondingConfig),
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
    metrics: Arc<Mutex<HashMap<usize, LinkMetrics>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl BondingRuntime {
    /// Creates a runtime with the default scheduler configuration (librist mode).
    pub fn new() -> Self {
        Self::with_config(SchedulerConfig::default())
    }

    /// Creates a runtime with the given scheduler configuration (librist mode).
    pub fn with_config(scheduler_config: SchedulerConfig) -> Self {
        Self::build(scheduler_config, false)
    }

    /// Creates a runtime using strata-transport (pure Rust) for link I/O.
    pub fn with_transport(scheduler_config: SchedulerConfig) -> Self {
        Self::build(scheduler_config, true)
    }

    fn build(scheduler_config: SchedulerConfig, use_transport: bool) -> Self {
        let channel_capacity = scheduler_config.channel_capacity;
        let (tx, rx) = bounded(channel_capacity);
        let metrics = Arc::new(Mutex::new(HashMap::new()));
        let metrics_clone = metrics.clone();

        let handle = thread::Builder::new()
            .name("rist-bond-worker".into())
            .spawn(move || runtime_worker(rx, metrics_clone, scheduler_config, use_transport))
            .expect("failed to spawn bonding runtime worker");

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
            .send(RuntimeMessage::ApplyConfig(config))
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

    /// Returns a snapshot of all link metrics (thread-safe clone).
    pub fn get_metrics(&self) -> HashMap<usize, LinkMetrics> {
        self.metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Returns a shared handle to the metrics map for external polling.
    pub fn metrics_handle(&self) -> Arc<Mutex<HashMap<usize, LinkMetrics>>> {
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
    metrics: Arc<Mutex<HashMap<usize, LinkMetrics>>>,
    scheduler_config: SchedulerConfig,
    use_transport: bool,
) {
    let mut scheduler: BondingScheduler<dyn LinkSender> =
        BondingScheduler::with_config(scheduler_config.clone());
    let mut current_links: HashMap<usize, LinkConfig> = HashMap::new();
    let mut lifecycle_config = LinkLifecycleConfig::default();
    let mut ewma_alpha = scheduler_config.ewma_alpha;

    let mut last_fast_stats = Instant::now();
    let fast_stats_interval = Duration::from_millis(100);

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => match msg {
                RuntimeMessage::Packet(data, profile) => {
                    let _ = scheduler.send(data, profile);
                }
                RuntimeMessage::AddLink(link) => {
                    apply_link(
                        &mut scheduler,
                        &mut current_links,
                        &lifecycle_config,
                        link,
                        ewma_alpha,
                        use_transport,
                    );
                }
                RuntimeMessage::RemoveLink(id) => {
                    scheduler.remove_link(id);
                    current_links.remove(&id);
                }
                RuntimeMessage::ApplyConfig(config) => {
                    lifecycle_config = config.lifecycle.clone();
                    ewma_alpha = config.scheduler.ewma_alpha;
                    let transport = config.use_transport || use_transport;
                    scheduler.update_config(config.scheduler.clone());
                    apply_config(
                        &mut scheduler,
                        &mut current_links,
                        &lifecycle_config,
                        config,
                        ewma_alpha,
                        transport,
                    );
                }
                RuntimeMessage::Shutdown => break,
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if last_fast_stats.elapsed() >= fast_stats_interval {
            scheduler.refresh_metrics();
            let all_metrics = scheduler.get_all_metrics();
            if let Ok(mut m) = metrics.lock() {
                *m = all_metrics;
            }
            last_fast_stats = Instant::now();
        }
    }
}

fn apply_config(
    scheduler: &mut BondingScheduler<dyn LinkSender>,
    current_links: &mut HashMap<usize, LinkConfig>,
    lifecycle_config: &LinkLifecycleConfig,
    config: BondingConfig,
    ewma_alpha: f64,
    use_transport: bool,
) {
    // Only reconcile links if the config explicitly defines them.
    // An empty links list means "don't touch existing links" — this allows
    // scheduler-only config updates without removing pad-configured links.
    if !config.links.is_empty() {
        let desired_ids: std::collections::HashSet<usize> =
            config.links.iter().map(|l| l.id).collect();

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
                apply_link(
                    scheduler,
                    current_links,
                    lifecycle_config,
                    link,
                    ewma_alpha,
                    use_transport,
                );
            }
        }
    }
}

fn apply_link(
    scheduler: &mut BondingScheduler<dyn LinkSender>,
    current_links: &mut HashMap<usize, LinkConfig>,
    lifecycle_config: &LinkLifecycleConfig,
    link: LinkConfig,
    ewma_alpha: f64,
    use_transport: bool,
) {
    scheduler.remove_link(link.id);

    if use_transport {
        match create_transport_link(&link) {
            Ok(tl) => {
                scheduler.add_link(Arc::new(tl) as Arc<dyn LinkSender>);
                current_links.insert(link.id, link);
            }
            Err(err) => {
                warn!(
                    "Failed to create transport link id={} uri={}: {}",
                    link.id, link.uri, err
                );
            }
        }
    } else {
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
                scheduler.add_link(Arc::new(new_link) as Arc<dyn LinkSender>);
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
}

/// Parse a RIST URI (e.g. `rist://1.2.3.4:5000`) to a `SocketAddr`.
fn parse_rist_uri(uri: &str) -> Option<SocketAddr> {
    // Try listener format first (more specific prefix)
    let stripped = uri
        .strip_prefix("rist://@")
        .or_else(|| uri.strip_prefix("rist://"))?;
    // Strip query parameters
    let host_port = stripped.split('?').next()?;
    host_port.parse::<SocketAddr>().ok()
}

/// Create a `TransportLink` from a `LinkConfig`.
fn create_transport_link(link: &LinkConfig) -> anyhow::Result<TransportLink> {
    let addr = parse_rist_uri(&link.uri)
        .ok_or_else(|| anyhow::anyhow!("Invalid URI for transport mode: {}", link.uri))?;

    let socket = if let Some(ref iface) = link.interface {
        let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let sock = UdpSocket::bind(bind_addr)?;
        // Bind to specific interface via SO_BINDTODEVICE
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let fd = sock.as_raw_fd();
            let iface_bytes = iface.as_bytes();
            unsafe {
                let ret = libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_BINDTODEVICE,
                    iface_bytes.as_ptr() as *const libc::c_void,
                    iface_bytes.len() as libc::socklen_t,
                );
                if ret != 0 {
                    warn!(
                        "SO_BINDTODEVICE failed for link {} iface {}: {}",
                        link.id,
                        iface,
                        std::io::Error::last_os_error()
                    );
                }
            }
        }
        sock
    } else {
        UdpSocket::bind("0.0.0.0:0")?
    };

    socket.connect(addr)?;
    Ok(TransportLink::new(link.id, socket, SenderConfig::default()))
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
        let m = handle.lock().unwrap();
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
    fn parse_rist_uri_basic() {
        let addr = parse_rist_uri("rist://127.0.0.1:5000").unwrap();
        assert_eq!(addr, "127.0.0.1:5000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_rist_uri_listener() {
        let addr = parse_rist_uri("rist://@0.0.0.0:5000").unwrap();
        assert_eq!(addr, "0.0.0.0:5000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_rist_uri_with_query() {
        let addr = parse_rist_uri("rist://10.0.0.1:6000?miface=eth0").unwrap();
        assert_eq!(addr, "10.0.0.1:6000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_rist_uri_invalid() {
        assert!(parse_rist_uri("http://example.com").is_none());
        assert!(parse_rist_uri("").is_none());
        assert!(parse_rist_uri("not-a-url").is_none());
    }

    #[test]
    fn transport_runtime_creates_links() {
        let rt = BondingRuntime::with_transport(SchedulerConfig::default());
        let link = LinkConfig {
            id: 1,
            uri: "rist://127.0.0.1:19200".to_string(),
            interface: None,
            recovery_maxbitrate: None,
            recovery_rtt_max: None,
            recovery_reorder_buffer: None,
        };
        assert!(rt.add_link(link).is_ok());
        thread::sleep(Duration::from_millis(250));
        let metrics = rt.get_metrics();
        assert!(
            metrics.contains_key(&1),
            "Transport link 1 should appear in metrics"
        );
    }

    #[test]
    fn transport_runtime_sends_packets() {
        let rt = BondingRuntime::with_transport(SchedulerConfig::default());

        // Bind a receiver socket to get a known port
        let rcv_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let rcv_addr = rcv_socket.local_addr().unwrap();

        let link = LinkConfig {
            id: 1,
            uri: format!("rist://{}", rcv_addr),
            interface: None,
            recovery_maxbitrate: None,
            recovery_rtt_max: None,
            recovery_reorder_buffer: None,
        };
        rt.add_link(link).unwrap();
        thread::sleep(Duration::from_millis(200));

        // Send a packet
        let data = Bytes::from_static(b"transport test");
        rt.try_send_packet(data, PacketProfile::default()).unwrap();
        thread::sleep(Duration::from_millis(200));

        // Verify the receiver socket got data
        rcv_socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut buf = [0u8; 4096];
        let result = rcv_socket.recv(&mut buf);
        assert!(result.is_ok(), "Should have received UDP data");
        assert!(result.unwrap() > 0, "Should have received non-empty data");
    }
}
