use crate::util::lock_or_recover;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::BaseSrcExt;
use gst_base::subclass::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use strata_bonding::receiver::ReceiverBackend;
use strata_bonding::receiver::aggregator::ReassemblyConfig;

mod imp {
    use super::*;

    struct Settings {
        links: String,
        latency: u32,
        max_latency_ms: u64,
        config_toml: String,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                links: String::new(),
                latency: 50,
                max_latency_ms: 800,
                config_toml: String::new(),
            }
        }
    }

    #[derive(Default)]
    pub struct StrataSrc {
        settings: Mutex<Settings>,
        pub(crate) receiver: Mutex<Option<ReceiverBackend>>,
        stats_running: Arc<AtomicBool>,
        stats_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
        /// Set by `unlock()` to interrupt the blocking `recv()` in `create()`.
        /// Cleared by `unlock_stop()` when the pipeline resumes.
        flushing: AtomicBool,
    }

    impl StrataSrc {
        fn apply_config_toml(&self, toml_str: &str) {
            if toml_str.trim().is_empty() {
                return;
            }
            match strata_bonding::config::BondingConfig::from_toml_str(toml_str) {
                Ok(cfg) => {
                    let mut settings = lock_or_recover(&self.settings);
                    settings.config_toml = toml_str.to_string();
                    settings.latency = cfg.receiver.start_latency.as_millis() as u32;
                    settings.max_latency_ms = cfg.scheduler.max_latency_ms;
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
                    gst::warning!(gst::CAT_DEFAULT, "StrataSrc: Invalid config TOML: {}", e);
                }
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StrataSrc {
        const NAME: &'static str = "StrataSrc";
        type Type = super::StrataSrc;
        type ParentType = gst_base::PushSrc;
    }

    impl ObjectImpl for StrataSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: std::sync::OnceLock<Vec<glib::ParamSpec>> =
                std::sync::OnceLock::new();

            PROPERTIES.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("links")
                        .nick("Links")
                        .blurb("Comma-separated list of bind addresses (e.g. '0.0.0.0:5000')")
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
                                "StrataSrc: Failed to read config file '{}': {}",
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

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_live(true);
            obj.set_format(gst::Format::Time);
            obj.set_do_timestamp(true);
        }
    }

    impl GstObjectImpl for StrataSrc {}

    impl ElementImpl for StrataSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static ELEMENT_METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
                std::sync::OnceLock::new();

            ELEMENT_METADATA.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "Strata Bonding Source",
                    "Source/Network",
                    "Receives packets via bonded Strata transport links",
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
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap(),
                ]
            });
            PAD_TEMPLATES.get().unwrap()
        }
    }

    impl BaseSrcImpl for StrataSrc {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            self.flushing.store(false, Ordering::SeqCst);

            let settings = lock_or_recover(&self.settings);
            let mut receiver_guard = lock_or_recover(&self.receiver);

            if receiver_guard.is_some() {
                return Ok(());
            }

            let latency_duration = Duration::from_millis(settings.latency as u64);
            let max_latency_ms = settings.max_latency_ms;
            let receiver = ReceiverBackend::new_with_config(ReassemblyConfig {
                start_latency: latency_duration,
                max_latency_ms,
                ..ReassemblyConfig::default()
            });

            for link in settings.links.split(',') {
                let link = link.trim();
                if link.is_empty() {
                    continue;
                }

                receiver.add_link(link).map_err(|e| {
                    let err_msg = format!("Failed to bind link {}: {}", link, e);
                    gst::error!(gst::CAT_DEFAULT, "StrataSrc Error: {}", err_msg);
                    gst::error_msg!(gst::ResourceError::OpenRead, ["{}", err_msg])
                })?;
            }

            *receiver_guard = Some(receiver);

            self.stats_running.store(true, Ordering::Relaxed);
            let running = self.stats_running.clone();
            let element_weak = self.obj().downgrade();

            let handle = std::thread::Builder::new()
                .name("strata-rcv-stats".into())
                .spawn(move || {
                    let start = Instant::now();
                    let mut stats_seq: u64 = 0;
                    while running.load(Ordering::Relaxed) {
                        if let Some(element) = element_weak.upgrade() {
                            let imp = element.imp();
                            if let Ok(receiver_guard) = imp.receiver.lock()
                                && let Some(receiver) = &*receiver_guard
                            {
                                let stats = receiver.get_stats();
                                let mono_time_ns = start.elapsed().as_nanos() as u64;
                                let wall_time_ms = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                                let msg = gst::Structure::builder("strata-stats")
                                    .field("schema_version", 1i32)
                                    .field("stats_seq", stats_seq)
                                    .field("heartbeat", true)
                                    .field("mono_time_ns", mono_time_ns)
                                    .field("wall_time_ms", wall_time_ms)
                                    .field("total_capacity", 0.0f64)
                                    .field("alive_links", 0u64)
                                    .field("queue_depth", stats.queue_depth as u64)
                                    .field("next_seq", stats.next_seq)
                                    .field("lost_packets", stats.lost_packets)
                                    .field("late_packets", stats.late_packets)
                                    .field("current_latency_ms", stats.current_latency_ms)
                                    .field("target_latency_ms", stats.target_latency_ms)
                                    .field("packets_delivered", stats.packets_delivered)
                                    .field("loss_rate", stats.loss_rate)
                                    .field("jitter_estimate_ms", stats.jitter_estimate_ms)
                                    .build();
                                let _ = element.post_message(gst::message::Element::new(msg));
                                stats_seq = stats_seq.wrapping_add(1);
                            }
                        } else {
                            break;
                        }

                        std::thread::sleep(Duration::from_secs(1));
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
            }
            Ok(())
        }

        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            // Signal the blocking recv() in create() to bail out.
            // We must NOT call receiver.shutdown() here — that permanently
            // destroys the receiver.  GStreamer calls unlock() during
            // state transitions (PLAYING→PAUSED) and expects create()
            // to return quickly; the receiver must survive for reuse.
            self.flushing.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
            self.flushing.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    impl PushSrcImpl for StrataSrc {
        fn create(
            &self,
            _buf: Option<&mut gst::BufferRef>,
        ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
            let receiver_guard = lock_or_recover(&self.receiver);

            let rx = if let Some(receiver) = &*receiver_guard {
                receiver.output_rx().clone()
            } else {
                return Err(gst::FlowError::Eos);
            };
            drop(receiver_guard);

            // Use recv_timeout so we can check the flushing flag
            // periodically instead of blocking forever (which would
            // prevent unlock() from taking effect).
            loop {
                if self.flushing.load(Ordering::SeqCst) {
                    return Err(gst::FlowError::Flushing);
                }
                match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok((bytes, discont)) => {
                        let mut buffer = gst::Buffer::from_slice(bytes);
                        if discont {
                            let buf_ref = buffer.get_mut().unwrap();
                            buf_ref.set_flags(gst::BufferFlags::DISCONT);
                        } else {
                            // Suppress tracing for every packet to avoid log spam
                            // println!("StrataSrc: producing buffer");
                        }
                        return Ok(gst_base::subclass::base_src::CreateSuccess::NewBuffer(
                            buffer,
                        ));
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        return Err(gst::FlowError::Eos);
                    }
                }
            }
        }
    }
}

glib::wrapper! {
    pub struct StrataSrc(ObjectSubclass<imp::StrataSrc>)
        @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

impl StrataSrc {
    /// Returns a shared handle to the receiver's reassembly stats.
    ///
    /// This can be passed to `ReceiverMetricsServer::start()` for Prometheus
    /// scraping. Returns `None` if the element hasn't been started yet.
    pub fn stats_handle(
        &self,
    ) -> Option<
        std::sync::Arc<std::sync::Mutex<strata_bonding::receiver::aggregator::ReassemblyStats>>,
    > {
        let receiver: std::sync::MutexGuard<'_, Option<ReceiverBackend>> =
            lock_or_recover(&self.imp().receiver);
        receiver.as_ref().map(|rx| rx.stats_handle())
    }
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    gst::Element::register(
        plugin,
        "stratasrc",
        gst::Rank::NONE,
        StrataSrc::static_type(),
    )
}
