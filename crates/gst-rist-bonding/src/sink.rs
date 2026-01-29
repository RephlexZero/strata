use crate::pad::RsRistBondSinkPad;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use rist_bonding_core::net::link::Link;
use rist_bonding_core::scheduler::bonding::BondingScheduler;
use rist_bonding_core::scheduler::PacketProfile;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default, Deserialize)]
#[serde(default)]
struct SinkConfigV1 {
    version: u32,
    links: Vec<LinkConfig>,
}

#[derive(Deserialize)]
struct LinkConfig {
    id: Option<usize>,
    uri: String,
    interface: Option<String>,
}

fn parse_config_links(config: &str) -> Result<Vec<(usize, String, Option<String>)>, String> {
    let config = config.trim();
    if config.is_empty() {
        return Ok(Vec::new());
    }

    let parsed: SinkConfigV1 =
        serde_json::from_str(config).map_err(|e| format!("Invalid config JSON: {}", e))?;

    if parsed.version != 0 && parsed.version != 1 {
        return Err(format!("Unsupported config version {}", parsed.version));
    }

    let mut used = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (idx, link) in parsed.links.into_iter().enumerate() {
        let id = link.id.unwrap_or(idx);
        if !used.insert(id) {
            continue;
        }
        if link.uri.trim().is_empty() {
            continue;
        }
        let iface = link.interface.and_then(|iface| {
            if iface.trim().is_empty() {
                None
            } else {
                Some(iface)
            }
        });
        out.push((id, link.uri, iface));
    }

    Ok(out)
}

mod imp {
    use super::*;

    pub(crate) enum SinkMessage {
        Packet(bytes::Bytes, PacketProfile),
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
        // We only hold the Sender. The Scheduler lives in the worker thread.
        pub(crate) sender: Mutex<Option<flume::Sender<SinkMessage>>>,
        pub(crate) worker_thread: Mutex<Option<std::thread::JoinHandle<()>>>,

        // Configuration state management
        pub(crate) links_config: Mutex<String>,
        pub(crate) config_json: Mutex<String>,
        pub(crate) pad_map: Mutex<HashMap<String, usize>>,
    }

    impl Default for RsRistBondSink {
        fn default() -> Self {
            Self {
                sender: Mutex::new(None),
                worker_thread: Mutex::new(None),
                links_config: Mutex::new(String::new()),
                config_json: Mutex::new(String::new()),
                pad_map: Mutex::new(HashMap::new()),
            }
        }
    }

