use crate::pad::RsRistBondSinkPad;
use crate::util::lock_or_recover;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use rist_bonding_core::config::{BondingConfig, LinkConfig, SchedulerConfig};
use rist_bonding_core::runtime::{BondingRuntime, PacketSendError};
use rist_bonding_core::scheduler::PacketProfile;
use rist_bonding_core::stats::{LinkStatsSnapshot, StatsSnapshot};
use std::collections::HashMap;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn parse_config(config: &str) -> Result<BondingConfig, String> {
    BondingConfig::from_toml_str(config)
}

/// NADA-derived rate signals per RFC 8698 §5.2.2.
///
/// Given the aggregate reference rate (`r_ref` — sum of per-link NADA
/// `estimated_capacity_bps`), derive:
///
/// - **`r_vin`** — target encoder bitrate:  `max(RMIN, r_ref × (1 − BETA_V))`
/// - **`headroom`** — bandwidth ceiling:    `min(RMAX, r_ref × (1 + BETA_S))`
///
/// Returns `None` when `r_ref` is zero (no alive links).
///
/// Constants:
///   RMIN    = 150 000 bps   (RFC 8698 default minimum)
///   BETA_V  = 0.05          (video smoothing margin — conservative)
///   BETA_S  = 0.05          (sending-rate margin)
fn compute_nada_rate_signals(r_ref: f64, max_capacity_bps: f64) -> Option<(u64, u64)> {
    if r_ref <= 0.0 {
        return None;
    }

    const RMIN: f64 = 150_000.0;
    const BETA_V: f64 = 0.05;
    const BETA_S: f64 = 0.05;

    let r_vin = (r_ref * (1.0 - BETA_V)).max(RMIN);

    let rmax = if max_capacity_bps > 0.0 {
        max_capacity_bps
    } else {
        f64::MAX
    };
    let headroom = (r_ref * (1.0 + BETA_S)).min(rmax);

    Some((r_vin.round() as u64, headroom.round() as u64))
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
        pub(crate) scheduler_config: Arc<Mutex<SchedulerConfig>>,
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
                scheduler_config: Arc::new(Mutex::new(SchedulerConfig::default())),
            }
        }
    }

    impl RsRistBondSink {
        pub(crate) fn send_msg(&self, msg: SinkMessage) {
            let runtime = lock_or_recover(&self.runtime);
            if let Some(rt) = &*runtime {
                match msg {
                    SinkMessage::AddLink { id, uri, iface } => {
                        if let Err(e) = rt.add_link(LinkConfig {
                            id,
                            uri: uri.clone(),
                            interface: iface,
                            recovery_maxbitrate: None,
                            recovery_rtt_max: None,
                            recovery_reorder_buffer: None,
                        }) {
                            gst::warning!(gst::CAT_DEFAULT, "Failed to add link {}: {}", uri, e);
                        }
                    }
                    SinkMessage::RemoveLink { id } => {
                        if let Err(e) = rt.remove_link(id) {
                            gst::warning!(gst::CAT_DEFAULT, "Failed to remove link {}: {}", id, e);
                        }
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
                                recovery_maxbitrate: None,
                                recovery_rtt_max: None,
                                recovery_reorder_buffer: None,
                            },
                        );
                    }
                    SinkMessage::RemoveLink { id } => {
                        lock_or_recover(&self.pending_links).remove(&id);
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
            if let Some(id) = lock_or_recover(&self.pad_map).remove(pad_name) {
                self.send_msg(SinkMessage::RemoveLink { id });
            }
        }

        fn get_id_for_pad(&self, pad_name: &str) -> usize {
            // Find existing or create ID
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
                        if let Err(e) = rt.apply_config(parsed) {
                            gst::warning!(gst::CAT_DEFAULT, "Failed to apply config: {}", e);
                        }
                    }
                }
                Err(err) => {
                    gst::warning!(gst::CAT_DEFAULT, "{}", err);
                }
            }
        }

        fn reconfigure_legacy(&self) {
            let config = lock_or_recover(&self.links_config).clone();
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
                    glib::ParamSpecString::builder("config-file")
                        .nick("Config File")
                        .blurb("Path to TOML config file (alternative to inline config property)")
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecUInt64::builder("max-bitrate")
                        .nick("Max Bitrate")
                        .blurb("Hard ceiling on per-link estimated capacity (bps). Set to encoder max-bitrate to derive RMAX (RFC 8698). 0 = disabled.")
                        .minimum(0)
                        .maximum(u64::MAX)
                        .default_value(0)
                        .mutable_playing()
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "links" => {
                    *lock_or_recover(&self.links_config) =
                        value.get().expect("type checked upstream");
                    self.reconfigure_legacy();
                }
                "config" => {
                    let cfg: String = value.get().expect("type checked upstream");
                    *lock_or_recover(&self.config_toml) = cfg.clone();
                    self.apply_config(&cfg);
                }
                "config-file" => {
                    let path: String = value.get().expect("type checked upstream");
                    if path.is_empty() {
                        return;
                    }
                    // Validate path: reject absolute paths outside expected directories
                    // to mitigate path traversal when set from untrusted input.
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
                "max-bitrate" => {
                    let bps: u64 = value.get().expect("type checked upstream");
                    let sched_clone = {
                        let mut sched = lock_or_recover(&self.scheduler_config);
                        sched.max_capacity_bps = bps as f64;
                        sched.clone()
                    };
                    // Live-update the runtime if already started
                    if let Some(rt) = lock_or_recover(&self.runtime).as_ref() {
                        if let Err(e) = rt.update_scheduler_config(sched_clone) {
                            gst::warning!(gst::CAT_DEFAULT, "Failed to update max-bitrate: {}", e);
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
                "links" => lock_or_recover(&self.links_config).to_value(),
                "config" | "config-file" => lock_or_recover(&self.config_toml).to_value(),
                "max-bitrate" => {
                    (lock_or_recover(&self.scheduler_config).max_capacity_bps as u64).to_value()
                }
                _ => {
                    gst::warning!(gst::CAT_DEFAULT, "Unknown property: {}", pspec.name());
                    "".to_value()
                }
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
                        if !lock_or_recover(&self.pad_map).contains_key(&candidate) {
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
            let sched_cfg = lock_or_recover(&self.scheduler_config).clone();
            let runtime = BondingRuntime::with_config(sched_cfg.clone());
            let metrics_handle = runtime.metrics_handle();
            *lock_or_recover(&self.runtime) = Some(runtime);

            for pad in self.obj().pads() {
                if let Some(bond_pad) = pad.downcast_ref::<RsRistBondSinkPad>() {
                    self.add_link_from_pad(bond_pad);
                }
            }

            if let Some(rt) = lock_or_recover(&self.runtime).as_ref() {
                let pending: Vec<LinkConfig> = lock_or_recover(&self.pending_links)
                    .drain()
                    .map(|(_, v)| v)
                    .collect();
                for link in pending {
                    if let Err(e) = rt.add_link(link) {
                        gst::warning!(gst::CAT_DEFAULT, "Failed to add pending link: {}", e);
                    }
                }
            }

            self.reconfigure_legacy();
            self.apply_config(&lock_or_recover(&self.config_toml));

            let element_weak = self.obj().downgrade();
            let running = self.stats_running.clone();
            running.store(true, Ordering::Relaxed);
            let sched_cfg_handle = Arc::clone(&self.scheduler_config);

            let handle = std::thread::Builder::new()
                .name("rist-bond-stats".into())
                .spawn(move || {
                    let mut last_stats = Instant::now();
                    let start = Instant::now();
                    let mut stats_seq: u64 = 0;

                    while running.load(Ordering::Relaxed) {
                        // Re-read stats_interval each iteration so runtime
                        // config changes take effect immediately.
                        let stats_interval = Duration::from_millis(
                            lock_or_recover(&sched_cfg_handle).stats_interval_ms,
                        );
                        if last_stats.elapsed() >= stats_interval {
                            if let Some(element) = element_weak.upgrade() {
                                let metrics = lock_or_recover(&metrics_handle).clone();
                                let mono_time_ns = start.elapsed().as_nanos() as u64;
                                let wall_time_ms = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);

                                let mut total_capacity = 0.0;
                                let mut total_nada_ref = 0.0;
                                let mut total_observed_bps = 0.0;
                                let mut alive_links = 0u64;
                                let mut links_map = std::collections::HashMap::new();

                                for (id, m) in &metrics {
                                    if m.alive {
                                        // total_capacity always sums raw link
                                        // capacity.
                                        total_capacity += m.capacity_bps;
                                        // aggregate_nada_ref uses the AIMD-
                                        // estimated rate when active, else the
                                        // raw capacity as fallback.
                                        if m.estimated_capacity_bps > 0.0 {
                                            total_nada_ref += m.estimated_capacity_bps;
                                        } else {
                                            total_nada_ref += m.capacity_bps;
                                        }
                                        total_observed_bps += m.observed_bps;
                                        alive_links += 1;
                                    }
                                    links_map
                                        .insert(id.to_string(), LinkStatsSnapshot::from_metrics(m));
                                }

                                let timestamp = if wall_time_ms > 0 {
                                    wall_time_ms as f64 / 1000.0
                                } else {
                                    0.0
                                };

                                let aggregate_nada_ref_bps = total_nada_ref;

                                let snapshot = StatsSnapshot {
                                    schema_version: 3,
                                    stats_seq,
                                    heartbeat: true,
                                    mono_time_ns,
                                    wall_time_ms,
                                    timestamp,
                                    total_capacity,
                                    aggregate_nada_ref_bps,
                                    alive_links,
                                    total_dead_drops: 0,
                                    links: links_map,
                                };

                                let stats_json =
                                    serde_json::to_string(&snapshot).unwrap_or_default();

                                let msg_struct = gst::Structure::builder("rist-bonding-stats")
                                    .field("schema_version", 3i32)
                                    .field("stats_json", &stats_json)
                                    .field("total_capacity", total_capacity)
                                    .field("alive_links", alive_links)
                                    .build();
                                let _ =
                                    element.post_message(gst::message::Element::new(msg_struct));

                                // Read max_capacity_bps from the shared
                                // scheduler config each iteration so runtime
                                // changes via the max-bitrate property are
                                // reflected immediately.
                                let max_capacity_bps =
                                    lock_or_recover(&sched_cfg_handle).max_capacity_bps;
                                if let Some((r_vin, headroom)) = compute_nada_rate_signals(
                                    aggregate_nada_ref_bps,
                                    max_capacity_bps,
                                ) {
                                    let cc_msg = gst::Structure::builder("congestion-control")
                                        .field("recommended-bitrate", r_vin)
                                        .field("aggregate-nada-ref-bps", aggregate_nada_ref_bps)
                                        .field("total_capacity", total_capacity)
                                        .field("observed_bps", total_observed_bps)
                                        .build();
                                    let _ =
                                        element.post_message(gst::message::Element::new(cc_msg));

                                    let bw_msg = gst::Structure::builder("bandwidth-available")
                                        .field("max-bitrate", headroom)
                                        .field("aggregate-nada-ref-bps", aggregate_nada_ref_bps)
                                        .field("total_capacity", total_capacity)
                                        .field("observed_bps", total_observed_bps)
                                        .build();
                                    let _ =
                                        element.post_message(gst::message::Element::new(bw_msg));
                                }
                            }
                            stats_seq = stats_seq.wrapping_add(1);
                            last_stats = Instant::now();
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                })
                .map_err(|e| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Failed to spawn sender stats thread: {}", e]
                    )
                })?;

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
            // Content-Aware Bonding:
            // 1. Critical: Broadcast Keyframes (IDR), Headers, and non-Delta (Audio) for reliability.
            // 2. Droppable: Allow dropping non-ref B-frames during congestion to preserve latency.
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
    use super::{compute_nada_rate_signals, parse_config};

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

    #[test]
    fn nada_rate_signals_derive_r_vin_and_headroom() {
        // 10 Mbps aggregate NADA r_ref, no RMAX ceiling
        let (r_vin, headroom) = compute_nada_rate_signals(10_000_000.0, 0.0).unwrap();
        // r_vin = 10M * 0.95 = 9.5M
        assert_eq!(r_vin, 9_500_000);
        // headroom = 10M * 1.05 = 10.5M
        assert_eq!(headroom, 10_500_000);
    }

    #[test]
    fn nada_rate_signals_clamp_to_rmin() {
        // Very low r_ref — r_vin should clamp to RMIN (150 kbps)
        let (r_vin, _headroom) = compute_nada_rate_signals(100_000.0, 0.0).unwrap();
        assert_eq!(r_vin, 150_000); // RMIN floor
    }

    #[test]
    fn nada_rate_signals_respect_rmax() {
        // 50 Mbps r_ref with 40 Mbps RMAX ceiling
        let (r_vin, headroom) = compute_nada_rate_signals(50_000_000.0, 40_000_000.0).unwrap();
        assert_eq!(r_vin, 47_500_000); // 50M * 0.95
        assert_eq!(headroom, 40_000_000); // clamped to RMAX
    }

    #[test]
    fn nada_rate_signals_return_none_for_zero() {
        assert!(compute_nada_rate_signals(0.0, 0.0).is_none());
        assert!(compute_nada_rate_signals(-1.0, 0.0).is_none());
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
