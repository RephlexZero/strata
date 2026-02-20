use anyhow::Result;
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use strata_bonding::config::LinkConfig;
use strata_bonding::receiver::transport::TransportBondingReceiver;
use strata_bonding::runtime::BondingRuntime;
use strata_bonding::scheduler::PacketProfile;
use tokio::net::UdpSocket;
use tokio::time;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .compact()
        .init();

    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("Missing mode (sender/receiver)");

    let mut bind_addrs = Vec::new();
    let mut dest_addrs = Vec::new();
    let mut stats_dest = None;
    let mut bitrate_kbps = 2000;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                let addrs = args.next().expect("Missing --bind value");
                for addr in addrs.split(',') {
                    bind_addrs.push(addr.parse::<SocketAddr>()?);
                }
            }
            "--dest" => {
                let addrs = args.next().expect("Missing --dest value");
                for addr in addrs.split(',') {
                    // Format: IP:PORT?rtt-min=60&buffer=2000
                    let ip_port = addr.split('?').next().unwrap();
                    dest_addrs.push(ip_port.parse::<SocketAddr>()?);
                }
            }
            "--stats-dest" => {
                stats_dest = Some(
                    args.next()
                        .expect("Missing --stats-dest value")
                        .parse::<SocketAddr>()?,
                );
            }
            "--bitrate" => {
                bitrate_kbps = args.next().expect("Missing --bitrate value").parse()?;
            }
            "--codec" | "--resolution" | "--framerate" => {
                // Ignore these args from the old tests
                args.next();
            }
            _ => {}
        }
    }

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            running.store(false, Ordering::Relaxed);
        });
    }

    if mode == "sender" {
        eprintln!(
            "Starting sender with dests: {:?}, bitrate: {}",
            dest_addrs, bitrate_kbps
        );
        run_sender(dest_addrs, stats_dest, bitrate_kbps, running).await?;
    } else if mode == "receiver" {
        eprintln!("Starting receiver with binds: {:?}", bind_addrs);
        run_receiver(bind_addrs, stats_dest, running).await?;
    } else {
        anyhow::bail!("Unknown mode: {}", mode);
    }

    Ok(())
}

/// Simulates a video Group of Pictures (GOP) structure to produce realistic
/// [`PacketProfile`] distributions for the bonding scheduler.
///
/// Pattern (repeating): `I BB P BB P BB P` — matching a typical H.264 closed GOP
/// at 30 fps with 3 B-frames between each reference frame.
///
/// Packet counts are proportional to real codec output ratios, so the scheduler
/// sees a realistic mix of critical (I), reference (P), and droppable (B) packets
/// regardless of the actual send rate. This is needed to exercise the
/// [`DegradationStage`] logic (e.g. `DropDisposable`, `KeyframeOnly`).
struct GopSimulator {
    packet_seq: u64,
    /// Cumulative packet boundary and profile for each frame slot in the GOP.
    frame_boundaries: Vec<(usize, PacketProfile)>,
    gop_total: usize,
}

impl GopSimulator {
    fn new() -> Self {
        // Relative packet counts per frame type.
        // I-frame  ≈ 10× a B-frame  (keyframe, much larger),
        // P-frame  ≈  3× a B-frame  (reference, medium),
        // B-frame  ≈  1× baseline   (non-reference, droppable).
        const I_PKTS: usize = 20;
        const P_PKTS: usize = 6;
        const B_PKTS: usize = 2;

        let i_prof = PacketProfile {
            is_critical: true,
            can_drop: false,
            size_bytes: 1200,
        };
        let p_prof = PacketProfile {
            is_critical: false,
            can_drop: false,
            size_bytes: 1200,
        };
        let b_prof = PacketProfile {
            is_critical: false,
            can_drop: true,
            size_bytes: 1200,
        };

        // GOP layout:  I  B B  P  B B  P  B B  P  (10 logical frames)
        let frames: &[(usize, PacketProfile)] = &[
            (I_PKTS, i_prof),
            (B_PKTS, b_prof),
            (B_PKTS, b_prof),
            (P_PKTS, p_prof),
            (B_PKTS, b_prof),
            (B_PKTS, b_prof),
            (P_PKTS, p_prof),
            (B_PKTS, b_prof),
            (B_PKTS, b_prof),
            (P_PKTS, p_prof),
        ];

        let gop_total: usize = frames.iter().map(|(n, _)| n).sum();
        let mut boundaries = Vec::with_capacity(frames.len());
        let mut cum = 0usize;
        for &(count, profile) in frames {
            cum += count;
            boundaries.push((cum, profile));
        }

        Self {
            packet_seq: 0,
            frame_boundaries: boundaries,
            gop_total,
        }
    }

    /// Returns the [`PacketProfile`] for the next packet in the GOP cycle.
    fn next_profile(&mut self) -> PacketProfile {
        let pos = (self.packet_seq as usize) % self.gop_total;
        self.packet_seq += 1;
        for &(boundary, profile) in &self.frame_boundaries {
            if pos < boundary {
                return profile;
            }
        }
        PacketProfile::default()
    }
}