    impl RsRistBondSink {
        pub(crate) fn send_msg(&self, msg: SinkMessage) {
            let sender = self.sender.lock().unwrap();
            if let Some(tx) = &*sender {
                let _ = tx.send(msg);
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
            match parse_config_links(config) {
                Ok(links) => {
                    for (id, uri, iface) in links {
                        self.send_msg(SinkMessage::AddLink { id, uri, iface });
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
                        .nick("Config (JSON)")
                        .blurb("JSON config with versioned schema")
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
                    *self.config_json.lock().unwrap() = cfg.clone();
                    self.apply_config(&cfg);
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "links" => self.links_config.lock().unwrap().to_value(),
                "config" => self.config_json.lock().unwrap().to_value(),
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
            let (tx, rx) = flume::bounded(1000);
            *self.sender.lock().unwrap() = Some(tx);

            self.reconfigure_legacy();
            self.apply_config(&self.config_json.lock().unwrap());

            let element_weak = self.obj().downgrade();

            // Spawn Worker
            let handle = std::thread::spawn(move || {
                let mut scheduler = BondingScheduler::new();
                let mut last_stats = Instant::now();
                let stats_interval = Duration::from_secs(1);
                let mut last_fast_stats = Instant::now();
                let fast_stats_interval = Duration::from_millis(100);

                loop {
                    // Timeout allows us to run stats periodically even if no data
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(msg) => match msg {
                            SinkMessage::Packet(data, profile) => {
                                let _ = scheduler.send(data, profile);
                            }
                            SinkMessage::AddLink { id, uri, iface } => {
                                match Link::new_with_iface(id, &uri, iface) {
                                    Ok(l) => {
                                        scheduler.add_link(Arc::new(l));
                                        gst::info!(
                                            gst::CAT_DEFAULT,
                                            "Added link {} -> {}",
                                            id,
                                            uri
                                        );
                                    }
                                    Err(e) => {
                                        gst::error!(
                                            gst::CAT_DEFAULT,
                                            "Failed to create link {}: {}",
                                            uri,
                                            e
                                        );
                                    }
                                }
                            }
                            SinkMessage::RemoveLink { id } => {
                                scheduler.remove_link(id);
                                gst::info!(gst::CAT_DEFAULT, "Removed link {}", id);
                            }
                        },
                        Err(flume::RecvTimeoutError::Timeout) => {
                            // idle check
                        }
                        Err(flume::RecvTimeoutError::Disconnected) => break,
                    }

                    // Fast-path scheduler metrics refresh (internal only)
                    if last_fast_stats.elapsed() >= fast_stats_interval {
                        scheduler.refresh_metrics();
                        last_fast_stats = Instant::now();
                    }

                    // Stats Tick
                    if last_stats.elapsed() >= stats_interval {
                        if let Some(element) = element_weak.upgrade() {
                            let metrics = scheduler.get_all_metrics();
                            let mut msg_struct = gst::Structure::builder("rist-bonding-stats");
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
                        last_stats = Instant::now();
                    }
                }
            });

            *self.worker_thread.lock().unwrap() = Some(handle);
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            // Drop sender to signal worker to stop
            *self.sender.lock().unwrap() = None;

            if let Some(handle) = self.worker_thread.lock().unwrap().take() {
                let _ = handle.join();
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

            let sender_guard = self.sender.lock().unwrap();
            if let Some(tx) = &*sender_guard {
                // Never block render thread. Try-send everything; drop on congestion.
                match tx.try_send(SinkMessage::Packet(data, profile)) {
                    Ok(_) => (),
                    Err(flume::TrySendError::Full(_)) => {
                        if can_drop {
                            gst::warning!(gst::CAT_DEFAULT, "Congestion dropping expendable frame");
                        } else {
                            gst::error!(gst::CAT_DEFAULT, "Congestion dropping critical frame");
                        }
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    Err(flume::TrySendError::Disconnected(_)) => {
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
    use super::parse_config_links;

    #[test]
    fn parse_config_links_basic() {
        let json = r#"{
            "version": 1,
            "links": [
                {"id": 10, "uri": "rist://1.2.3.4:5000"},
                {"uri": "rist://5.6.7.8:5000"}
            ]
        }"#;

        let links = parse_config_links(json).unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].0, 10);
        assert_eq!(links[0].1, "rist://1.2.3.4:5000");
        assert!(links[0].2.is_none());
        assert_eq!(links[1].0, 1); // idx fallback
        assert_eq!(links[1].1, "rist://5.6.7.8:5000");
        assert!(links[1].2.is_none());
    }

    #[test]
    fn parse_config_links_dedup() {
        let json = r#"{"version": 1, "links": [
            {"id": 1, "uri": "a"},
            {"id": 1, "uri": "b"}
        ]}"#;
        let links = parse_config_links(json).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "a");
    }

    #[test]
    fn parse_config_links_with_interface() {
        let json = r#"{"version": 1, "links": [
            {"id": 2, "uri": "rist://9.9.9.9:5000", "interface": "eth0"},
            {"id": 3, "uri": "rist://9.9.9.10:5000", "interface": ""}
        ]}"#;

        let links = parse_config_links(json).unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].0, 2);
        assert_eq!(links[0].1, "rist://9.9.9.9:5000");
        assert_eq!(links[0].2.as_deref(), Some("eth0"));
        assert_eq!(links[1].0, 3);
        assert_eq!(links[1].1, "rist://9.9.9.10:5000");
        assert!(links[1].2.is_none());
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
