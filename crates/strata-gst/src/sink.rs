use crate::pad::StrataSinkPad;
use crate::util::lock_or_recover;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use std::collections::HashMap;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use strata_bonding::config::{BondingConfig, LinkConfig, SchedulerConfig};
use strata_bonding::runtime::{BondingRuntime, PacketSendError};
use strata_bonding::scheduler::PacketProfile;

fn parse_config(config: &str) -> Result<BondingConfig, String> {
    BondingConfig::from_toml_str(config)
}

fn compute_congestion_recommendation(
    total_capacity_bps: f64,
    observed_bps: f64,
    headroom_ratio: f64,
    trigger_ratio: f64,
) -> Option<u64> {
    if total_capacity_bps <= 0.0 {
        return None;
    }
    let recommended = (total_capacity_bps * headroom_ratio).round() as u64;
    if observed_bps > total_capacity_bps * trigger_ratio {
        Some(recommended)
    } else {
        None
    }
}

mod imp {
    use super::*;

    pub(crate) enum SinkMessage {
        AddLink {
            id: usize,
            uri: String,
            iface: Option<String>,
        },
        RemoveLink {
            id: usize,
        },
    }

    pub struct StrataSink {
        pub(crate) runtime: Mutex<Option<BondingRuntime>>,
        pub(crate) stats_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
        pub(crate) stats_running: Arc<AtomicBool>,

        pub(crate) destinations_config: Mutex<String>,
        pub(crate) config_toml: Mutex<String>,
        pub(crate) metrics_addr: Mutex<String>,
        pub(crate) pad_map: Mutex<HashMap<String, usize>>,
        pub(crate) pending_links: Mutex<HashMap<usize, LinkConfig>>,
        pub(crate) scheduler_config: Mutex<SchedulerConfig>,
    }

    impl Default for StrataSink {
        fn default() -> Self {
            Self {
                runtime: Mutex::new(None),
                stats_thread: Mutex::new(None),
                stats_running: Arc::new(AtomicBool::new(false)),
                destinations_config: Mutex::new(String::new()),
                config_toml: Mutex::new(String::new()),
                metrics_addr: Mutex::new(String::new()),
                pad_map: Mutex::new(HashMap::new()),
                pending_links: Mutex::new(HashMap::new()),
                scheduler_config: Mutex::new(SchedulerConfig::default()),
            }
        }
    }

    impl StrataSink {
        pub(crate) fn send_msg(&self, msg: SinkMessage) {
            let runtime = lock_or_recover(&self.runtime);
            if let Some(rt) = &*runtime {
                match msg {
                    SinkMessage::AddLink { id, uri, iface } => {
                        let _ = rt.add_link(LinkConfig {
                            id,
                            uri,
                            interface: iface,
                        });
                    }
                    SinkMessage::RemoveLink { id } => {
                        let _ = rt.remove_link(id);
                    }
                }
            } else {
                match msg {
                    SinkMessage::AddLink { id, uri, iface } => {
                        lock_or_recover(&self.pending_links).insert(
                            id,
                            LinkConfig {
                                id,
                                uri,
                                interface: iface,
                            },
                        );
                    }
                    SinkMessage::RemoveLink { id } => {
                        lock_or_recover(&self.pending_links).remove(&id);
                    }
                }
            }
        }

        pub(crate) fn add_link_from_pad(&self, pad: &StrataSinkPad) {
            let pad_name = pad.name().to_string();
            let uri = pad.get_uri();

            if uri.is_empty() {
                return;
            }

            let id = self.get_id_for_pad(&pad_name);
            self.send_msg(SinkMessage::AddLink {
                id,
                uri,
                iface: pad.get_interface(),
            });
        }

        pub(crate) fn remove_link_by_pad_name(&self, pad_name: &str) {
            if let Some(id) = lock_or_recover(&self.pad_map).remove(pad_name) {
                self.send_msg(SinkMessage::RemoveLink { id });
            }
        }

