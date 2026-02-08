use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use rist_bonding_core::receiver::bonding::BondingReceiver;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod imp {
    use super::*;

    struct Settings {
        links: String,
        latency: u32,
        config_toml: String,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                links: String::new(),
                latency: 100, // Default 100ms
                config_toml: String::new(),
            }
        }
    }

    #[derive(Default)]
    pub struct RsRistBondSrc {
        settings: Mutex<Settings>,
        receiver: Mutex<Option<BondingReceiver>>,
        stats_running: Arc<AtomicBool>,
        stats_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl RsRistBondSrc {
        fn apply_config_toml(&self, toml_str: &str) {
            if toml_str.trim().is_empty() {
                return;
            }
            match rist_bonding_core::config::BondingConfig::from_toml_str(toml_str) {
                Ok(cfg) => {
                    let mut settings = self.settings.lock().unwrap();
                    settings.config_toml = toml_str.to_string();
                    // Apply receiver config: override latency and links if specified
                    settings.latency = cfg.receiver.start_latency.as_millis() as u32;
                    // If links are specified in config, override the links property
                    if !cfg.links.is_empty() {
                        settings.links = cfg
                            .links
                            .iter()
                            .map(|l| l.uri.as_str())
                            .collect::<Vec<_>>()
                            .join(",");
                    }
                }
                Err(e) => {
                    eprintln!("RsRistBondSrc: Invalid config TOML: {}", e);
                }
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RsRistBondSrc {
        const NAME: &'static str = "RsRistBondSrc";
        type Type = super::RsRistBondSrc;
        type ParentType = gst_base::PushSrc;
    }

    impl ObjectImpl for RsRistBondSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: std::sync::OnceLock<Vec<glib::ParamSpec>> =
                std::sync::OnceLock::new();

            PROPERTIES.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("links")
                        .nick("Links")
                        .blurb("Comma-separated list of RIST URLs to bind to (e.g. 'rist://@0.0.0.0:5000')")
                        .build(),
                    glib::ParamSpecUInt::builder("latency")
                        .nick("Latency")
                        .blurb("Reassembly buffer latency in milliseconds")
                        .default_value(100)
                        .build(),
                    glib::ParamSpecString::builder("config")
                        .nick("Config (TOML)")
                        .blurb("TOML config with versioned schema (receiver section)")
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecString::builder("config-file")
                        .nick("Config File")
                        .blurb("Path to TOML config file (alternative to inline config property)")
                        .mutable_ready()
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "links" => {
                    let mut settings = self.settings.lock().unwrap();
                    settings.links = value.get().expect("type checked upstream");
                }
                "latency" => {
                    let mut settings = self.settings.lock().unwrap();
                    settings.latency = value.get().expect("type checked upstream");
                }
                "config" => {
                    let cfg: String = value.get().expect("type checked upstream");
                    self.apply_config_toml(&cfg);
                }
                "config-file" => {
                    let path: String = value.get().expect("type checked upstream");
                    if path.is_empty() {
                        return;
                    }
                    match std::fs::read_to_string(&path) {
                        Ok(cfg) => {
                            self.apply_config_toml(&cfg);
                        }
                        Err(e) => {
                            eprintln!(
                                "RsRistBondSrc: Failed to read config file '{}': {}",
                                path, e
                            );
                        }
                    }
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "links" => {
                    let settings = self.settings.lock().unwrap();
                    settings.links.to_value()
                }
                "latency" => {
                    let settings = self.settings.lock().unwrap();
                    settings.latency.to_value()
                }
                "config" | "config-file" => {
                    let settings = self.settings.lock().unwrap();
                    settings.config_toml.to_value()
                }
                _ => unimplemented!(),
            }
        }
    }

    impl GstObjectImpl for RsRistBondSrc {}

    impl ElementImpl for RsRistBondSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static ELEMENT_METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
                std::sync::OnceLock::new();

            ELEMENT_METADATA.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "RIST Bonding Source",
                    "Source/Network",
                    "Receives packets via bonded RIST links",
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
                vec![gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap()]
            });
            PAD_TEMPLATES.get().unwrap()
        }
    }

    impl BaseSrcImpl for RsRistBondSrc {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let settings = self.settings.lock().unwrap();
            let mut receiver_guard = self.receiver.lock().unwrap();

            if receiver_guard.is_some() {
                return Ok(());
            }

            let latency_duration = Duration::from_millis(settings.latency as u64);
            let receiver = BondingReceiver::new(latency_duration);

            for link in settings.links.split(',') {
                let link = link.trim();
                if link.is_empty() {
                    continue;
                }

                receiver.add_link(link).map_err(|e| {
                    let err_msg = format!("Failed to bind link {}: {}", link, e);
                    eprintln!("RsRistBondSrc Error: {}", err_msg);
                    gst::error_msg!(gst::ResourceError::OpenRead, ["{}", err_msg])
                })?;
            }

            *receiver_guard = Some(receiver);

            // Setup stats thread
            self.stats_running.store(true, Ordering::Relaxed);
            let running = self.stats_running.clone();
            let element_weak = self.obj().downgrade();

            let handle = std::thread::spawn(move || {
                let start = Instant::now();
                let mut stats_seq: u64 = 0;
                while running.load(Ordering::Relaxed) {
                    if let Some(element) = element_weak.upgrade() {
                        let imp = element.imp();
                        if let Ok(receiver_guard) = imp.receiver.lock() {
                            if let Some(receiver) = &*receiver_guard {
                                let stats = receiver.get_stats();
                                let mono_time_ns = start.elapsed().as_nanos() as u64;
                                let wall_time_ms = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                                let msg = gst::Structure::builder("rist-bonding-stats")
                                    .field("schema_version", 1i32)
                                    .field("stats_seq", stats_seq)
                                    .field("heartbeat", true)
                                    .field("mono_time_ns", mono_time_ns)
                                    .field("wall_time_ms", wall_time_ms)
                                    .field("total_capacity", 0.0f64)
                                    .field("alive_links", 0u64)
                                    .field("queue_depth", stats.queue_depth as u64)
                                    .field("next_seq", stats.next_seq as u64)
                                    .field("lost_packets", stats.lost_packets as u64)
                                    .field("late_packets", stats.late_packets as u64)
                                    .field("current_latency_ms", stats.current_latency_ms) // Added
                                    .build();
                                let _ = element.post_message(gst::message::Element::new(msg));
                                stats_seq = stats_seq.wrapping_add(1);
                            }
                        }
                    } else {
                        break;
                    }

                    std::thread::sleep(Duration::from_secs(1));
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

            let mut receiver_guard = self.receiver.lock().unwrap();
            if let Some(mut receiver) = receiver_guard.take() {
                receiver.shutdown();
                // We leave flushing/closing to Drop?
            }
            Ok(())
        }

        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            let mut receiver_guard = self.receiver.lock().unwrap();
            if let Some(receiver) = &mut *receiver_guard {
                receiver.shutdown();
            }
            Ok(())
        }
    }

    impl PushSrcImpl for RsRistBondSrc {
        fn create(
            &self,
            _buf: Option<&mut gst::BufferRef>,
        ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
            let receiver_guard = self.receiver.lock().unwrap();

            let rx = if let Some(receiver) = &*receiver_guard {
                receiver.output_rx.clone()
            } else {
                return Err(gst::FlowError::Eos);
            };
            drop(receiver_guard);

            match rx.recv() {
                Ok(bytes) => {
                    let buffer = gst::Buffer::from_slice(bytes);
                    Ok(gst_base::subclass::base_src::CreateSuccess::NewBuffer(
                        buffer,
                    ))
                }
                Err(_) => Err(gst::FlowError::Eos),
            }
        }
    }
}

glib::wrapper! {
    pub struct RsRistBondSrc(ObjectSubclass<imp::RsRistBondSrc>)
        @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    gst::Element::register(
        plugin,
        "rsristbondsrc",
        gst::Rank::NONE,
        RsRistBondSrc::static_type(),
    )
}