async fn run_sender(
    dest_addrs: Vec<SocketAddr>,
    stats_dest: Option<SocketAddr>,
    bitrate_kbps: u32,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let mut sender = BondingRuntime::new();

    for (id, dest) in dest_addrs.into_iter().enumerate() {
        sender.add_link(LinkConfig {
            id,
            uri: format!("strata://{}", dest),
            interface: None,
        })?;
    }

    let stats_handle = sender.metrics_handle();
    let stats_socket = if let Some(dest) = stats_dest {
        let sock = UdpSocket::bind("0.0.0.0:0").await?;
        sock.connect(dest).await?;
        Some(sock)
    } else {
        None
    };

    let mut stats_interval = time::interval(Duration::from_millis(200));
    let mut current_bitrate_bps = bitrate_kbps as f64 * 1000.0;
    let mut packet_interval = time::interval(Duration::from_micros(
        ((1_000_000.0 * 8.0 * 1200.0) / current_bitrate_bps) as u64,
    ));

    let mut seq = 0u64;
    let payload = vec![0u8; 1200];
    let mut gop = GopSimulator::new();

    while running.load(Ordering::Relaxed) {
        tokio::select! {
            _ = packet_interval.tick() => {
                let mut data = payload.clone();
                data[0..8].copy_from_slice(&seq.to_be_bytes());
                let profile = gop.next_profile();
                if let Err(e) = sender.try_send_packet(Bytes::from(data), profile) {
                    tracing::error!("Failed to send packet: {:?}", e);
                }
                seq += 1;
            }
            _ = stats_interval.tick() => {
                let stats = stats_handle.lock().unwrap().clone();

                // Adapt sending rate to estimated capacity (like BitrateAdapter does)
                let total_capacity: f64 = stats.values().map(|m| m.estimated_capacity_bps).sum();
                let total_observed: f64 = stats.values().map(|m| m.observed_bps).sum();

                // If capacity is 0 (e.g. no ACKs yet), fall back to observed throughput
                // to allow the link to probe and discover capacity.
                let effective_capacity = if total_capacity > 0.0 {
                    total_capacity
                } else {
                    total_observed.max(500_000.0) // Floor at 500 kbps to keep probing
                };

                // Target 90% of capacity, bounded by the CLI bitrate
                let target_bps = (effective_capacity * 0.9)
                    .min(bitrate_kbps as f64 * 1000.0)
                    .max(500_000.0);
                // Smooth the transition
                current_bitrate_bps = 0.8 * current_bitrate_bps + 0.2 * target_bps;
                let new_interval = Duration::from_micros(
                    ((1_000_000.0 * 8.0 * 1200.0) / current_bitrate_bps) as u64,
                );
                if new_interval != packet_interval.period() {
                    packet_interval = time::interval(new_interval);
                }

                if let Some(sock) = &stats_socket {
                    let mut links = Vec::new();
                    for metrics in stats.values() {
                        // Use receiver-reported goodput when available;
                        // falls back to sender observed_bps during warmup.
                        let reported_bps = if let Some(ref rr) = metrics.receiver_report {
                            if rr.goodput_bps > 0 { rr.goodput_bps as f64 } else { metrics.observed_bps }
                        } else {
                            metrics.observed_bps
                        };
                        links.push(serde_json::json!({
                            "observed_bps": reported_bps,
                            "rtt_ms": metrics.rtt_ms,
                            "loss_ratio": metrics.loss_rate,
                            // BiscayController BBR-based capacity estimate (from ACK feedback)
                            "estimated_capacity_bps": metrics.estimated_capacity_bps,
                            // Cumulative bytes sent on this link since process start
                            "sent_bytes": metrics.observed_bytes,
                        }));
                    }
                    let json = serde_json::json!({
                        "timestamp_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),
                        // Sender's current adapted bitrate (bps), follows capacity estimate
                        "current_bitrate_bps": current_bitrate_bps,
                        "links": links
                    });
                    if let Ok(json_str) = serde_json::to_string(&json) {
                        let _ = sock.send(json_str.as_bytes()).await;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_receiver(
    bind_addrs: Vec<SocketAddr>,
    stats_dest: Option<SocketAddr>,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let receiver = TransportBondingReceiver::new(Duration::from_millis(2000));

    for bind in bind_addrs {
        receiver.add_link(bind)?;
    }

    let stats_handle = receiver.stats_handle();
    let stats_socket = if let Some(dest) = stats_dest {
        let sock = UdpSocket::bind("0.0.0.0:0").await?;
        sock.connect(dest).await?;
        Some(sock)
    } else {
        None
    };

    let mut stats_interval = time::interval(Duration::from_millis(200));

    // Run the blocking recv in a separate thread to avoid blocking the async executor
    let rx = receiver.output_rx.clone();
    let running_clone = running.clone();
    std::thread::spawn(move || {
        while running_clone.load(Ordering::Relaxed) {
            if rx.recv_timeout(Duration::from_millis(100)).is_err() {
                continue;
            }
        }
    });

    while running.load(Ordering::Relaxed) {
        tokio::select! {
            _ = stats_interval.tick() => {
                if let Some(_sock) = &stats_socket {
                    let _stats = stats_handle.lock().unwrap().clone();
                    // We don't strictly need receiver stats for the tests right now,
                    // but we can emit them if needed.
                }
            }
        }
    }

    Ok(())
}