        fn get_id_for_pad(&self, pad_name: &str) -> usize {
            let mut map = lock_or_recover(&self.pad_map);
            if let Some(&id) = map.get(pad_name) {
                return id;
            }

            let id_str = pad_name.trim_start_matches("link_");
            let id = if let Ok(n) = id_str.parse::<usize>() {
                n
            } else {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                pad_name.hash(&mut hasher);
                hasher.finish() as usize
            };
            map.insert(pad_name.to_string(), id);
            id
        }

        fn apply_config(&self, config: &str) {
            if config.trim().is_empty() {
                return;
            }
            match parse_config(config) {
                Ok(parsed) => {
                    *lock_or_recover(&self.scheduler_config) = parsed.scheduler.clone();
                    if let Some(rt) = lock_or_recover(&self.runtime).as_ref() {
                        let _ = rt.apply_config(parsed);
                    }
                }
                Err(err) => {
                    gst::warning!(gst::CAT_DEFAULT, "{}", err);
                }
            }
        }

        fn reconfigure_destinations(&self) {
            let config = lock_or_recover(&self.destinations_config).clone();
            if config.is_empty() {
                return;
            }
            for (idx, addr) in config.split(',').enumerate() {
                let addr = addr.trim();
                if !addr.is_empty() {
                    self.send_msg(SinkMessage::AddLink {
                        id: idx,
                        uri: addr.to_string(),
                        iface: None,
                    });
                }
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StrataSink {
        const NAME: &'static str = "StrataSink";
        type Type = super::StrataSink;
        type ParentType = gst_base::BaseSink;
    }

    impl ObjectImpl for StrataSink {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: std::sync::OnceLock<Vec<glib::ParamSpec>> =
                std::sync::OnceLock::new();
            PROPERTIES.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("destinations")
                        .nick("Destinations")
                        .blurb("Comma-separated list of destination addresses (host:port)")
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecString::builder("config")
                        .nick("Config (TOML)")
                        .blurb("TOML config with versioned schema")
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecString::builder("config-file")
                        .nick("Config File")
                        .blurb("Path to TOML config file (alternative to inline config property)")
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecString::builder("metrics-addr")
                        .nick("Metrics Address")
                        .blurb("Prometheus metrics server address (e.g. 0.0.0.0:9090). Empty to disable.")
                        .mutable_ready()
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "destinations" => {
                    *lock_or_recover(&self.destinations_config) =
                        value.get().expect("type checked upstream");
                    self.reconfigure_destinations();
                }
                "config" => {
                    let cfg: String = value.get().expect("type checked upstream");
                    *lock_or_recover(&self.config_toml) = cfg.clone();
                    self.apply_config(&cfg);
                }
                "metrics-addr" => {
                    *lock_or_recover(&self.metrics_addr) =
                        value.get().expect("type checked upstream");
                }
                "config-file" => {
                    let path: String = value.get().expect("type checked upstream");
                    if path.is_empty() {
                        return;
                    }
                    if path.contains("..") {
                        gst::warning!(
                            gst::CAT_DEFAULT,
                            "Rejected config-file path with '..': {}",
                            path
                        );
                        return;
                    }
                    match std::fs::read_to_string(&path) {
                        Ok(cfg) => {
                            *lock_or_recover(&self.config_toml) = cfg.clone();
                            self.apply_config(&cfg);
                        }
                        Err(e) => {
                            gst::warning!(
                                gst::CAT_DEFAULT,
                                "Failed to read config file '{}': {}",
                                path,
                                e
                            );
                        }
                    }
                }
                _ => {
                    gst::warning!(gst::CAT_DEFAULT, "Unknown property: {}", pspec.name());
                }
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "destinations" => lock_or_recover(&self.destinations_config).to_value(),
                "config" | "config-file" => lock_or_recover(&self.config_toml).to_value(),
                "metrics-addr" => lock_or_recover(&self.metrics_addr).to_value(),
                _ => {
                    gst::warning!(gst::CAT_DEFAULT, "Unknown property: {}", pspec.name());
                    "".to_value()
                }
            }
        }
    }

    impl GstObjectImpl for StrataSink {}

