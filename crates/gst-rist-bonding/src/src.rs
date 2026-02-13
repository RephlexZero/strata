use crate::util::lock_or_recover;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use rist_bonding_core::receiver::bonding::BondingReceiver;
use rist_bonding_core::receiver::aggregator::ReassemblyConfig;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod imp {
    use super::*;

    struct Settings {
        links: String,
        latency: u32,
        config_toml: String,
        jitter_latency_multiplier: f64,
        max_latency_ms: u64,
        stats_interval_ms: u64,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                links: String::new(),
                latency: 50, // Default 50ms — matches ReceiverConfig::default().start_latency
                config_toml: String::new(),
                jitter_latency_multiplier: 4.0,
                max_latency_ms: 500,
                stats_interval_ms: 1000,
            }
        }
    }

    #[derive(Default)]
    pub struct RsRistBondSrc {
        settings: Mutex<Settings>,
        receiver: Mutex<Option<BondingReceiver>>,
        stats_running: Arc<AtomicBool>,
        stats_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
        /// Set by `unlock()` to interrupt the blocking `create()` call;
        /// cleared by `unlock_stop()` so normal operation can resume.
        flushing: AtomicBool,
    }

    impl RsRistBondSrc {
        fn apply_config_toml(&self, toml_str: &str) {
            if toml_str.trim().is_empty() {
                return;
            }
            match rist_bonding_core::config::BondingConfig::from_toml_str(toml_str) {
                Ok(cfg) => {
                    let mut settings = lock_or_recover(&self.settings);
                    settings.config_toml = toml_str.to_string();
                    // Apply receiver config: override latency and links if specified
                    settings.latency = cfg.receiver.start_latency.as_millis() as u32;
                    settings.jitter_latency_multiplier = cfg.scheduler.jitter_latency_multiplier;
                    settings.max_latency_ms = cfg.scheduler.max_latency_ms;
                    settings.stats_interval_ms = cfg.scheduler.stats_interval_ms;
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
                    gst::warning!(
                        gst::CAT_DEFAULT,
                        "RsRistBondSrc: Invalid config TOML: {}",
                        e
                    );
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
                        .default_value(50)
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
                    let mut settings = lock_or_recover(&self.settings);
                    settings.links = value.get().expect("type checked upstream");
                }
                "latency" => {
                    let mut settings = lock_or_recover(&self.settings);
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
                            self.apply_config_toml(&cfg);
                        }
                        Err(e) => {
                            gst::warning!(
                                gst::CAT_DEFAULT,
                                "RsRistBondSrc: Failed to read config file '{}': {}",
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
                "links" => {
                    let settings = lock_or_recover(&self.settings);
                    settings.links.to_value()
                }
                "latency" => {
                    let settings = lock_or_recover(&self.settings);
                    settings.latency.to_value()
                }
                "config" | "config-file" => {
                    let settings = lock_or_recover(&self.settings);
                    settings.config_toml.to_value()
                }
                _ => {
                    gst::warning!(gst::CAT_DEFAULT, "Unknown property: {}", pspec.name());
                    "".to_value()
                }
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
                    "Strata Contributors <https://github.com/rist-bonding>",
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
            let settings = lock_or_recover(&self.settings);
            let mut receiver_guard = lock_or_recover(&self.receiver);

            if receiver_guard.is_some() {
                return Ok(());
            }

            let latency_duration = Duration::from_millis(settings.latency as u64);
            let reassembly_config = ReassemblyConfig {
                start_latency: latency_duration,
                jitter_latency_multiplier: settings.jitter_latency_multiplier,
                max_latency_ms: settings.max_latency_ms,
                ..ReassemblyConfig::default()
            };
            let receiver = BondingReceiver::new_with_config(reassembly_config);

            for link in settings.links.split(',') {
                let link = link.trim();
                if link.is_empty() {
                    continue;
                }

                receiver.add_link(link).map_err(|e| {
                    let err_msg = format!("Failed to bind link {}: {}", link, e);
                    gst::error!(gst::CAT_DEFAULT, "RsRistBondSrc Error: {}", err_msg);
                    gst::error_msg!(gst::ResourceError::OpenRead, ["{}", err_msg])
                })?;
            }

            *receiver_guard = Some(receiver);

            // Setup stats thread
            self.stats_running.store(true, Ordering::Relaxed);
            let running = self.stats_running.clone();
            let element_weak = self.obj().downgrade();
            let stats_interval = Duration::from_millis(settings.stats_interval_ms);

            let handle = std::thread::Builder::new()
                .name("rist-rcv-stats".into())
                .spawn(move || {
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
                                    let alive_links = receiver.link_count();
                                    let msg = gst::Structure::builder("rist-bonding-stats")
                                        .field("schema_version", 3i32)
                                        .field("stats_seq", stats_seq)
                                        .field("heartbeat", true)
                                        .field("mono_time_ns", mono_time_ns)
                                        .field("wall_time_ms", wall_time_ms)
                                        .field("total_capacity", 0.0f64) // receiver has no capacity metric
                                        .field("alive_links", alive_links)
                                        .field("queue_depth", stats.queue_depth as u64)
                                        .field("next_seq", stats.next_seq)
                                        .field("lost_packets", stats.lost_packets)
                                        .field("late_packets", stats.late_packets)
                                        .field("duplicate_packets", stats.duplicate_packets)
                                        .field("current_latency_ms", stats.current_latency_ms)
                                        .build();
                                    let _ = element.post_message(gst::message::Element::new(msg));
                                    stats_seq = stats_seq.wrapping_add(1);
                                }
                            }
                        } else {
                            break;
                        }

                        std::thread::sleep(stats_interval);
                    }
                })
                .expect("failed to spawn receiver stats thread");
            *lock_or_recover(&self.stats_thread) = Some(handle);

            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            self.stats_running.store(false, Ordering::Relaxed);
            if let Some(handle) = lock_or_recover(&self.stats_thread).take() {
                let _ = handle.join();
            }

            let mut receiver_guard = lock_or_recover(&self.receiver);
            if let Some(mut receiver) = receiver_guard.take() {
                receiver.shutdown();
                // We leave flushing/closing to Drop?
            }
            Ok(())
        }

        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            // Non-destructive: just set the flushing flag so create() returns Flushing.
            // The receiver stays alive and can resume after unlock_stop().
            self.flushing.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
            self.flushing.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    impl PushSrcImpl for RsRistBondSrc {
        fn create(
            &self,
            _buf: Option<&mut gst::BufferRef>,
        ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
            let receiver_guard = lock_or_recover(&self.receiver);

            let rx = if let Some(receiver) = &*receiver_guard {
                receiver.output_rx.clone()
            } else {
                return Err(gst::FlowError::Eos);
            };
            drop(receiver_guard);

            // Use recv_timeout in a loop so we can check the flushing flag
            // periodically. This allows unlock() to interrupt us without
            // destroying the receiver.
            loop {
                if self.flushing.load(Ordering::SeqCst) {
                    return Err(gst::FlowError::Flushing);
                }

                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(bytes) => {
                        let buffer = gst::Buffer::from_slice(bytes);
                        return Ok(gst_base::subclass::base_src::CreateSuccess::NewBuffer(
                            buffer,
                        ));
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        // Loop back to check flushing flag
                        continue;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        return Err(gst::FlowError::Eos);
                    }
                }
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
