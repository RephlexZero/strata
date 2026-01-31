use crate::pad::RsRistBondSinkPad;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use rist_bonding_core::config::{BondingConfig, LinkConfig};
use rist_bonding_core::runtime::{BondingRuntime, PacketSendError};
use rist_bonding_core::scheduler::PacketProfile;
use std::collections::HashMap;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
fn parse_config(config: &str) -> Result<BondingConfig, String> {
    BondingConfig::from_toml_str(config)
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

    pub struct RsRistBondSink {
        pub(crate) runtime: Mutex<Option<BondingRuntime>>,
        pub(crate) stats_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
        pub(crate) stats_running: Arc<AtomicBool>,

        // Configuration state management
        pub(crate) links_config: Mutex<String>,
        pub(crate) config_toml: Mutex<String>,
        pub(crate) pad_map: Mutex<HashMap<String, usize>>,
        pub(crate) pending_links: Mutex<HashMap<usize, LinkConfig>>,
    }

    impl Default for RsRistBondSink {
        fn default() -> Self {
            Self {
                runtime: Mutex::new(None),
                stats_thread: Mutex::new(None),
                stats_running: Arc::new(AtomicBool::new(false)),
                links_config: Mutex::new(String::new()),
                config_toml: Mutex::new(String::new()),
                pad_map: Mutex::new(HashMap::new()),
                pending_links: Mutex::new(HashMap::new()),
            }
        }
    }

    impl RsRistBondSink {
        pub(crate) fn send_msg(&self, msg: SinkMessage) {
            let runtime = self.runtime.lock().unwrap();
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
                        self.pending_links.lock().unwrap().insert(
                            id,
                            LinkConfig {
                                id,
                                uri,
                                interface: iface,
                            },
                        );
                    }
                    SinkMessage::RemoveLink { id } => {
                        self.pending_links.lock().unwrap().remove(&id);
                    }
                }
            }
        }

        pub(crate) fn add_link_from_pad(&self, pad: &RsRistBondSinkPad) {
            let pad_name = pad.name().to_string();
            let uri = pad.get_uri();

            if uri.is_empty() {
                return;
            }

            let id = self.get_id_for_pad(&pad_name);
            self.send_msg(SinkMessage::AddLink {
                id,
                uri,
                iface: None,
            });
        }

        pub(crate) fn remove_link_by_pad_name(&self, pad_name: &str) {
            if let Some(id) = self.pad_map.lock().unwrap().remove(pad_name) {
                self.send_msg(SinkMessage::RemoveLink { id });
            }
        }

        fn get_id_for_pad(&self, pad_name: &str) -> usize {
            // Find existing or create ID
            let mut map = self.pad_map.lock().unwrap();
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
                    if let Some(rt) = self.runtime.lock().unwrap().as_ref() {
                        let _ = rt.apply_config(parsed);
                    }
                }
                Err(err) => {
                    gst::warning!(gst::CAT_DEFAULT, "{}", err);
                }
            }
        }

        fn reconfigure_legacy(&self) {
            let config = self.links_config.lock().unwrap().clone();
            if config.is_empty() {
                return;
            }
            for (idx, url) in config.split(',').enumerate() {
                let url = url.trim();
                if !url.is_empty() {
                    self.send_msg(SinkMessage::AddLink {
                        id: idx,
                        uri: url.to_string(),
                        iface: None,
                    });
                }
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RsRistBondSink {
        const NAME: &'static str = "RsRistBondSink";
        type Type = super::RsRistBondSink;
        type ParentType = gst_base::BaseSink;
    }

    impl ObjectImpl for RsRistBondSink {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: std::sync::OnceLock<Vec<glib::ParamSpec>> =
                std::sync::OnceLock::new();
            PROPERTIES.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("links")
                        .nick("Links (Deprecated)")
                        .blurb("Comma-separated list of links configuration")
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecString::builder("config")
                        .nick("Config (TOML)")
                        .blurb("TOML config with versioned schema")
                        .mutable_ready()
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "links" => {
                    *self.links_config.lock().unwrap() =
                        value.get().expect("type checked upstream");
                    self.reconfigure_legacy();
                }
                "config" => {
                    let cfg: String = value.get().expect("type checked upstream");
                    *self.config_toml.lock().unwrap() = cfg.clone();
                    self.apply_config(&cfg);
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "links" => self.links_config.lock().unwrap().to_value(),
                "config" => self.config_toml.lock().unwrap().to_value(),
                _ => unimplemented!(),
            }
        }
    }

    impl GstObjectImpl for RsRistBondSink {}

    impl ElementImpl for RsRistBondSink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static ELEMENT_METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
                std::sync::OnceLock::new();
            ELEMENT_METADATA.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "RIST Bonding Sink",
                    "Sink/Network",
                    "Sends packets via bonded RIST links",
                    "Author <author@example.com>",
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
                        &gst::Caps::new_empty_simple("meta/x-rist-config"),
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
                        if !self.pad_map.lock().unwrap().contains_key(&candidate) {
                            break candidate;
                        }
                        i += 1;
                    }
                };

                let pad = glib::Object::builder::<RsRistBondSinkPad>()
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
            if let Some(bond_pad) = pad.downcast_ref::<RsRistBondSinkPad>() {
                self.remove_link_by_pad_name(&bond_pad.name());
            }
            self.obj().remove_pad(pad).unwrap();
        }
    }

    impl BaseSinkImpl for RsRistBondSink {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let runtime = BondingRuntime::new();
            let metrics_handle = runtime.metrics_handle();
            *self.runtime.lock().unwrap() = Some(runtime);

            for pad in self.obj().pads() {
                if let Some(bond_pad) = pad.downcast_ref::<RsRistBondSinkPad>() {
                    self.add_link_from_pad(bond_pad);
                }
            }

            if let Some(rt) = self.runtime.lock().unwrap().as_ref() {
                let pending: Vec<LinkConfig> =
                    self.pending_links.lock().unwrap().drain().map(|(_, v)| v).collect();
                for link in pending {
                    let _ = rt.add_link(link);
                }
            }

            self.reconfigure_legacy();
            self.apply_config(&self.config_toml.lock().unwrap());

            let element_weak = self.obj().downgrade();
            let running = self.stats_running.clone();
            running.store(true, Ordering::Relaxed);

            let handle = std::thread::spawn(move || {
                let stats_interval = Duration::from_secs(1);
                let mut last_stats = Instant::now();
                let start = Instant::now();
                let mut stats_seq: u64 = 0;

                while running.load(Ordering::Relaxed) {
                    if last_stats.elapsed() >= stats_interval {
                        if let Some(element) = element_weak.upgrade() {
                            let metrics = metrics_handle.lock().unwrap().clone();
                            let mono_time_ns = start.elapsed().as_nanos() as u64;
                            let wall_time_ms = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);

                            let mut total_capacity = 0.0;
                            let mut alive_links = 0u64;
                            for m in metrics.values() {
                                if m.alive {
                                    total_capacity += m.capacity_bps;
                                    alive_links += 1;
                                }
                            }

                            let mut msg_struct = gst::Structure::builder("rist-bonding-stats")
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
                                    .field(&format!("link_{}_rtt", id), m.rtt_ms)
                                    .field(&format!("link_{}_capacity", id), m.capacity_bps)
                                    .field(&format!("link_{}_loss", id), m.loss_rate)
                                    .field(&format!("link_{}_observed_bps", id), m.observed_bps)
                                    .field(&format!("link_{}_observed_bytes", id), m.observed_bytes)
                                    .field(&format!("link_{}_alive", id), m.alive)
                                    .field(&format!("link_{}_phase", id), m.phase.as_str())
                                    .field(&format!("link_{}_os_up", id), os_up)
                                    .field(&format!("link_{}_mtu", id), mtu)
                                    .field(&format!("link_{}_iface", id), iface)
                                    .field(&format!("link_{}_kind", id), link_kind);
                            }
                            let _ = element
                                .post_message(gst::message::Element::new(msg_struct.build()));
                        }
                        stats_seq = stats_seq.wrapping_add(1);
                        last_stats = Instant::now();
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });

            *self.stats_thread.lock().unwrap() = Some(handle);
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            self.stats_running.store(false, Ordering::Relaxed);
            if let Some(handle) = self.stats_thread.lock().unwrap().take() {
                let _ = handle.join();
            }

            if let Some(mut runtime) = self.runtime.lock().unwrap().take() {
                runtime.shutdown();
            }
            Ok(())
        }

        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let data = bytes::Bytes::copy_from_slice(&map);

            let flags = buffer.flags();
            // Content-Aware Bonding:
            // 1. Critical: Broadcast Keyframes (IDR), Headers, and non-Delta (Audio) for reliability.
            // 2. Droppable: Allow dropping non-ref B-frames during congestion to preserve latency.
            let is_critical = !flags.contains(gst::BufferFlags::DELTA_UNIT)
                || flags.contains(gst::BufferFlags::HEADER);
            let can_drop = flags.contains(gst::BufferFlags::DROPPABLE);

            let profile = PacketProfile {
                is_critical,
                can_drop,
            };

            if let Some(rt) = self.runtime.lock().unwrap().as_ref() {
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
    use super::parse_config;

    #[test]
    fn parse_config_links_basic() {
        let toml = r#"
            version = 1

            [[links]]
            id = 10
            uri = "rist://1.2.3.4:5000"

            [[links]]
            uri = "rist://5.6.7.8:5000"
        "#;

        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.links.len(), 2);
        assert_eq!(cfg.links[0].id, 10);
        assert_eq!(cfg.links[0].uri, "rist://1.2.3.4:5000");
        assert!(cfg.links[0].interface.is_none());
        assert_eq!(cfg.links[1].id, 1); // idx fallback
        assert_eq!(cfg.links[1].uri, "rist://5.6.7.8:5000");
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
            uri = "rist://9.9.9.9:5000"
            interface = "eth0"

            [[links]]
            id = 3
            uri = "rist://9.9.9.10:5000"
            interface = ""
        "#;

        let cfg = parse_config(toml).unwrap();
        assert_eq!(cfg.links.len(), 2);
        assert_eq!(cfg.links[0].id, 2);
        assert_eq!(cfg.links[0].uri, "rist://9.9.9.9:5000");
        assert_eq!(cfg.links[0].interface.as_deref(), Some("eth0"));
        assert_eq!(cfg.links[1].id, 3);
        assert_eq!(cfg.links[1].uri, "rist://9.9.9.10:5000");
        assert!(cfg.links[1].interface.is_none());
    }
}

glib::wrapper! {
    pub struct RsRistBondSink(ObjectSubclass<imp::RsRistBondSink>)
        @extends gst_base::BaseSink, gst::Element, gst::Object;
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    gst::Element::register(
        plugin,
        "rsristbondsink",
        gst::Rank::NONE,
        RsRistBondSink::static_type(),
    )
}
