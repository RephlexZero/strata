use crate::pad::RsRistBondSinkPad;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use rist_bonding_core::net::link::Link;
use rist_bonding_core::scheduler::bonding::BondingScheduler;
use rist_bonding_core::scheduler::PacketProfile;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod imp {
    use super::*;

    pub(crate) enum SinkMessage {
        Packet(bytes::Bytes, PacketProfile),
        AddLink { id: usize, uri: String },
        RemoveLink { id: usize },
    }

    pub struct RsRistBondSink {
        // We only hold the Sender. The Scheduler lives in the worker thread.
        pub(crate) sender: Mutex<Option<flume::Sender<SinkMessage>>>,
        pub(crate) worker_thread: Mutex<Option<std::thread::JoinHandle<()>>>,

        // Configuration state management
        pub(crate) links_config: Mutex<String>,
        pub(crate) pad_map: Mutex<HashMap<String, usize>>,
    }

    impl Default for RsRistBondSink {
        fn default() -> Self {
            Self {
                sender: Mutex::new(None),
                worker_thread: Mutex::new(None),
                links_config: Mutex::new(String::new()),
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
            self.send_msg(SinkMessage::AddLink { id, uri });
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
                vec![glib::ParamSpecString::builder("links")
                    .nick("Links (Deprecated)")
                    .blurb("Comma-separated list of links configuration")
                    .mutable_ready()
                    .build()]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "links" => {
                    *self.links_config.lock().unwrap() =
                        value.get().expect("type checked upstream");
                    self.reconfigure_legacy();
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "links" => self.links_config.lock().unwrap().to_value(),
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

            let element_weak = self.obj().downgrade();

            // Spawn Worker
            let handle = std::thread::spawn(move || {
                let mut scheduler = BondingScheduler::new();
                let mut last_stats = Instant::now();
                let stats_interval = Duration::from_secs(1);

                loop {
                    // Timeout allows us to run stats periodically even if no data
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(msg) => match msg {
                            SinkMessage::Packet(data, profile) => {
                                let _ = scheduler.send(data, profile);
                            }
                            SinkMessage::AddLink { id, uri } => match Link::new(id, &uri) {
                                Ok(l) => {
                                    scheduler.add_link(Arc::new(l));
                                    gst::info!(gst::CAT_DEFAULT, "Added link {} -> {}", id, uri);
                                }
                                Err(e) => {
                                    gst::error!(
                                        gst::CAT_DEFAULT,
                                        "Failed to create link {}: {}",
                                        uri,
                                        e
                                    );
                                }
                            },
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

                    // Stats Tick
                    if last_stats.elapsed() >= stats_interval {
                        if let Some(element) = element_weak.upgrade() {
                            let metrics = scheduler.get_all_metrics();
                            let mut msg_struct = gst::Structure::builder("rist-bonding-stats");
                            for (id, m) in metrics {
                                msg_struct = msg_struct
                                    .field(&format!("link_{}_rtt", id), m.rtt_ms)
                                    .field(&format!("link_{}_capacity", id), m.capacity_bps)
                                    .field(&format!("link_{}_loss", id), m.loss_rate)
                                    .field(&format!("link_{}_alive", id), m.alive);
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
                if can_drop {
                    // Trick: If the buffer is full (congestion), drop B-frames immediately
                    // instead of blocking. This preserves low-latency for P/I frames.
                    match tx.try_send(SinkMessage::Packet(data, profile)) {
                        Ok(_) => (),
                        Err(flume::TrySendError::Full(_)) => {
                            gst::warning!(gst::CAT_DEFAULT, "Congestion dropping expendable frame");
                            return Ok(gst::FlowSuccess::Ok);
                        }
                        Err(flume::TrySendError::Disconnected(_)) => {
                            return Err(gst::FlowError::Error);
                        }
                    }
                } else {
                    // For important frames, blocking backpressure is better than dropping
                    if let Err(_) = tx.send(SinkMessage::Packet(data, profile)) {
                        // Worker died?
                        return Err(gst::FlowError::Error);
                    }
                }
            }

            Ok(gst::FlowSuccess::Ok)
        }
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
