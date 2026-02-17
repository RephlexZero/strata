use crate::config::{BondingConfig, LinkConfig, SchedulerConfig};
use crate::metrics::MetricsServer;
use crate::net::interface::{LinkMetrics, LinkSender};
use crate::net::transport::TransportLink;
use crate::scheduler::bonding::BondingScheduler;
use crate::scheduler::PacketProfile;
use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use quanta::Instant;
use std::time::Duration;
use strata_transport::sender::SenderConfig;
use tracing::warn;

/// Error returned when a packet cannot be sent to the bonding worker thread.
#[derive(Debug)]
pub enum PacketSendError {
    Full,
    Disconnected,
}

/// Hot-path packet data sent via lock-free SPSC ring buffer.
struct PacketData {
    data: Bytes,
    profile: PacketProfile,
}

/// Control messages sent via crossbeam channel (infrequent).
enum ControlMessage {
    ApplyConfig(Box<BondingConfig>),
    AddLink(LinkConfig),
    RemoveLink(usize),
    Shutdown,
}

/// Thread-safe handle to the bonding scheduler worker.
///
/// Owns a background thread that runs the [`BondingScheduler`]
/// loop, processing packets, applying configuration changes, and refreshing
/// link metrics.
///
/// **Hot path** (packets) uses a lock-free SPSC ring buffer (`rtrb`) for
/// minimal latency. **Control path** (config, link changes, shutdown) uses
/// a bounded crossbeam channel.
///
/// Dropping the runtime triggers a graceful shutdown of the worker thread.
pub struct BondingRuntime {
    packet_tx: rtrb::Producer<PacketData>,
    control_tx: Sender<ControlMessage>,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<Mutex<HashMap<usize, LinkMetrics>>>,
    handle: Option<thread::JoinHandle<()>>,
    metrics_server: Option<MetricsServer>,
}

impl BondingRuntime {
    /// Creates a runtime with the default scheduler configuration.
    pub fn new() -> Self {
        Self::with_config(SchedulerConfig::default())
    }

    /// Creates a runtime with the given scheduler configuration.
    pub fn with_config(scheduler_config: SchedulerConfig) -> Self {
        let channel_capacity = scheduler_config.channel_capacity;
        let (packet_tx, packet_rx) = rtrb::RingBuffer::new(channel_capacity);
        let (control_tx, control_rx) = bounded(64);
        let metrics = Arc::new(Mutex::new(HashMap::new()));
        let metrics_clone = metrics.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = thread::Builder::new()
            .name("strata-worker".into())
            .spawn(move || {
                runtime_worker(packet_rx, control_rx, metrics_clone, scheduler_config, shutdown_clone)
            })
            .expect("failed to spawn bonding runtime worker");

        Self {
            packet_tx,
            control_tx,
            shutdown,
            metrics,
            handle: Some(handle),
            metrics_server: None,
        }
    }

    /// Enqueues a packet for transmission. Returns immediately.
    ///
    /// Returns `PacketSendError::Full` if the internal ring buffer is saturated,
    /// or `PacketSendError::Disconnected` if the worker thread has exited.
    pub fn try_send_packet(
        &mut self,
        data: Bytes,
        profile: PacketProfile,
    ) -> Result<(), PacketSendError> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(PacketSendError::Disconnected);
        }
        match self.packet_tx.push(PacketData { data, profile }) {
            Ok(()) => Ok(()),
            Err(_) => Err(PacketSendError::Full),
        }
    }

    /// Sends a full configuration update to the worker thread.
    pub fn apply_config(&self, config: BondingConfig) -> anyhow::Result<()> {
        self.control_tx
            .send(ControlMessage::ApplyConfig(Box::new(config)))
            .map_err(|e| anyhow::anyhow!("Failed to send config: {}", e))
    }

    /// Adds a single link dynamically at runtime.
    pub fn add_link(&self, link: LinkConfig) -> anyhow::Result<()> {
        self.control_tx
            .send(ControlMessage::AddLink(link))
            .map_err(|e| anyhow::anyhow!("Failed to add link: {}", e))
    }

    /// Removes a link by ID at runtime.
    pub fn remove_link(&self, id: usize) -> anyhow::Result<()> {
        self.control_tx
            .send(ControlMessage::RemoveLink(id))
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

    /// Start a Prometheus-compatible HTTP metrics server on the given address.
    ///
    /// The server responds to `GET /metrics` with Prometheus text exposition
    /// format containing all link metrics. Calling this multiple times replaces
    /// the previous server.
    ///
    /// Returns the actual bound address (useful when binding to port 0).
    pub fn start_metrics_server(&mut self, addr: SocketAddr) -> std::io::Result<SocketAddr> {
        // Stop existing server if any
        if let Some(mut old) = self.metrics_server.take() {
            old.stop();
        }
        let server = MetricsServer::start(addr, self.metrics.clone())?;
        let bound = server.addr();
        self.metrics_server = Some(server);
        Ok(bound)
    }

    /// Gracefully shuts down the worker thread. Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(mut server) = self.metrics_server.take() {
            server.stop();
        }
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.control_tx.send(ControlMessage::Shutdown);
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
    mut packet_rx: rtrb::Consumer<PacketData>,
    control_rx: Receiver<ControlMessage>,
    metrics: Arc<Mutex<HashMap<usize, LinkMetrics>>>,
    scheduler_config: SchedulerConfig,
    shutdown: Arc<AtomicBool>,
) {
    let mut scheduler: BondingScheduler<dyn LinkSender> =
        BondingScheduler::with_config(scheduler_config.clone());
    let mut current_links: HashMap<usize, LinkConfig> = HashMap::new();

    let mut last_fast_stats = Instant::now();
    let fast_stats_interval = Duration::from_millis(100);

    loop {
        // Hot path: drain all available packets from SPSC ring buffer
        while let Ok(pkt) = packet_rx.pop() {
            let _ = scheduler.send(pkt.data, pkt.profile);
        }

        // Control path: process one control message (non-blocking)
        match control_rx.try_recv() {
            Ok(msg) => match msg {
                ControlMessage::AddLink(link) => {
                    apply_link(&mut scheduler, &mut current_links, link);
                }
                ControlMessage::RemoveLink(id) => {
                    scheduler.remove_link(id);
                    current_links.remove(&id);
                }
                ControlMessage::ApplyConfig(config) => {
                    scheduler.update_config(config.scheduler.clone());
                    apply_config(&mut scheduler, &mut current_links, *config);
                }
                ControlMessage::Shutdown => break,
            },
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => break,
        }

        // If no work was available, yield CPU briefly to avoid busy-spin
        if packet_rx.slots() == 0 && shutdown.load(Ordering::Relaxed) {
            break;
        }

        if last_fast_stats.elapsed() >= fast_stats_interval {
            scheduler.refresh_metrics();
            let all_metrics = scheduler.get_all_metrics();
            if let Ok(mut m) = metrics.lock() {
                *m = all_metrics;
            }
            last_fast_stats = Instant::now();
        }

        // Brief yield when idle to avoid burning CPU
        if packet_rx.slots() == 0 {
            thread::sleep(Duration::from_micros(100));
        }
    }
}

