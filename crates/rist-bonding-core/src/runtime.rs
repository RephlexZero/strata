use crate::config::{BondingConfig, LinkConfig, LinkLifecycleConfig};
use crate::net::interface::LinkMetrics;
use crate::net::link::Link;
use crate::scheduler::bonding::BondingScheduler;
use crate::scheduler::PacketProfile;
use bytes::Bytes;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::warn;

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

pub struct BondingRuntime {
    sender: Sender<RuntimeMessage>,
    metrics: Arc<Mutex<HashMap<usize, LinkMetrics>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl BondingRuntime {
    pub fn new() -> Self {
        let (tx, rx) = bounded(1000);
        let metrics = Arc::new(Mutex::new(HashMap::new()));
        let metrics_clone = metrics.clone();

        let handle = thread::spawn(move || runtime_worker(rx, metrics_clone));

        Self {
            sender: tx,
            metrics,
            handle: Some(handle),
        }
    }

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

    pub fn apply_config(&self, config: BondingConfig) -> anyhow::Result<()> {
        self.sender
            .send(RuntimeMessage::ApplyConfig(config))
            .map_err(|e| anyhow::anyhow!("Failed to send config: {}", e))
    }

    pub fn add_link(&self, link: LinkConfig) -> anyhow::Result<()> {
        self.sender
            .send(RuntimeMessage::AddLink(link))
            .map_err(|e| anyhow::anyhow!("Failed to add link: {}", e))
    }

    pub fn remove_link(&self, id: usize) -> anyhow::Result<()> {
        self.sender
            .send(RuntimeMessage::RemoveLink(id))
            .map_err(|e| anyhow::anyhow!("Failed to remove link: {}", e))
    }

    pub fn get_metrics(&self) -> HashMap<usize, LinkMetrics> {
        self.metrics.lock().unwrap().clone()
    }

    pub fn metrics_handle(&self) -> Arc<Mutex<HashMap<usize, LinkMetrics>>> {
        self.metrics.clone()
    }

    pub fn shutdown(&mut self) {
        let _ = self.sender.send(RuntimeMessage::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for BondingRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn runtime_worker(rx: Receiver<RuntimeMessage>, metrics: Arc<Mutex<HashMap<usize, LinkMetrics>>>) {
    let mut scheduler = BondingScheduler::new();
    let mut current_links: HashMap<usize, LinkConfig> = HashMap::new();
    let mut lifecycle_config = LinkLifecycleConfig::default();

    let mut last_fast_stats = Instant::now();
    let fast_stats_interval = Duration::from_millis(100);

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => match msg {
                RuntimeMessage::Packet(data, profile) => {
                    let _ = scheduler.send(data, profile);
                }
                RuntimeMessage::AddLink(link) => {
                    apply_link(&mut scheduler, &mut current_links, &lifecycle_config, link);
                }
                RuntimeMessage::RemoveLink(id) => {
                    scheduler.remove_link(id);
                    current_links.remove(&id);
                }
                RuntimeMessage::ApplyConfig(config) => {
                    lifecycle_config = config.lifecycle.clone();
                    apply_config(
                        &mut scheduler,
                        &mut current_links,
                        &lifecycle_config,
                        config,
                    );
                }
                RuntimeMessage::Shutdown => break,
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if last_fast_stats.elapsed() >= fast_stats_interval {
            scheduler.refresh_metrics();
            if let Ok(mut m) = metrics.lock() {
                *m = scheduler.get_all_metrics();
            }
            last_fast_stats = Instant::now();
        }
    }
}

fn apply_config(
    scheduler: &mut BondingScheduler<Link>,
    current_links: &mut HashMap<usize, LinkConfig>,
    lifecycle_config: &LinkLifecycleConfig,
    config: BondingConfig,
) {
    let desired_ids: std::collections::HashSet<usize> = config.links.iter().map(|l| l.id).collect();

    let existing_ids: Vec<usize> = current_links.keys().copied().collect();
    for id in existing_ids {
        if !desired_ids.contains(&id) {
            scheduler.remove_link(id);
            current_links.remove(&id);
        }
    }

    for link in config.links {
        let needs_update = match current_links.get(&link.id) {
            Some(existing) => existing != &link,
            None => true,
        };

        if needs_update {
            scheduler.remove_link(link.id);
            match Link::new_with_iface(
                link.id,
                &link.uri,
                link.interface.clone(),
                lifecycle_config.clone(),
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
    }
}

fn apply_link(
    scheduler: &mut BondingScheduler<Link>,
    current_links: &mut HashMap<usize, LinkConfig>,
    lifecycle_config: &LinkLifecycleConfig,
    link: LinkConfig,
) {
    scheduler.remove_link(link.id);
    match Link::new_with_iface(
        link.id,
        &link.uri,
        link.interface.clone(),
        lifecycle_config.clone(),
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