    impl ElementImpl for StrataSink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static ELEMENT_METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
                std::sync::OnceLock::new();
            ELEMENT_METADATA.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "Strata Bonding Sink",
                    "Sink/Network",
                    "Sends packets via bonded Strata transport links",
                    "Strata Contributors <https://github.com/RephlexZero/strata>",
                )
            });
            Some(ELEMENT_METADATA.get().unwrap())
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> =
                std::sync::OnceLock::new();
            PAD_TEMPLATES.get_or_init(|| {
                let caps = gst::Caps::new_any();
                vec![
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap(),
                    gst::PadTemplate::new(
                        "link_%u",
                        gst::PadDirection::Src,
                        gst::PadPresence::Request,
                        &gst::Caps::new_empty_simple("meta/x-strata-link"),
                    )
                    .unwrap(),
                ]
            });
            PAD_TEMPLATES.get().unwrap()
        }

        fn request_new_pad(
            &self,
            templ: &gst::PadTemplate,
            name: Option<&str>,
            _caps: Option<&gst::Caps>,
        ) -> Option<gst::Pad> {
            if templ.name_template() == "link_%u" {
                let name = if let Some(n) = name {
                    n.to_string()
                } else {
                    let mut i = 0;
                    loop {
                        let candidate = format!("link_{}", i);
                        if !lock_or_recover(&self.pad_map).contains_key(&candidate) {
                            break candidate;
                        }
                        i += 1;
                    }
                };

                let pad = glib::Object::builder::<StrataSinkPad>()
                    .property("name", &name)
                    .property("direction", gst::PadDirection::Src)
                    .property("template", templ)
                    .build();

                self.obj().add_pad(&pad).unwrap();

                let weak_sink = self.obj().downgrade();
                pad.connect_notify(Some("uri"), move |pad, _pspec| {
                    if let Some(sink) = weak_sink.upgrade() {
                        sink.imp().add_link_from_pad(pad);
                    }
                });
                self.add_link_from_pad(&pad);
                Some(pad.upcast())
            } else {
                None
            }
        }

        fn release_pad(&self, pad: &gst::Pad) {
            if let Some(bond_pad) = pad.downcast_ref::<StrataSinkPad>() {
                self.remove_link_by_pad_name(&bond_pad.name());
            }
            self.obj().remove_pad(pad).unwrap();
        }
    }

    impl BaseSinkImpl for StrataSink {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let sched_cfg = lock_or_recover(&self.scheduler_config).clone();
            let mut runtime = BondingRuntime::with_config(sched_cfg.clone());

            // Start Prometheus metrics server if configured
            let metrics_addr_str = lock_or_recover(&self.metrics_addr).clone();
            if !metrics_addr_str.is_empty() {
                match metrics_addr_str.parse::<std::net::SocketAddr>() {
                    Ok(addr) => match runtime.start_metrics_server(addr) {
                        Ok(bound) => {
                            gst::info!(
                                gst::CAT_DEFAULT,
                                "Prometheus metrics server listening on {}",
                                bound
                            );
                        }
                        Err(e) => {
                            gst::warning!(
                                gst::CAT_DEFAULT,
                                "Failed to start metrics server on {}: {}",
                                metrics_addr_str,
                                e
                            );
                        }
                    },
                    Err(e) => {
                        gst::warning!(
                            gst::CAT_DEFAULT,
                            "Invalid metrics-addr '{}': {}",
                            metrics_addr_str,
                            e
                        );
                    }
                }
            }

            let metrics_handle = runtime.metrics_handle();
            *lock_or_recover(&self.runtime) = Some(runtime);

            for pad in self.obj().pads() {
                if let Some(bond_pad) = pad.downcast_ref::<StrataSinkPad>() {
                    self.add_link_from_pad(bond_pad);
                }
            }

            if let Some(rt) = lock_or_recover(&self.runtime).as_ref() {
                let pending: Vec<LinkConfig> = lock_or_recover(&self.pending_links)
                    .drain()
                    .map(|(_, v)| v)
                    .collect();
                for link in pending {
                    let _ = rt.add_link(link);
                }
            }

            self.reconfigure_destinations();
            self.apply_config(&lock_or_recover(&self.config_toml));

            let element_weak = self.obj().downgrade();
            let running = self.stats_running.clone();
            running.store(true, Ordering::Relaxed);
            let congestion_headroom = sched_cfg.congestion_headroom_ratio;
            let congestion_trigger = sched_cfg.congestion_trigger_ratio;

            let handle = std::thread::Builder::new()
                .name("strata-stats".into())
                .spawn(move || {
                    let stats_interval = Duration::from_millis(sched_cfg.stats_interval_ms);
                    let mut last_stats = Instant::now();
                    let start = Instant::now();
                    let mut stats_seq: u64 = 0;

                    while running.load(Ordering::Relaxed) {
                        if last_stats.elapsed() >= stats_interval {
                            if let Some(element) = element_weak.upgrade() {
                                let metrics = lock_or_recover(&metrics_handle).clone();
                                let mono_time_ns = start.elapsed().as_nanos() as u64;
                                let wall_time_ms = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);

                                let mut total_capacity = 0.0;
                                let mut total_observed_bps = 0.0;
                                let mut alive_links = 0u64;
                                for m in metrics.values() {
                                    if m.alive {
                                        total_capacity += m.capacity_bps;
                                        total_observed_bps += m.observed_bps;
                                        alive_links += 1;
                                    }
                                }

                                let mut msg_struct = gst::Structure::builder("strata-stats")
                                    .field("schema_version", 1i32)
                                    .field("stats_seq", stats_seq)
                                    .field("heartbeat", true)
                                    .field("mono_time_ns", mono_time_ns)
                                    .field("wall_time_ms", wall_time_ms)
                                    .field("total_capacity", total_capacity)
                                    .field("alive_links", alive_links);
                                for (id, m) in metrics {
                                    let os_up =
                                        m.os_up.map(|v| if v { 1i32 } else { 0i32 }).unwrap_or(-1);
                                    let mtu = m.mtu.map(|v| v as i32).unwrap_or(-1);
                                    let iface = m.iface.as_deref().unwrap_or("");
                                    let link_kind = m.link_kind.as_deref().unwrap_or("");
                                    msg_struct = msg_struct
                                        .field(format!("link_{}_rtt", id), m.rtt_ms)
                                        .field(format!("link_{}_capacity", id), m.capacity_bps)
                                        .field(format!("link_{}_loss", id), m.loss_rate)
                                        .field(format!("link_{}_observed_bps", id), m.observed_bps)
                                        .field(
                                            format!("link_{}_observed_bytes", id),
                                            m.observed_bytes,
                                        )
                                        .field(format!("link_{}_alive", id), m.alive)
                                        .field(format!("link_{}_phase", id), m.phase.as_str())
                                        .field(format!("link_{}_os_up", id), os_up)
                                        .field(format!("link_{}_mtu", id), mtu)
                                        .field(format!("link_{}_iface", id), iface)
                                        .field(format!("link_{}_kind", id), link_kind);
                                }
                                let _ = element
                                    .post_message(gst::message::Element::new(msg_struct.build()));

                                if let Some(recommended) = compute_congestion_recommendation(
                                    total_capacity,
                                    total_observed_bps,
                                    congestion_headroom,
                                    congestion_trigger,
                                ) {
                                    let msg = gst::Structure::builder("congestion-control")
                                        .field("recommended-bitrate", recommended)
                                        .field("total_capacity", total_capacity)
                                        .field("observed_bps", total_observed_bps)
                                        .build();
                                    let _ = element.post_message(gst::message::Element::new(msg));
                                }
                            }
                            stats_seq = stats_seq.wrapping_add(1);
                            last_stats = Instant::now();
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                })
                .expect("failed to spawn stats thread");

            *lock_or_recover(&self.stats_thread) = Some(handle);
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            self.stats_running.store(false, Ordering::Relaxed);
            if let Some(handle) = lock_or_recover(&self.stats_thread).take() {
                let _ = handle.join();
            }

            if let Some(mut runtime) = lock_or_recover(&self.runtime).take() {
                runtime.shutdown();
            }
            Ok(())
        }

        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let data = bytes::Bytes::copy_from_slice(&map);

            let flags = buffer.flags();
            let is_critical = !flags.contains(gst::BufferFlags::DELTA_UNIT)
                || flags.contains(gst::BufferFlags::HEADER);
            let can_drop = flags.contains(gst::BufferFlags::DROPPABLE);

            let profile = PacketProfile {
                is_critical,
                can_drop,
                size_bytes: data.len(),
            };

            if let Some(rt) = lock_or_recover(&self.runtime).as_ref() {
                match rt.try_send_packet(data, profile) {
                    Ok(_) => (),
                    Err(PacketSendError::Full) => {
                        if can_drop {
                            gst::warning!(gst::CAT_DEFAULT, "Congestion dropping expendable frame");
                        } else {
                            gst::error!(gst::CAT_DEFAULT, "Congestion dropping critical frame");
                        }
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    Err(PacketSendError::Disconnected) => {
                        return Err(gst::FlowError::Error);
                    }
                }
            }

            Ok(gst::FlowSuccess::Ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_congestion_recommendation, parse_config};

    #[test]
    fn parse_config_links_basic() {
        let toml = r#"
            version = 1

            [[links]]
            id = 10
            uri = "192.168.1.100:5000"

            [[links]]
            uri = "10.0.0.1:5000"
        "#;

        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.links.len(), 2);
        assert_eq!(cfg.links[0].id, 10);
        assert_eq!(cfg.links[0].uri, "192.168.1.100:5000");
        assert!(cfg.links[0].interface.is_none());
        assert_eq!(cfg.links[1].id, 1);
        assert_eq!(cfg.links[1].uri, "10.0.0.1:5000");
        assert!(cfg.links[1].interface.is_none());
    }

    #[test]
    fn parse_config_links_dedup() {
        let toml = r#"
            version = 1
            [[links]]
            id = 1
            uri = "a"
            [[links]]
            id = 1
            uri = "b"
        "#;
        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.links.len(), 1);
        assert_eq!(cfg.links[0].uri, "a");
    }

    #[test]
    fn parse_config_links_with_interface() {
        let toml = r#"
            version = 1
            [[links]]
            id = 2
            uri = "10.0.0.1:5000"
            interface = "eth0"

            [[links]]
            id = 3
            uri = "10.0.0.2:5000"
            interface = ""
        "#;

        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.links.len(), 2);
        assert_eq!(cfg.links[0].id, 2);
        assert_eq!(cfg.links[0].uri, "10.0.0.1:5000");
        assert_eq!(cfg.links[0].interface.as_deref(), Some("eth0"));
        assert_eq!(cfg.links[1].id, 3);
        assert_eq!(cfg.links[1].uri, "10.0.0.2:5000");
        assert!(cfg.links[1].interface.is_none());
    }

    #[test]
    fn congestion_recommendation_respects_headroom() {
        let recommended = compute_congestion_recommendation(10_000_000.0, 9_200_000.0, 0.85, 0.90);
        assert_eq!(recommended, Some(8_500_000));
    }

    #[test]
    fn congestion_recommendation_skips_below_trigger() {
        let recommended = compute_congestion_recommendation(10_000_000.0, 8_000_000.0, 0.85, 0.90);
        assert!(recommended.is_none());
    }

    #[test]
    fn congestion_recommendation_skips_zero_capacity() {
        let recommended = compute_congestion_recommendation(0.0, 9_000_000.0, 0.85, 0.90);
        assert!(recommended.is_none());
    }
}

glib::wrapper! {
    pub struct StrataSink(ObjectSubclass<imp::StrataSink>)
        @extends gst_base::BaseSink, gst::Element, gst::Object;
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    gst::Element::register(
        plugin,
        "stratasink",
        gst::Rank::NONE,
        StrataSink::static_type(),
    )
}