fn apply_config(
    scheduler: &mut BondingScheduler<dyn LinkSender>,
    current_links: &mut HashMap<usize, LinkConfig>,
    config: BondingConfig,
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
                apply_link(scheduler, current_links, link);
            }
        }
    }
}

fn apply_link(
    scheduler: &mut BondingScheduler<dyn LinkSender>,
    current_links: &mut HashMap<usize, LinkConfig>,
    link: LinkConfig,
) {
    scheduler.remove_link(link.id);

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
}

/// Parse a URI (e.g. `rist://1.2.3.4:5000` or `1.2.3.4:5000`) to a `SocketAddr`.
fn parse_uri(uri: &str) -> Option<SocketAddr> {
    // Strip legacy rist:// prefix if present
    let stripped = uri
        .strip_prefix("rist://@")
        .or_else(|| uri.strip_prefix("rist://"))
        .unwrap_or(uri);
    // Strip query parameters
    let host_port = stripped.split('?').next()?;
    host_port.parse::<SocketAddr>().ok()
}

/// Create a `TransportLink` from a `LinkConfig`.
fn create_transport_link(link: &LinkConfig) -> anyhow::Result<TransportLink> {
    let addr = parse_uri(&link.uri)
        .ok_or_else(|| anyhow::anyhow!("Invalid URI for transport: {}", link.uri))?;

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

    // Enable SO_BUSY_POLL for reduced latency on the hot path
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = socket.as_raw_fd();
        let poll_us: libc::c_int = 50; // 50µs busy-poll budget
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_BUSY_POLL,
                &poll_us as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    socket.connect(addr)?;
    Ok(TransportLink::new(link.id, socket, SenderConfig::default()))
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
        let mut rt = BondingRuntime::with_config(cfg);

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
            uri: "127.0.0.1:19100".to_string(),
            interface: None,
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
            uri: "127.0.0.1:19101".to_string(),
            interface: None,
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
                    uri: "127.0.0.1:19102".to_string(),
                    interface: None,
                },
                LinkConfig {
                    id: 2,
                    uri: "127.0.0.1:19103".to_string(),
                    interface: None,
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
                uri: "127.0.0.1:19103".to_string(),
                interface: None,
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
    fn start_metrics_server_binds_and_serves() {
        let mut rt = BondingRuntime::new();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let bound = rt.start_metrics_server(addr).expect("should bind");
        assert_ne!(bound.port(), 0, "should assign a real port");
        // Clean shutdown should stop the server
        rt.shutdown();
    }

    #[test]
    fn parse_uri_basic() {
        let addr = parse_uri("127.0.0.1:5000").unwrap();
        assert_eq!(addr, "127.0.0.1:5000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_uri_legacy_rist() {
        let addr = parse_uri("rist://127.0.0.1:5000").unwrap();
        assert_eq!(addr, "127.0.0.1:5000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_uri_legacy_rist_listener() {
        let addr = parse_uri("rist://@0.0.0.0:5000").unwrap();
        assert_eq!(addr, "0.0.0.0:5000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_uri_with_query() {
        let addr = parse_uri("rist://10.0.0.1:6000?miface=eth0").unwrap();
        assert_eq!(addr, "10.0.0.1:6000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_uri_invalid() {
        assert!(parse_uri("").is_none());
        assert!(parse_uri("not-a-url").is_none());
    }

    #[test]
    fn transport_runtime_creates_links() {
        let rt = BondingRuntime::new();
        let link = LinkConfig {
            id: 1,
            uri: "127.0.0.1:19200".to_string(),
            interface: None,
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
        let mut rt = BondingRuntime::new();

        // Bind a receiver socket to get a known port
        let rcv_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let rcv_addr = rcv_socket.local_addr().unwrap();

        let link = LinkConfig {
            id: 1,
            uri: format!("{}", rcv_addr),
            interface: None,
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
