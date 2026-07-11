//! # Transport Adapter
//!
//! Bridges `strata-transport`'s Sender/Receiver with the bonding crate's
//! `LinkSender` trait. This adapter encapsulates the wire-format encode/decode
//! and reliability layer (FEC + ARQ) behind the existing scheduling interface.
//!
//! Uses `quinn-udp` for GSO (Generic Segmentation Offload) batched sends,
//! reducing per-packet syscall overhead.

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use quinn_udp::{Transmit, UdpSockRef, UdpSocketState};
use std::net::UdpSocket;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crate::net::interface::{LinkMetrics, LinkPhase, LinkSender};
use crate::scheduler::oracle::CapacityOracle;
use strata_transport::congestion::{BbrPhase, BiscayController, BiscayState};

/// Per-link token bucket for send-path pacing.
///
/// Refills at the BiscayController's pacing_rate (bytes/sec) with a burst cap
/// of 100 ms worth of data.  Tokens are deducted after each send based on
/// actual wire bytes (including FEC/ARQ overhead).  When tokens are negative
/// the next send is rejected, which the scheduler records as a failed send and
/// feeds into congestion detection.
struct PacingState {
    tokens: f64,
    last_refill: std::time::Instant,
}

/// Compute a pacing throttle factor from smoothed RTT vs observed minimum RTT.
///
/// When `srtt` rises above `min_rtt` the forwarding queue is filling — we are
/// pushing faster than the link drains.  Packet loss only starts AFTER the
/// queue overflows, so this RTT-growth signal is strictly earlier than loss.
///
/// Returns a multiplier in `[0.25, 1.0]` to apply to the base pacing rate:
///   ratio ≤ 1.5 → 1.0  (normal jitter, no throttle)
///   ratio = 2.0 → 0.75
///   ratio = 3.0 → 0.50
///   ratio ≥ 6.0 → 0.25 (clamped floor — keep probing, don't stall fully)
///
/// The 0.25 floor prevents a feedback loop where the throttle collapses the
/// link faster than its queue can drain.
fn rtt_bufferbloat_throttle(srtt_us: f64, min_rtt_us: f64) -> f64 {
    if srtt_us <= 0.0 || !min_rtt_us.is_finite() || min_rtt_us <= 0.0 {
        return 1.0;
    }
    let ratio = srtt_us / min_rtt_us;
    if ratio > 1.5 {
        (1.5 / ratio).clamp(0.25, 1.0)
    } else {
        1.0
    }
}
use strata_transport::pool::Priority;
use strata_transport::pool::TimestampClock;
use strata_transport::sender::{Sender, SenderConfig};
use strata_transport::session::RttTracker;
use strata_transport::wire::{Packet, PacketHeader, ReceiverReportPacket};

/// Explicit state for whether receiver feedback on this link is
/// probe-contaminated and should be ignored by the `BitrateAdapter`.
///
/// Replaces a far-future-`Instant` sentinel (`Instant::now() +
/// PROBE_FEEDBACK_COOLDOWN * 100`) that was used to "hold the block open"
/// while a probe ran. That trick made the running-probe state indistinguishable
/// from a very long cooldown from the reader's side — if a probe ever failed
/// to call `set_saturation_probe_active(false)` (crash, early return), feedback
/// was silently ignored for 150 s with nothing in the code explaining why.
/// With this enum the two situations are distinct, inspectable states.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ProbeFeedbackBlock {
    /// No probe has run recently; feedback is trustworthy.
    Clear,
    /// A saturation probe is actively pinning traffic on this link right
    /// now — feedback is unconditionally contaminated until the probe ends.
    ProbeRunning,
    /// The probe ended; feedback remains contaminated until this deadline
    /// (see `PROBE_FEEDBACK_COOLDOWN`).
    Cooldown(Instant),
}

impl ProbeFeedbackBlock {
    /// Whether feedback should currently be treated as contaminated.
    fn is_blocked(&self, now: Instant) -> bool {
        match self {
            ProbeFeedbackBlock::Clear => false,
            ProbeFeedbackBlock::ProbeRunning => true,
            ProbeFeedbackBlock::Cooldown(deadline) => now < *deadline,
        }
    }
}

/// A link backed by `strata-transport::Sender`.
///
/// Uses `quinn-udp` for GSO/GRO-accelerated UDP I/O when the kernel supports
/// it (Linux 4.18+ for GSO, 5.13+ for GRO).
pub struct TransportLink {
    /// Unique link ID.
    id: usize,
    /// The strata-transport sender (handles FEC, ARQ, wire format).
    sender: Mutex<Sender>,
    /// RTT tracker for this link.
    rtt: Mutex<RttTracker>,
    /// Clock for generating timestamps.
    clock: Mutex<TimestampClock>,
    /// Biscay congestion controller (BBR-based capacity estimation).
    congestion: Mutex<BiscayController>,
    /// UDP socket for this link.
    socket: UdpSocket,
    /// quinn-udp socket state for GSO/GRO.
    udp_state: UdpSocketState,
    /// Remote peer address (for quinn-udp Transmit).
    peer_addr: std::net::SocketAddr,
    /// Total bytes sent through this link.
    bytes_sent: AtomicU64,
    /// Total packets sent.
    packets_sent: AtomicU64,
    /// Packets silently deleted by the paced-queue AQM
    /// (`enforce_paced_queue_bound`). Every one of these is a hole the
    /// receiver must FEC/NACK its way around — loopback measurement showed
    /// this self-inflicted loss (~2.3%) dominating real link loss, so it
    /// must never again be invisible.
    aqm_dropped_pkts: AtomicU64,
    /// Bytes deleted by the paced-queue AQM.
    aqm_dropped_bytes: AtomicU64,
    /// AQM-deleted packets that were NACK retransmissions — each one is a
    /// repair the receiver asked for and silently never got.
    aqm_dropped_retx: AtomicU64,
    /// Rate limiter for the AQM drop warning log.
    aqm_last_log: Mutex<Instant>,
    /// Snapshot of `bytes_sent` at the last rate computation.
    prev_rate_bytes: AtomicU64,
    /// Microsecond timestamp of the last rate computation.
    prev_rate_time_us: AtomicU64,
    /// EWMA-smoothed socket-level sending rate (bits/sec).
    rate_ewma_bps: Mutex<f64>,
    /// EWMA-smoothed receiver goodput (bits/sec).
    goodput_ewma_bps: Mutex<f64>,
    /// Cumulative bytes acknowledged (for delivery rate tracking).
    bytes_acked: AtomicU64,
    /// Microsecond timestamp of last delivery rate sample.
    prev_ack_time_us: AtomicU64,
    /// Snapshot of bytes_acked at last delivery rate sample.
    prev_ack_bytes: AtomicU64,
    /// EWMA-smoothed ACK rate (bits/sec), from global total_received.
    ack_rate_ewma_bps: Mutex<f64>,
    /// Per-link ACK rate (bits/sec), from per-link packets_acked.
    per_link_ack_rate_bps: Mutex<f64>,
    /// Previous per-link packets_acked snapshot.
    prev_pkts_acked: AtomicU64,
    /// Timestamp of previous per-link packets_acked snapshot.
    prev_pkts_acked_us: AtomicU64,
    /// True once the per-link ACK-rate EWMA has decayed to zero during a
    /// delivery stall. The first non-zero ACK interval after a stall is a
    /// *stall-release burst* — thousands of cumulative ACKs flushed at once
    /// (carrier NAT rebind) compressed into one ~500 ms window, which
    /// over-states the true rate by 100×+. That single sample is discarded
    /// for capacity purposes; the next clean interval seeds the rate.
    ack_rate_was_zeroed: AtomicBool,
    /// Latest receiver report from the remote receiver (if any).
    receiver_report: Mutex<Option<ReceiverReportPacket>>,
    /// Most recent receiver-side cumulative `bytes_delivered` for this link.
    /// Updated on every ReceiverReport. Used by the sender's saturation-probe
    /// path to compute receiver-observed throughput, which is independent of
    /// modem TX queue depth (sender's `observed_bytes` is not).
    last_recv_bytes_delivered: AtomicU64,
    /// `Instant` when the most recent ReceiverReport for this link was
    /// processed. Used to detect when a fresh report has arrived after a
    /// saturation probe window closes.
    last_recv_report_at: Mutex<Instant>,
    /// Network interface name (e.g. "eth1").
    iface: Option<String>,
    /// Token bucket pacer — limits per-link send rate to pacing_rate.
    pacing: Mutex<PacingState>,
    /// Paced send queue.
    paced_queue: Mutex<std::collections::VecDeque<strata_transport::sender::OutputPacket>>,
    /// When (`mono_now_us`) the pacer last observed the paced queue empty.
    /// An ACK-rate sample whose interval contains this instant is app-limited:
    /// the wire went unfilled, so the measured delivery rate reflects the
    /// app's send rate, not link capacity (see the `on_bandwidth_sample`
    /// call site).
    paced_queue_last_empty_us: AtomicU64,
    /// Capacity oracle — independent of BBR btl_bw.
    oracle: Mutex<CapacityOracle>,
    /// Previous retransmissions snapshot for per-interval loss_rate.
    prev_retransmissions: AtomicU64,
    /// Previous packets_sent snapshot for per-interval loss_rate.
    prev_loss_pkts_sent: AtomicU64,
    /// While blocked (`ProbeRunning` or `Cooldown` before its deadline), a
    /// saturation probe is active on this link OR within the post-probe
    /// cooldown. Receiver reports covering this window are contaminated by
    /// the probe's traffic pin (the disturbance shows up ~1 report interval
    /// *after* the window closes), so the encoder `BitrateAdapter` ignores
    /// feedback while blocked. See `ProbeFeedbackBlock`.
    probe_feedback_block: Mutex<ProbeFeedbackBlock>,
    /// `packets_acked` snapshot from the previous `get_metrics`, used to
    /// detect ACK *progress* (not just a nonzero total).
    prev_acked_liveness: AtomicU64,
    /// When ACK progress was last observed (packets_acked increased) OR a
    /// receiver report last arrived. Seeded at construction. Used purely to
    /// detect a link that is *currently* delivering nothing so its
    /// scheduling weight can be crushed (NOT to latch it dead).
    last_ack_or_report: Mutex<Instant>,
    /// Whether the last `get_metrics` classified this link delivery-starved.
    /// Used only to log the starved↔recovered *transitions* instead of
    /// spamming a WARN every metrics tick (~10 Hz) while starved.
    was_delivery_starved: std::sync::atomic::AtomicBool,
    /// `(last_resize_at, last_target_bytes)` throttle for the dynamic
    /// `SO_SNDBUF` sizing (F2/ex-F4). `(_, 0)` = never resized yet.
    sndbuf_state: Mutex<(std::time::Instant, usize)>,
}

/// A link is only treated as delivery-starved once it has sent at least
/// this many packets — gives startup a grace window before the first
/// ACKs/reports have had time to return.
const STARVED_MIN_SENT: u64 = 40;

/// How long a link may go without ACK progress or a receiver report
/// (after sending meaningful traffic) before its scheduling weight is
/// crushed to a probe trickle. ~3 s comfortably exceeds a bonded cellular
/// RTT plus the 1 s receiver-report cadence.
///
/// Crucially this is NOT a death sentence: the link stays `alive`, keeps
/// receiving a thin trickle (and periodic saturation probes), and its
/// capacity is restored automatically on the *next* metric tick after a
/// single ACK or receiver report arrives. There is no sticky "dead"
/// state — a transient cellular loss burst can no longer permanently
/// remove a link from the bond.
const STARVED_STALE: std::time::Duration = std::time::Duration::from_secs(3);

/// Capacity (bits/sec) a delivery-starved link is pinned to. Small enough
/// that EDPF deprioritises it to a trickle (its `predicted_arrival`
/// balloons) yet non-zero so it still gets occasional packets and
/// periodic saturation probes — the mechanism by which a recovered link
/// re-admits itself. Roughly one 1300 B packet every ~150 ms.
const STARVED_CAPACITY_FLOOR_BPS: f64 = 64_000.0;

/// Capacity (bits/sec) a *hard-blackholed* link is pinned to — one that has
/// sent meaningful traffic but had **zero** ACK progress ever (not a
/// transient dip: `packets_acked == 0`). 64 kbps still earns a recovered
/// link real EDPF share; a link that has *never once* delivered a packet
/// must get effectively none while still keeping `alive=true` and its
/// periodic saturation probe (the self-readmission path). ~4 kbps is one
/// ~1300 B probe every ~2.6 s — enough to re-test the path, negligible
/// scheduling weight so the bond never dumps media into a proven hole.
const STARVED_HARD_BLACKHOLE_FLOOR_BPS: f64 = 4_000.0;

/// Maximum factor by which the passive CapacityOracle estimate may exceed
/// BBR's `btl_bw` before it is rejected as contaminated. `btl_bw` is the
/// physically-grounded windowed-max-filter bottleneck estimate; a passive
/// delivery-rate sample legitimately exceeds it only modestly (ACK
/// batching, brief bursts). A 4×+ excess is a stall-release ACK burst or
/// similar wrong-signal artifact (field: oracle 43.8 Mbps vs btl_bw
/// 1.26 Mbps on a link that delivered ~0). Same doctrinal invariant as the
/// probe-poisoning fix: a contaminated passive sample must never override
/// the steady-state physical estimate.
const ORACLE_SANE_BTLBW_MULT: f64 = 4.0;

// Compile-time invariants: the hard-blackhole floor must be strictly below
// the transient-starve trickle, and the oracle sanity multiple must stay in
// a conservative band (reject stall-release bursts — 30×+ btl_bw in the
// field — without clipping normal ACK-batching headroom).
const _: () = assert!(STARVED_HARD_BLACKHOLE_FLOOR_BPS < STARVED_CAPACITY_FLOOR_BPS);
const _: () = assert!(ORACLE_SANE_BTLBW_MULT >= 2.0 && ORACLE_SANE_BTLBW_MULT <= 8.0);

/// Cooldown after a saturation probe's send window closes during which
/// receiver feedback is still treated as contaminated. Receiver reports
/// are sent at ~1 s cadence and the probe pin perturbs the link for the
/// following report interval, so ~1.5 s covers the contaminated samples.
const PROBE_FEEDBACK_COOLDOWN: std::time::Duration = std::time::Duration::from_millis(1500);

/// Headroom multiple on the BDP for the inflight / paced-queue cap.
/// `queue ≤ k·(btl_bw × RTprop)`. Auto-scales with the path — no
/// per-regime constant. ~1.25 ≈ one quarter-BDP of slack for jitter.
const BDP_QUEUE_K: f64 = 1.25;

/// Bootstrap paced-queue byte budget used only until the BDP is known
/// (no bandwidth/RTprop estimate yet). ~100 × 1400 B — the old fixed cap,
/// retained purely as a startup guardrail before measurement converges.
const PACED_QUEUE_BOOTSTRAP_BYTES: usize = 140_000;

/// Hard floor for the BDP-derived paced-queue budget: one GSO superpacket
/// (max UDP datagram). Never shrink the queue below a single batched send
/// or a GSO flush would be starved mid-assembly.
const GSO_SUPERPACKET_BYTES: usize = 65_536;

/// Maximum time a packet may sit in the paced queue before the AQM is
/// allowed to cut it, expressed as a drain-time byte budget
/// (`pacing_rate × this`). Worst-case added queue latency is therefore
/// 500 ms — well inside the broadcast playout window (1.5-3 s) and the
/// NACK retransmit budget (10 × 100 ms). Encoder frame bursts (an IDR is
/// several × the per-frame average) drain in well under this and must
/// survive intact; cutting them was the dominant source of mid-GOP holes.
const PACED_QUEUE_SOJOURN_BUDGET_SECS: f64 = 0.5;

/// Minimum delivery-rate sample (bits/sec) treated as a real signal rather
/// than measurement noise near zero. Gates the goodput-vs-ack-rate delivery
/// signal choice and the btl_bw-capping/fallback logic in `get_metrics`.
/// ~100 kbps is comfortably above single-sample ACK-timing jitter yet far
/// below any link this bonding stack targets.
const MEANINGFUL_BASELINE_BPS: f64 = 100_000.0;

/// Floor on the pacing rate as a fraction of the oracle's slow-decaying
/// peak capacity estimate. Stops the congestion controller from starving a
/// link below 20% of what it has proven capable of, which would otherwise
/// create a death spiral (oracle collapse → pacing collapse → less
/// delivery → further oracle collapse).
const PACING_FLOOR_VS_PEAK: f64 = 0.2;

/// Token-bucket burst window: caps accumulated tokens at this many seconds
/// of data at the current pacing rate. Bounds how much a long idle period
/// can let the bucket build up before the next send.
const TOKEN_BUCKET_BURST_SECS: f64 = 0.01;

/// Floor on the token-bucket burst cap in bytes, applied regardless of
/// pacing rate — keeps the bucket able to burst enough for initial probes
/// before the pacing rate has ramped up from zero.
const TOKEN_BUCKET_MIN_BURST_BYTES: f64 = 10_000.0;

/// Clamp band (microseconds) for the SRTT-derived ACK-rate sampling
/// interval: never sample faster than this (avoids spikes from batched
/// ACKs) even if SRTT is tiny.
const ACK_RATE_MIN_INTERVAL_US: u64 = 250_000;
/// Upper end of the same clamp band — never wait longer than this to take
/// a sample even if SRTT is large, keeping the rate estimate responsive.
const ACK_RATE_MAX_INTERVAL_US: u64 = 1_000_000;

/// Multiple of SRTT beyond which an ACK-rate sampling interval is treated
/// as an idle gap (scheduler burst boundary) rather than a real
/// measurement — the interval's baseline is reset instead of computing a
/// rate diluted by idle time.
const ACK_RATE_IDLE_GAP_SRTT_MULT: u64 = 4;
/// Clamp band (microseconds) for the idle-gap threshold itself, in case
/// SRTT is momentarily very small or very large.
const ACK_RATE_IDLE_GAP_MIN_US: u64 = 500_000;
const ACK_RATE_IDLE_GAP_MAX_US: u64 = 2_000_000;

const _: () = assert!(ACK_RATE_MIN_INTERVAL_US < ACK_RATE_MAX_INTERVAL_US);
const _: () = assert!(ACK_RATE_IDLE_GAP_MIN_US < ACK_RATE_IDLE_GAP_MAX_US);

/// Minimum interval (microseconds) between per-link ACK-rate samples in
/// `get_metrics`'s `per_link_ack_rate` path — a fixed 500 ms band, distinct
/// from the SRTT-derived `ACK_RATE_*` window above (different accounting
/// path: per-link bytes_acked vs. the global total_received counter), used
/// to avoid spikes from batched ACKs.
const PER_LINK_ACK_RATE_MIN_INTERVAL_US: u64 = 500_000;

/// Cap on BBR's `btl_bw` relative to the measured per-link ACK rate:
/// `btl_bw` is a windowed-max filter so it can lag a fast-rising ACK rate,
/// and this bounds how far it is allowed to run ahead before being clipped
/// back toward the ACK-confirmed rate. Adjacent to
/// `ACK_RATE_FALLBACK_HEADROOM_MULT` below but NOT confirmed co-tuned with
/// it (per audit) — don't merge them into one constant.
const BTLBW_VS_ACK_RATE_CAP_MULT: f64 = 1.5;

/// EWMA weight for ACK-rate estimates (the recurring 0.2 "measurement
/// smoothing" weight — wiki/Adaptation-EWMA-Conventions.md §1b).
const ACK_RATE_EWMA_ALPHA: f64 = 0.2;

/// EWMA weight for receiver-report goodput (balanced 0.5: the report is
/// already a 1 s aggregate, so it needs little extra smoothing).
const GOODPUT_REPORT_EWMA_ALPHA: f64 = 0.5;

/// Absolute sanity ceiling on the BBR btl_bw capacity input (50 Mbps) —
/// mirrors oracle.rs's `PPD_ABSOLUTE_CEILING_BPS` (deliberately per-file,
/// like `MEANINGFUL_BASELINE_BPS`; keep the values in sync). Well above any
/// bonded-cellular reality today; will need raising for wired/5G bonding.
const BTLBW_ABSOLUTE_CEILING_BPS: f64 = 50_000_000.0;

/// Headroom applied to the ACK delivery rate when it is used as the
/// capacity fallback (no oracle estimate, no btl_bw yet), so the adapter
/// doesn't fall back to the static capacity floor when actual achievable
/// throughput is already known from ACK measurements. Adjacent to
/// `BTLBW_VS_ACK_RATE_CAP_MULT` above but NOT confirmed co-tuned with it
/// (per audit) — don't merge them into one constant.
const ACK_RATE_FALLBACK_HEADROOM_MULT: f64 = 1.2;

/// Microsecond monotonic clock shared by the ACK-rate sampler and the
/// paced-queue empty stamp — a single epoch so the two timestamps are
/// directly comparable for app-limited detection.
fn mono_now_us() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
}

impl TransportLink {
    /// The paced-queue byte budget shared by the AQM
    /// (`enforce_paced_queue_bound`) and retransmit admission control:
    /// `max(k×BDP, pacing_rate × sojourn budget, one GSO superpacket)`.
    ///
    /// The drain-time term (Little's law: the queue empties at pacing_rate,
    /// so `rate × budget` bounds the oldest packet's sojourn directly)
    /// exists because the BDP cap alone collapses to the GSO floor on
    /// short-RTT paths (loopback BDP ≈ 30 B) and modest-RTT cellular
    /// (BDP ≈ 30 KB), where a single keyframe burst is larger than the
    /// floor — measured 2.3% SELF-inflicted loss on loopback with zero
    /// network loss, every drop a mid-GOP hole. A burst that drains within
    /// the sojourn budget must never be cut; a queue standing past it is
    /// genuine overload (the adapter's job) and still gets bounded.
    fn paced_queue_cap_bytes(&self) -> usize {
        let cc = self.congestion.lock().unwrap();
        let bdp_cap = cc.inflight_cap_bytes(BDP_QUEUE_K);
        let drain_cap = cc.pacing_rate() * PACED_QUEUE_SOJOURN_BUDGET_SECS;
        if bdp_cap > 0.0 {
            (bdp_cap.max(drain_cap) as usize).max(GSO_SUPERPACKET_BYTES)
        } else {
            PACED_QUEUE_BOOTSTRAP_BYTES
        }
    }

    /// Enforce the byte bound on the paced queue with keyframe-protected
    /// oldest-drop (see `paced_queue_cap_bytes` for the bound itself).
    ///
    /// This is the F2/F4 AQM: the only queue strata actually owns is this
    /// userspace one, so we bound *it* instead of poking `tc`. Oldest
    /// low-priority packets are dropped first; keyframes/config
    /// (priority ≥ Reference) are preserved until nothing else remains.
    fn enforce_paced_queue_bound(
        &self,
        q: &mut std::collections::VecDeque<strata_transport::sender::OutputPacket>,
    ) {
        let cap_bytes = self.paced_queue_cap_bytes();

        let mut total: usize = q.iter().map(|p| p.data.len()).sum();
        if total <= cap_bytes {
            return;
        }

        // Drop the oldest non-keyframe packet repeatedly. Scan from the
        // front (oldest) for the first droppable (priority < Reference);
        // keyframes/config survive until they are all that is left.
        let mut dropped_pkts = 0u64;
        let mut dropped_bytes = 0u64;
        let mut dropped_retx = 0u64;
        while total > cap_bytes {
            let drop_idx = q
                .iter()
                .position(|p| p.priority < Priority::Reference)
                .or(if q.is_empty() { None } else { Some(0) });
            match drop_idx {
                Some(idx) => {
                    if let Some(pkt) = q.remove(idx) {
                        total -= pkt.data.len();
                        dropped_pkts += 1;
                        dropped_bytes += pkt.data.len() as u64;
                        if pkt.is_retransmit {
                            dropped_retx += 1;
                        }
                    } else {
                        break;
                    }
                }
                None => break,
            }
        }
        if dropped_pkts > 0 {
            let pkts_total = self
                .aqm_dropped_pkts
                .fetch_add(dropped_pkts, Ordering::Relaxed)
                + dropped_pkts;
            self.aqm_dropped_bytes
                .fetch_add(dropped_bytes, Ordering::Relaxed);
            let retx_total = self
                .aqm_dropped_retx
                .fetch_add(dropped_retx, Ordering::Relaxed)
                + dropped_retx;
            let mut last = self.aqm_last_log.lock().unwrap();
            if last.elapsed() >= std::time::Duration::from_secs(1) {
                *last = Instant::now();
                tracing::warn!(
                    link_id = self.id,
                    cap_bytes,
                    queue_bytes = total,
                    dropped_now = dropped_pkts,
                    dropped_total = pkts_total,
                    retx_dropped_total = retx_total,
                    "paced-queue AQM dropped packets (each is a self-inflicted hole)"
                );
            }
        }
    }

    /// Periodically size `SO_SNDBUF` toward the BDP (floored at one GSO
    /// superpacket). Shrinking the kernel buffer converts silent kernel
    /// absorption of an over-send into explicit `EAGAIN` backpressure that
    /// the userspace pacer/AQM acts on, instead of letting the datagram
    /// vanish into a deep socket buffer and bloat RTT. Throttled so we
    /// don't `setsockopt` on every flush; only resized on a material change.
    #[cfg(unix)]
    fn maybe_resize_sndbuf(&self) {
        use std::os::unix::io::AsRawFd;

        let bdp_cap = {
            let cc = self.congestion.lock().unwrap();
            cc.inflight_cap_bytes(BDP_QUEUE_K)
        };
        if bdp_cap <= 0.0 {
            return; // BDP unknown — keep the generous bootstrap buffer.
        }
        let target = (bdp_cap as usize).max(GSO_SUPERPACKET_BYTES);

        let mut guard = self.sndbuf_state.lock().unwrap();
        let (last_at, last_target) = *guard;
        let now = std::time::Instant::now();
        // Resize at most ~1/sec and only when the target moved ≥25%.
        if now.duration_since(last_at) < std::time::Duration::from_secs(1) {
            return;
        }
        let changed = last_target == 0
            || (target as f64 - last_target as f64).abs() / (last_target as f64) >= 0.25;
        if !changed {
            return;
        }
        let fd = self.socket.as_raw_fd();
        let buf_size: libc::c_int = target as libc::c_int;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &buf_size as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        *guard = (now, target);
        tracing::debug!(
            target: "strata::transport",
            link_id = self.id,
            sndbuf_target = target,
            "resized SO_SNDBUF toward BDP (explicit backpressure)"
        );
    }

    #[cfg(not(unix))]
    fn maybe_resize_sndbuf(&self) {}
    /// Create a new transport link.
    ///
    /// `socket` should be bound and connected to the remote peer.
    pub fn new(id: usize, socket: UdpSocket, config: SenderConfig, iface: Option<String>) -> Self {
        let peer_addr = socket
            .peer_addr()
            .expect("socket must be connected before creating TransportLink");
        // Non-blocking so recv_feedback() can poll without stalling the worker.
        socket.set_nonblocking(true).ok();
        // Increase kernel send buffer to absorb initial encoder burst before
        // BBR pacing kicks in. Default ~212KB is too small for HD video
        // keyframes; 512KB prevents EAGAIN storms at startup.
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = socket.as_raw_fd();
            let buf_size: libc::c_int = 524_288; // 512 KB
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
        let udp_state = UdpSocketState::new(UdpSockRef::from(&socket))
            .expect("failed to initialize quinn-udp socket state");
        TransportLink {
            id,
            sender: Mutex::new(Sender::new(config)),
            rtt: Mutex::new(RttTracker::new()),
            clock: Mutex::new(TimestampClock::new()),
            congestion: Mutex::new(BiscayController::new()),
            socket,
            udp_state,
            peer_addr,
            bytes_sent: AtomicU64::new(0),
            packets_sent: AtomicU64::new(0),
            prev_rate_bytes: AtomicU64::new(0),
            prev_rate_time_us: AtomicU64::new(0),
            rate_ewma_bps: Mutex::new(0.0),
            goodput_ewma_bps: Mutex::new(0.0),
            bytes_acked: AtomicU64::new(0),
            prev_ack_time_us: AtomicU64::new(0),
            prev_ack_bytes: AtomicU64::new(0),
            ack_rate_ewma_bps: Mutex::new(0.0),
            per_link_ack_rate_bps: Mutex::new(0.0),
            prev_pkts_acked: AtomicU64::new(0),
            prev_pkts_acked_us: AtomicU64::new(0),
            ack_rate_was_zeroed: AtomicBool::new(false),
            receiver_report: Mutex::new(None),
            last_recv_bytes_delivered: AtomicU64::new(0),
            last_recv_report_at: Mutex::new(Instant::now()),
            iface,
            pacing: Mutex::new(PacingState {
                tokens: 10_000.0, // Bootstrap burst — enough for initial probes
                last_refill: std::time::Instant::now(),
            }),
            paced_queue: Mutex::new(std::collections::VecDeque::new()),
            paced_queue_last_empty_us: AtomicU64::new(0),
            aqm_dropped_pkts: AtomicU64::new(0),
            aqm_dropped_bytes: AtomicU64::new(0),
            aqm_dropped_retx: AtomicU64::new(0),
            aqm_last_log: Mutex::new(Instant::now()),
            oracle: Mutex::new(CapacityOracle::new()),
            prev_retransmissions: AtomicU64::new(0),
            prev_loss_pkts_sent: AtomicU64::new(0),
            probe_feedback_block: Mutex::new(ProbeFeedbackBlock::Clear),
            prev_acked_liveness: AtomicU64::new(0),
            last_ack_or_report: Mutex::new(Instant::now()),
            was_delivery_starved: std::sync::atomic::AtomicBool::new(false),
            sndbuf_state: Mutex::new((std::time::Instant::now(), 0)),
        }
    }

    /// Send data through the transport layer (encode → wire → socket).
    ///
    /// Uses GSO batching when outputs have uniform segment size.
    fn transport_send(&self, data: &[u8], priority: Priority) -> Result<usize> {
        let mut sender = self.sender.lock().unwrap();
        sender.send(Bytes::copy_from_slice(data), priority);
        let outputs: Vec<_> = sender.drain_output().collect();

        let mut q = self.paced_queue.lock().unwrap();
        q.extend(outputs);
        // BDP-relative, keyframe-protected bound (F2/F4): scale-free so the
        // same code is correct on fiber, cellular and satellite.
        self.enforce_paced_queue_bound(&mut q);
        drop(q);

        self.flush_paced();

        // Return data.len() to pretend we sent it all (or the actual bytes sent?)
        // The trait expects the number of bytes accepted.
        Ok(data.len())
    }

    /// Flush any pending packets in the paced send queue.
    pub fn flush_paced(&self) {
        // Keep the kernel send buffer sized to the BDP so an over-send
        // surfaces as EAGAIN backpressure rather than silent bloat.
        self.maybe_resize_sndbuf();
        let cc_pacing_rate = self.congestion.lock().unwrap().pacing_rate();
        // Floor: don't let the CC starve a link below 20% of the oracle's
        // slow-decaying peak estimate. Using peak_cap() (not estimated_cap())
        // prevents a death spiral where oracle collapse → pacing collapse →
        // less delivery → further oracle collapse.
        let peak_cap_bytes = self.oracle.lock().unwrap().peak_cap() / 8.0;
        let floor_rate = peak_cap_bytes * PACING_FLOOR_VS_PEAK;
        let base_rate = cc_pacing_rate.max(floor_rate);

        // RTT-aware bufferbloat throttle.
        //
        // When the smoothed RTT rises significantly above the link's minimum
        // RTT, the forwarding queue is filling up — we are pushing faster than
        // the link can drain.  Packet loss always starts AFTER the queue
        // overflows, so waiting for loss to signal congestion is too late:
        // by that point we have already tipped the link into collapse.
        //
        // Policy:
        //   srtt / min_rtt ≤ 1.5  →  throttle = 1.0  (no change; normal jitter)
        //   srtt / min_rtt  = 2.0  →  throttle = 0.75
        //   srtt / min_rtt  = 3.0  →  throttle = 0.5
        //   srtt / min_rtt ≥ 6.0  →  throttle = 0.25 (clamped floor)
        //
        // The floor of 0.25 prevents a feedback loop where throttle collapses
        // a link faster than the queue can drain.  Throttle applies AFTER the
        // CC floor because the floor exists to prevent CC-driven starvation,
        // while THIS mechanism deliberately starves a bloated link to recover.
        //
        // Acknowledged, not yet consolidated: `cc_pacing_rate` above already
        // folds in congestion.rs's own `drain_factor`, which likewise responds
        // to RTT rising above baseline. So a single bufferbloat event can be
        // multiplied down by both reducers in the same tick (e.g. 0.5 ×
        // 0.25 = 8x). Each is independently justified (different files,
        // different baselines: `drain_factor` reacts to gradient/regime
        // signals in congestion.rs, this throttle reacts to the srtt/min_rtt
        // ratio here) and the ordering is deliberate (CC floor first, then
        // this throttle), but the double-count itself is not — a candidate
        // consolidation, not implemented here.
        let rtt_throttle = {
            let rtt = self.rtt.lock().unwrap();
            rtt_bufferbloat_throttle(rtt.srtt_us(), rtt.min_rtt_us())
        };
        let pacing_rate = base_rate * rtt_throttle;
        let mut p = self.pacing.lock().unwrap();
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(p.last_refill).as_secs_f64();
        p.tokens += pacing_rate * elapsed;
        // Burst cap: 10 ms of data (or 10 KB minimum for startup)
        p.tokens = p
            .tokens
            .min((pacing_rate * TOKEN_BUCKET_BURST_SECS).max(TOKEN_BUCKET_MIN_BURST_BYTES));
        p.last_refill = now;

        let mut q = self.paced_queue.lock().unwrap();
        if q.is_empty() {
            // App-limited marker: the wire is not being kept full right now,
            // so ACK-rate samples covering this instant measure the app's
            // send rate, not link capacity. The pacer runs every few ms, so
            // this early return observes any mid-interval queue drain.
            self.paced_queue_last_empty_us
                .store(mono_now_us(), Ordering::Relaxed);
            return;
        }

        let mut to_send = Vec::new();
        while let Some(pkt) = q.front() {
            let len = pkt.data.len() as f64;
            // Check-then-subtract: admit whenever the balance is still
            // non-negative, THEN deduct this packet's bytes. That ordering
            // is what lets the balance go negative by up to one packet
            // (~1 MTU) rather than a separate "OR" condition — it's an
            // emergent burst-debt allowance, not two rules.
            if p.tokens >= 0.0 {
                p.tokens -= len;
                to_send.push(q.pop_front().unwrap());
            } else {
                break;
            }
        }
        drop(q);
        drop(p);

        if !to_send.is_empty() {
            let (total_bytes, pkts_sent) = self.send_batch(&to_send);

            if pkts_sent < to_send.len() {
                let mut q = self.paced_queue.lock().unwrap();
                let mut p = self.pacing.lock().unwrap();
                // Refund tokens and push back in REVERSE order to maintain sequence
                for pkt in to_send.into_iter().skip(pkts_sent).rev() {
                    p.tokens += pkt.data.len() as f64;
                    q.push_front(pkt);
                }
            }

            self.bytes_sent
                .fetch_add(total_bytes as u64, Ordering::Relaxed);
            self.packets_sent
                .fetch_add(pkts_sent as u64, Ordering::Relaxed);
        }
    }

    /// Batch-send outputs via quinn-udp with GSO when possible.
    /// Returns `(bytes_sent, packets_sent)`.
    fn send_batch(&self, outputs: &[strata_transport::sender::OutputPacket]) -> (usize, usize) {
        if outputs.is_empty() {
            return (0, 0);
        }

        let mut max_gso = self.udp_state.max_gso_segments();

        // Cap GSO batching in calibration mode to reduce burstiness
        {
            let cc = self.congestion.lock().unwrap();
            if matches!(cc.bbr_phase, BbrPhase::SlowStart) {
                max_gso = max_gso.min(4); // Small batches during probe
            }
        }

        let mut total_bytes = 0;
        let mut pkts_sent = 0;

        if max_gso > 1 {
            // Try GSO: group consecutive same-size outputs into batches
            let mut i = 0;
            while i < outputs.len() {
                let seg_len = outputs[i].data.len();
                let mut end = i + 1;

                // Collect consecutive outputs with the same length (up to max_gso)
                while end < outputs.len()
                    && outputs[end].data.len() == seg_len
                    && (end - i) < max_gso
                {
                    end += 1;
                }

                if end - i > 1 {
                    #[cfg(feature = "bursty_diag")]
                    tracing::info!(
                        target: "strata::bursty_diag",
                        link_id = self.id,
                        batch_size = end - i,
                        "GSO batch"
                    );

                    // GSO batch: concatenate into a single buffer
                    let mut buf = Vec::with_capacity(seg_len * (end - i));
                    for output in &outputs[i..end] {
                        buf.extend_from_slice(&output.data);
                    }

                    let transmit = Transmit {
                        destination: self.peer_addr,
                        ecn: None,
                        contents: &buf,
                        segment_size: Some(seg_len),
                        src_ip: None,
                    };

                    match self
                        .udp_state
                        .send(UdpSockRef::from(&self.socket), &transmit)
                    {
                        Ok(()) => {
                            total_bytes += buf.len();
                            pkts_sent += end - i;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(link_id = self.id, error = %e, "GSO send failed, falling back");
                            // Fallback: send individually
                            for output in &outputs[i..end] {
                                match self.send_single(&output.data) {
                                    Ok(len) => {
                                        total_bytes += len;
                                        pkts_sent += 1;
                                    }
                                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                                        return (total_bytes, pkts_sent);
                                    }
                                    Err(_) => {
                                        pkts_sent += 1; // Count as processed to avoid retry loops on permanent errors
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Single packet — no GSO needed
                    match self.send_single(&outputs[i].data) {
                        Ok(len) => {
                            total_bytes += len;
                            pkts_sent += 1;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            break;
                        }
                        Err(_) => {
                            pkts_sent += 1;
                        }
                    }
                }
                i = end;
            }
        } else {
            // No GSO support — send individually
            for output in outputs {
                match self.send_single(&output.data) {
                    Ok(len) => {
                        total_bytes += len;
                        pkts_sent += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        break;
                    }
                    Err(_) => {
                        pkts_sent += 1;
                    }
                }
            }
        }

        (total_bytes, pkts_sent)
    }

    /// Send a single datagram via quinn-udp.
    fn send_single(&self, data: &[u8]) -> std::io::Result<usize> {
        let transmit = Transmit {
            destination: self.peer_addr,
            ecn: None,
            contents: data,
            segment_size: None,
            src_ip: None,
        };

        match self
            .udp_state
            .send(UdpSockRef::from(&self.socket), &transmit)
        {
            Ok(()) => Ok(data.len()),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    tracing::warn!(link_id = self.id, error = %e, "send failed");
                }
                Err(e)
            }
        }
    }

    /// Process an incoming ACK/NACK packet from the receiver.
    pub fn process_feedback(&self, data: &[u8]) -> Result<()> {
        use strata_transport::wire::{ControlBody, Packet, PacketType};

        let mut cursor = data;
        let packet = Packet::decode(&mut cursor)
            .ok_or_else(|| anyhow::anyhow!("failed to decode feedback packet"))?;

        if packet.header.packet_type != PacketType::Control {
            return Ok(());
        }

        let mut payload_cursor = &packet.payload[..];
        if let Some(ctrl) = ControlBody::decode(&mut payload_cursor) {
            let mut sender = self.sender.lock().unwrap();
            match &ctrl {
                ControlBody::Ack(ack) => {
                    let _newly_acked = sender.process_ack(ack);

                    // ── Delivery rate measurement ──────────────────────
                    // Use the receiver's total_received counter — a smooth,
                    // monotonically-increasing count of unique data packets.
                    // This avoids dependency on the sender's packet pool
                    // capacity (which can fill up, causing newly_acked to
                    // drop to zero and halt BW measurement entirely).
                    let total_recv = ack.total_received.value();
                    let avg_payload = 1200u64;

                    if total_recv > 0 {
                        let total_recv_bytes = total_recv * avg_payload;
                        let prev_bytes_val = self.bytes_acked.load(Ordering::Relaxed);
                        if total_recv_bytes > prev_bytes_val {
                            self.bytes_acked.store(total_recv_bytes, Ordering::Relaxed);
                        }
                    }

                    let now_us = mono_now_us();
                    let total_acked = self.bytes_acked.load(Ordering::Relaxed);
                    let prev_bytes = self.prev_ack_bytes.load(Ordering::Relaxed);
                    let prev_us = self.prev_ack_time_us.load(Ordering::Relaxed);
                    let interval_us = now_us.saturating_sub(prev_us);

                    let srtt_us = {
                        let rtt = self.rtt.lock().unwrap();
                        rtt.srtt_us()
                    };
                    let min_interval_us =
                        (srtt_us as u64).clamp(ACK_RATE_MIN_INTERVAL_US, ACK_RATE_MAX_INTERVAL_US);

                    if interval_us >= min_interval_us && prev_us > 0 {
                        let delta_bytes = total_acked.saturating_sub(prev_bytes);
                        if delta_bytes > 0 {
                            // Idle-gap detection: if the interval is much
                            // larger than expected (> 4×SRTT), the link was
                            // idle between scheduler bursts.  Reset the baseline
                            // without computing a rate — the interval includes
                            // idle time which would dilute the measurement and
                            // underestimate the link's actual delivery rate.
                            let max_interval_us = (srtt_us as u64 * ACK_RATE_IDLE_GAP_SRTT_MULT)
                                .clamp(ACK_RATE_IDLE_GAP_MIN_US, ACK_RATE_IDLE_GAP_MAX_US);
                            if interval_us > max_interval_us {
                                self.prev_ack_bytes.store(total_acked, Ordering::Relaxed);
                                self.prev_ack_time_us.store(now_us, Ordering::Relaxed);
                            } else {
                                self.prev_ack_bytes.store(total_acked, Ordering::Relaxed);
                                self.prev_ack_time_us.store(now_us, Ordering::Relaxed);

                                let ack_rate_bps =
                                    (delta_bytes as f64 * 8.0) / (interval_us as f64 / 1_000_000.0);
                                let mut ewma = self.ack_rate_ewma_bps.lock().unwrap();
                                if *ewma == 0.0 {
                                    *ewma = ack_rate_bps;
                                } else {
                                    *ewma = ACK_RATE_EWMA_ALPHA * ack_rate_bps
                                        + (1.0 - ACK_RATE_EWMA_ALPHA) * *ewma;
                                }

                                let mut cc = self.congestion.lock().unwrap();

                                #[cfg(feature = "bursty_diag")]
                                tracing::info!(
                                    target: "strata::bursty_diag",
                                    link_id = self.id,
                                    interval_us = interval_us,
                                    delta_bytes = delta_bytes,
                                    "ACK sample"
                                );

                                // App-limited iff the paced queue was observed
                                // empty inside this sample's interval: the wire
                                // went unfilled, so the delivery rate measures
                                // the app's send rate, not capacity. Hardcoding
                                // `false` here (2026-07-11 field stream) let
                                // btl_bw converge onto our own throttled send
                                // rate — pacing ≈ inflow, so every IDR burst
                                // stood in the paced queue (sojourn 1-2 s, 29%
                                // of packets late, continuous AQM shed) and the
                                // capacity chain tracked inflow instead of the
                                // link, floor-locking the adapter. This does
                                // not resurrect the old ratchet-by-survivor-
                                // bias: samples taken under genuine backlog are
                                // NOT app-limited and still pull btl_bw down
                                // when capacity truly drops.
                                let last_empty_us =
                                    self.paced_queue_last_empty_us.load(Ordering::Relaxed);
                                let app_limited = last_empty_us >= prev_us;
                                cc.on_bandwidth_sample(delta_bytes, interval_us, app_limited);
                            }
                        }
                    } else if prev_us == 0 {
                        self.prev_ack_bytes.store(total_acked, Ordering::Relaxed);
                        self.prev_ack_time_us.store(now_us, Ordering::Relaxed);
                    }
                }
                ControlBody::Nack(nack) => {
                    // Retransmit admission control: when the paced queue is
                    // already past half its budget, requeueing repairs only
                    // multiplies the offered load — the AQM trims them, the
                    // receiver re-NACKs, and the loop amplifies a radio
                    // stall into a sustained storm (field run 12: 273k of
                    // 396k AQM drops were retransmissions, ~5× the fresh
                    // traffic). Skipping here does NOT consume the sender
                    // retry budget; the receiver re-asks after its rearm
                    // interval, by which time the queue has drained if the
                    // stall has passed.
                    let q_bytes: usize = {
                        let q = self.paced_queue.lock().unwrap();
                        q.iter().map(|p| p.data.len()).sum()
                    };
                    if q_bytes * 2 > self.paced_queue_cap_bytes() {
                        tracing::debug!(
                            link_id = self.id,
                            q_bytes,
                            "NACK deferred: paced queue above half budget"
                        );
                    } else {
                        sender.process_nack(nack);
                        // Drain retransmits into paced queue so they actually
                        // get sent. Without this, retransmits pile up in the
                        // sender's internal output_queue and inflate
                        // queue_depth, keeping the BDP cap permanently blocked.
                        let outputs: Vec<_> = sender.drain_output().collect();
                        if !outputs.is_empty() {
                            let mut q = self.paced_queue.lock().unwrap();
                            q.extend(outputs);
                            // Same BDP-relative, keyframe-protected bound.
                            self.enforce_paced_queue_bound(&mut q);
                        }
                    }
                }
                ControlBody::Pong(pong) => {
                    let mut rtt = self.rtt.lock().unwrap();
                    rtt.handle_pong(pong);
                    // Feed RTT sample to BiscayController
                    let rtt_us = rtt.srtt_us();
                    if rtt_us > 0.0 {
                        let mut cc = self.congestion.lock().unwrap();
                        cc.on_rtt_sample(rtt_us);
                    }
                }
                ControlBody::ReceiverReport(report) => {
                    *self.receiver_report.lock().unwrap() = Some(report.clone());
                    // Record the receiver-side delivered byte counter and the
                    // local timestamp at which we observed it. The saturation
                    // probe driver uses these to compute receiver-observed
                    // throughput across the probe window.
                    self.last_recv_bytes_delivered
                        .store(report.bytes_delivered, Ordering::Relaxed);
                    let report_now = Instant::now();
                    *self.last_recv_report_at.lock().unwrap() = report_now;
                    // A receiver report is positive proof the path delivers,
                    // even if ACKs are sparse — refresh the blackhole timer.
                    *self.last_ack_or_report.lock().unwrap() = report_now;
                    // F3: feed the receiver-measured relative-OWD gradient
                    // into Biscay. This is the primary, path-relative
                    // delay-pressure signal and demotes *this* link only.
                    self.congestion
                        .lock()
                        .unwrap()
                        .on_delay_gradient_us(report.delay_gradient_us);
                    let mut ewma = self.goodput_ewma_bps.lock().unwrap();
                    let goodput = report.goodput_bps as f64;
                    if *ewma == 0.0 {
                        *ewma = goodput;
                    } else {
                        *ewma = GOODPUT_REPORT_EWMA_ALPHA * goodput
                            + (1.0 - GOODPUT_REPORT_EWMA_ALPHA) * *ewma;
                    }
                    tracing::debug!(target: "strata::transport", link_id = self.id, goodput = goodput, ewma = *ewma, bytes_delivered = report.bytes_delivered, "Received ReceiverReport");
                }
                ControlBody::PpdReport(ppd) => {
                    let capacity_bps = ppd.capacity_bps as f64;
                    self.oracle
                        .lock()
                        .unwrap()
                        .observe_packet_pair(capacity_bps);
                    tracing::debug!(
                        target: "strata::transport",
                        link_id = self.id,
                        capacity_bps = ppd.capacity_bps,
                        dispersion_us = ppd.dispersion_us,
                        packet_size = ppd.packet_size,
                        "PPD report received"
                    );
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Flush any pending FEC repair packets.
    pub fn flush_fec(&self) -> Result<usize> {
        let mut sender = self.sender.lock().unwrap();
        sender.flush_fec();

        let outputs: Vec<_> = sender.drain_output().collect();

        let mut q = self.paced_queue.lock().unwrap();
        q.extend(outputs);
        self.enforce_paced_queue_bound(&mut q);
        drop(q);

        self.flush_paced();

        Ok(0)
    }

    /// Get the current RTT estimate in milliseconds.
    pub fn rtt_ms(&self) -> f64 {
        let rtt = self.rtt.lock().unwrap();
        rtt.srtt_us() / 1000.0
    }

    /// Get the latest receiver report, if any.
    pub fn latest_receiver_report(&self) -> Option<ReceiverReportPacket> {
        self.receiver_report.lock().unwrap().clone()
    }
}

impl LinkSender for TransportLink {
    fn id(&self) -> usize {
        self.id
    }

    fn send(&self, packet: &[u8]) -> Result<usize> {
        self.transport_send(packet, Priority::Standard)
    }

    fn send_prioritized(&self, packet: &[u8], priority: Priority) -> Result<usize> {
        self.transport_send(packet, priority)
    }

    fn get_metrics(&self) -> LinkMetrics {
        // ── Snapshot sender state and release lock immediately ──────────
        // Holding the sender lock during oracle/CC computations blocks
        // process_ack() in the feedback recv loop, causing ACK batching
        // that inflates delivery rate measurements.
        let (stats, sender_queue_depth) = {
            let sender = self.sender.lock().unwrap();
            let s = sender.stats().clone();
            let q = sender.output_queue_len();
            (s, q)
        };

        let rtt_ms = {
            let rtt = self.rtt.lock().unwrap();
            rtt.srtt_us() / 1000.0
        };

        // --- Socket-level rate (includes FEC/retransmit overhead) ---
        let total_bytes = self.bytes_sent.load(Ordering::Relaxed);
        let now_us = {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let epoch = EPOCH.get_or_init(std::time::Instant::now);
            epoch.elapsed().as_micros() as u64
        };
        let prev_bytes = self.prev_rate_bytes.swap(total_bytes, Ordering::Relaxed);
        let prev_us = self.prev_rate_time_us.swap(now_us, Ordering::Relaxed);
        let dt_s = now_us.saturating_sub(prev_us) as f64 / 1_000_000.0;

        let socket_rate_bps = if dt_s >= 0.01 {
            let delta = total_bytes.saturating_sub(prev_bytes);
            (delta as f64 * 8.0) / dt_s
        } else {
            0.0
        };

        let mut ewma = self.rate_ewma_bps.lock().unwrap();
        if socket_rate_bps > 0.0 {
            if *ewma == 0.0 {
                *ewma = socket_rate_bps;
            } else {
                *ewma = 0.2 * socket_rate_bps + 0.8 * *ewma;
            }
        } else if *ewma > 0.0 {
            // Gentle decay when no data flows — slow enough to survive
            // normal scheduler idle gaps (multi-link round-robin) but will
            // eventually reach 0 if a link is truly idle for seconds.
            *ewma *= 0.98;
            if *ewma < 1000.0 {
                *ewma = 0.0;
            }
        }
        let observed_bps = *ewma;

        tracing::debug!(
            target: "strata::transport",
            link_id = self.id,
            socket_rate_bps = socket_rate_bps,
            ewma_bps = observed_bps,
            dt_s = dt_s,
            delta_bytes = total_bytes.saturating_sub(prev_bytes),
            total_bytes_sent = total_bytes,
            "get_metrics: observed rate"
        );

        // --- Capacity estimate: Oracle (primary) + BBR btl_bw (fallback) ---
        //
        // The CapacityOracle provides a stable, feedback-loop-free capacity
        // estimate for EDPF scheduling. btl_bw is only used as a fallback
        // before the first saturation probe completes.
        let mut cc = self.congestion.lock().unwrap();
        // Drive ProbeRtt phase transitions — without this, RTprop never
        // re-probes and BBR capacity estimates go stale on cellular links.
        cc.tick();
        let phase = match cc.state {
            BiscayState::PreHandover => LinkPhase::Degrade,
            BiscayState::Cautious => LinkPhase::Degrade,
            BiscayState::Normal => match cc.bbr_phase {
                BbrPhase::SlowStart => LinkPhase::Probe,
                BbrPhase::ProbeBw => LinkPhase::Live,
                // ProbeRtt is a short control cycle for RTprop refresh, not
                // a capacity-discovery startup phase. Mapping it to Probe
                // would trigger scheduler capacity-floor overrides and flatten
                // per-link estimates toward the floor.
                BbrPhase::ProbeRtt => LinkPhase::Live,
            },
        };
        let btl_bw_bps = cc.btl_bw() * 8.0;

        // Feed ACK-confirmed delivery rate to the Oracle (passive lower-bound).
        // Use per-link bytes_acked (actual payload sizes summed during ACK
        // processing) — NOT packets_acked * estimated_size, which is inaccurate
        // due to FEC packets and varying payload sizes.
        let per_link_ack_bytes = stats.bytes_acked;
        let now_ack_us = {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let epoch = EPOCH.get_or_init(std::time::Instant::now);
            epoch.elapsed().as_micros() as u64
        };
        // Read without swapping — only commit the snapshot when the interval
        // condition is met. This lets the interval accumulate across multiple
        // get_metrics() calls until ≥ 500ms has passed.
        let prev_bytes = self.prev_pkts_acked.load(Ordering::Relaxed);
        let prev_us = self.prev_pkts_acked_us.load(Ordering::Relaxed);
        let interval_us = now_ack_us.saturating_sub(prev_us);

        // Require ≥ 500ms between rate samples to avoid spikes from batch
        // ACKs. This gives a smoother, more accurate delivery rate.
        let per_link_ack_rate = if interval_us >= PER_LINK_ACK_RATE_MIN_INTERVAL_US && prev_us > 0 {
            // Commit the snapshot for next interval
            self.prev_pkts_acked
                .store(per_link_ack_bytes, Ordering::Relaxed);
            self.prev_pkts_acked_us.store(now_ack_us, Ordering::Relaxed);

            let delta = per_link_ack_bytes.saturating_sub(prev_bytes);
            let mut ewma = self.per_link_ack_rate_bps.lock().unwrap();
            if delta > 0 {
                let rate = (delta as f64 * 8.0) / (interval_us as f64 / 1_000_000.0);
                if *ewma == 0.0 {
                    // Re-acquiring delivery from a zeroed rate. If the rate
                    // was zeroed by a *stall* (not a cold start), this first
                    // non-zero interval is a stall-release ACK burst: the
                    // cumulative ACKs for thousands of packets sent over the
                    // whole stall are flushed at once and compressed into one
                    // ~500 ms window, over-stating the true rate by 100×+
                    // (field: link 0 → 43.8 Mbps vs btl_bw 1.26 Mbps). The
                    // snapshot is already committed above, so discard this
                    // one sample and let the *next* clean interval seed the
                    // rate. A genuine cold start (no preceding stall) seeds
                    // immediately as before.
                    if self.ack_rate_was_zeroed.swap(false, Ordering::Relaxed) {
                        // leave *ewma == 0.0 — burst sample dropped
                    } else {
                        *ewma = rate;
                    }
                } else {
                    *ewma = 0.2 * rate + 0.8 * *ewma;
                }
            } else {
                // No new ACKs in this 500ms window — decay the EWMA.
                // Without this, the rate stays frozen at a peak value
                // indefinitely, inflating the oracle's capacity estimate.
                *ewma *= 0.5;
                if *ewma < 1000.0 {
                    *ewma = 0.0;
                    // Mark that the rate was zeroed by a delivery stall so
                    // the next non-zero interval is treated as a contaminated
                    // stall-release burst, not a real capacity sample.
                    self.ack_rate_was_zeroed.store(true, Ordering::Relaxed);
                }
            }
            *ewma
        } else if prev_us == 0 {
            // First call — seed the baseline
            self.prev_pkts_acked
                .store(per_link_ack_bytes, Ordering::Relaxed);
            self.prev_pkts_acked_us.store(now_ack_us, Ordering::Relaxed);
            0.0
        } else {
            *self.per_link_ack_rate_bps.lock().unwrap()
        };

        let mut oracle = self.oracle.lock().unwrap();

        // Use the best available delivery rate signal for the oracle.
        // per_link_ack_rate underreports when the sender pool overflows
        // (entries expire before ACKs arrive). The receiver-reported goodput
        // is more accurate since it measures actual delivered data.
        let goodput = *self.goodput_ewma_bps.lock().unwrap();
        let delivery_signal = if goodput > MEANINGFUL_BASELINE_BPS {
            goodput
        } else {
            per_link_ack_rate
        };

        if delivery_signal > 0.0 {
            oracle.observe_delivery(delivery_signal);
        }
        // Update baseline RTT for downshift detection
        oracle.update_baseline_rtt(rtt_ms);
        // Apply time-based confidence decay
        oracle.tick();

        // Check for downshift conditions (handover/severe degradation)
        // Uses cumulative retransmit_ratio — correct for detecting handovers.
        let cumulative_loss = if stats.packets_acked > 0 && stats.packets_sent > 0 {
            stats.retransmit_ratio()
        } else {
            0.0
        };
        if oracle.should_reset(rtt_ms, cumulative_loss) {
            oracle.reset_on_downshift();
        }

        // Per-interval loss_rate for the scheduling/adaptation layer.
        // Cumulative retransmit_ratio is permanently inflated by startup
        // burst losses — same bug we fixed receiver-side for loss_after_fec.
        let prev_retx = self.prev_retransmissions.load(Ordering::Relaxed);
        let prev_sent = self.prev_loss_pkts_sent.load(Ordering::Relaxed);
        let delta_retx = stats.retransmissions.saturating_sub(prev_retx);
        let delta_sent = stats.packets_sent.saturating_sub(prev_sent);
        self.prev_retransmissions
            .store(stats.retransmissions, Ordering::Relaxed);
        self.prev_loss_pkts_sent
            .store(stats.packets_sent, Ordering::Relaxed);
        // Denominator is total on-the-wire traffic this window (originals + retries).
        // Using only `delta_sent` (originals) lets retry bursts from older windows
        // push the ratio past 1.0 and clamp to 1.0, producing phantom 100%-loss ticks
        // and phantom link-death WARNs even when the link is healthy.
        let total_wire = delta_sent.saturating_add(delta_retx).max(1);
        let loss_rate = (delta_retx as f64 / total_wire as f64).clamp(0.0, 1.0);
        // Feed the loss EWMA used purely to classify the regime for metrics
        // (loss already drives backoff through the CC itself).
        cc.observe_loss_rate(loss_rate);

        // ── Continuous liveness model (no binary death) ─────────────────
        //
        // A configured, OS-up link is ALWAYS `alive`. Cellular links take
        // transient loss bursts, HARQ stalls and handovers constantly;
        // binary-killing one on a blip — and then never re-admitting it
        // because exclusion starves the very traffic needed to prove
        // recovery — permanently destroys half the bond from one hiccup.
        //
        // Instead, degradation is purely continuous:
        //   * Ordinary loss → already discounted by EDPF's `(1-loss)`
        //     per-link capacity factor (`capacity_bytes_per_sec`).
        //   * A link *currently* delivering nothing (sent meaningful
        //     traffic but zero ACK progress / receiver reports for
        //     `STARVED_STALE`) has its reported `capacity_bps` crushed to
        //     `STARVED_CAPACITY_FLOOR_BPS` (applied at capacity
        //     finalisation below). EDPF then trickles it instead of
        //     dumping the stream into it.
        //
        // `delivery_starved` is recomputed every call from a non-latching
        // timer: the instant one ACK or receiver report arrives,
        // `last_ack_or_report` refreshes and the next tick restores full
        // capacity. The link never leaves the bond, so the existing
        // saturation-probe rotation keeps re-testing it and it re-admits
        // itself automatically. There is no sticky "dead" state.
        let now = Instant::now();
        let acked = stats.packets_acked;
        let prev_acked = self.prev_acked_liveness.swap(acked, Ordering::Relaxed);
        if acked > prev_acked {
            *self.last_ack_or_report.lock().unwrap() = now;
        }
        let last_proof = *self.last_ack_or_report.lock().unwrap();
        let delivery_starved = stats.packets_sent >= STARVED_MIN_SENT
            && now.duration_since(last_proof) >= STARVED_STALE;
        // Observability only — NOT a death. Log the starved↔recovered
        // transitions only: this runs every metrics tick (~10 Hz) and a
        // blackholed link would otherwise spam hundreds of thousands of
        // identical WARN lines per hour.
        let was_starved = self
            .was_delivery_starved
            .swap(delivery_starved, Ordering::Relaxed);
        if delivery_starved && !was_starved {
            tracing::warn!(
                link_id = self.id,
                packets_sent = stats.packets_sent,
                packets_acked = acked,
                stale_ms = now.duration_since(last_proof).as_millis() as u64,
                "link delivery-starved: crushing to probe trickle (NOT dead — \
                 auto-recovers on next ACK/report)"
            );
        } else if !delivery_starved && was_starved {
            tracing::info!(
                link_id = self.id,
                "link delivery recovered: restoring full capacity weight"
            );
        }
        // The link itself never self-reports dead; OS-down is handled
        // separately by the `os_up` field.
        let alive = true;

        // Capacity: prefer Oracle → BBR btl_bw → ack_delivery_bps fallback.
        // When Oracle and BBR are both stale (no real bandwidth samples),
        // use ack_delivery_bps as a direct proxy for achievable throughput.
        let oracle_cap = oracle.estimated_cap();
        let btl_bw_capped = if btl_bw_bps > 0.0 {
            let capped = if per_link_ack_rate > MEANINGFUL_BASELINE_BPS {
                btl_bw_bps.min(per_link_ack_rate * BTLBW_VS_ACK_RATE_CAP_MULT)
            } else {
                btl_bw_bps
            };
            capped.clamp(MEANINGFUL_BASELINE_BPS, BTLBW_ABSOLUTE_CEILING_BPS)
        } else {
            0.0
        };
        let capacity_bps = if oracle_cap > 0.0 {
            // Defense-in-depth (same invariant as the probe-poisoning fix):
            // BBR's `btl_bw` is the physically-grounded bottleneck estimate.
            // A passive oracle estimate that exceeds it by ≥4× cannot be a
            // real capacity ceiling — it is a contaminated passive sample
            // (stall-release ACK burst, ACK batching artifact). Never let it
            // override the physical estimate; fall back to btl_bw.
            if btl_bw_capped > 0.0 && oracle_cap > btl_bw_capped * ORACLE_SANE_BTLBW_MULT {
                tracing::warn!(
                    link_id = self.id,
                    oracle_cap_kbps = (oracle_cap / 1000.0) as u64,
                    btl_bw_kbps = (btl_bw_capped / 1000.0) as u64,
                    "oracle cap >{}× btl_bw — contaminated passive sample, \
                     using btl_bw",
                    ORACLE_SANE_BTLBW_MULT as u64,
                );
                btl_bw_capped
            } else {
                oracle_cap
            }
        } else if btl_bw_capped > 0.0 {
            btl_bw_capped
        } else if per_link_ack_rate > MEANINGFUL_BASELINE_BPS {
            // Fallback: use ack delivery rate with headroom as capacity.
            // This prevents the adapter from using the 5 Mbps floor when
            // actual achievable throughput is known from ACK measurements.
            per_link_ack_rate * ACK_RATE_FALLBACK_HEADROOM_MULT
        } else {
            0.0
        };
        drop(oracle);

        // Continuous demotion (replaces binary death): a link that is
        // currently delivering nothing is pinned to a trickle floor so EDPF
        // deprioritises it instead of dumping the stream into a hole. It
        // stays in the bond; the probe rotation re-tests it and the floor
        // lifts automatically on the next tick after an ACK/report.
        //
        // Two tiers — a *transient* starve (had delivered before, currently
        // stale) keeps the 64 kbps trickle so a recovering link earns share
        // back quickly; a *hard blackhole* (sent ≥STARVED_MIN_SENT packets
        // and **zero** acked, ever) is pinned far lower so EDPF gives it
        // essentially no media while the periodic saturation probe still
        // re-tests the path. This stops the bond repeatedly dealing the
        // encoder onto a link that has never once delivered a packet
        // (field run #5: link 0, 22k sent / 0 acked, still scheduled).
        let capacity_bps = if delivery_starved {
            let floor = if acked == 0 {
                STARVED_HARD_BLACKHOLE_FLOOR_BPS
            } else {
                STARVED_CAPACITY_FLOOR_BPS
            };
            capacity_bps.min(floor)
        } else {
            capacity_bps
        };

        let btlbw_bps = if btl_bw_bps > 0.0 {
            Some(btl_bw_bps)
        } else {
            None
        };

        let rtprop_ms = if cc.rt_prop_us() < f64::MAX {
            Some(cc.rt_prop_us() / 1000.0)
        } else {
            None
        };

        // F6 observability: surface the inferred regime and the actual
        // BDP-relative cap the link is enforcing so the system explains its
        // own decisions in production.
        let inferred_regime = Some(cc.inferred_regime().as_str().to_string());
        let bdp_bytes = cc.bdp_bytes();
        let inflight_cap_bytes = {
            let bdp_cap = cc.inflight_cap_bytes(BDP_QUEUE_K);
            if bdp_cap > 0.0 {
                (bdp_cap as usize).max(GSO_SUPERPACKET_BYTES) as f64
            } else {
                PACED_QUEUE_BOOTSTRAP_BYTES as f64
            }
        };

        tracing::debug!(
            target: "strata::transport",
            link_id = self.id,
            capacity_bps = capacity_bps,
            btl_bw_bps = btl_bw_bps,
            observed_bps = observed_bps,
            pacing_rate_bps = cc.pacing_rate() * 8.0,
            phase = ?phase,
            rtt_ms = rtt_ms,
            rtprop_ms = rtprop_ms,
            loss_rate = loss_rate,
            drain_factor = cc.drain_factor(),
            pkts_sent = stats.packets_sent,
            pkts_acked = stats.packets_acked,
            retransmissions = stats.retransmissions,
            fec_repairs_sent = stats.fec_repairs_sent,
            "get_metrics: full snapshot"
        );

        LinkMetrics {
            rtt_ms,
            capacity_bps,
            loss_rate,
            observed_bps,
            observed_bytes: total_bytes,
            queue_depth: sender_queue_depth + self.paced_queue.lock().unwrap().len(),
            max_queue: 0,
            alive,
            phase,
            os_up: Some(true),
            mtu: None,
            iface: self.iface.clone(),
            link_kind: Some("strata-transport".into()),
            btlbw_bps,
            rtprop_ms,
            transport: Some(crate::net::interface::TransportMetrics {
                packets_sent: stats.packets_sent,
                packets_acked: stats.packets_acked,
                retransmissions: stats.retransmissions,
                fec_repairs_sent: stats.fec_repairs_sent,
                packets_expired: stats.packets_expired,
            }),
            ack_delivery_bps: per_link_ack_rate,
            ack_bytes: per_link_ack_bytes,
            estimated_capacity_bps: capacity_bps,
            owd_ms: rtt_ms / 2.0,
            receiver_report: self.latest_receiver_report().map(|r| {
                crate::net::interface::ReceiverReportMetrics {
                    goodput_bps: r.goodput_bps,
                    fec_repair_rate: r.fec_repair_rate_f32(),
                    jitter_buffer_ms: r.jitter_buffer_ms,
                    loss_after_fec: r.loss_after_fec_f32(),
                    late_rate: r.late_rate_f32(),
                    delay_gradient_us: r.delay_gradient_us,
                }
            }),
            probe_active: self
                .probe_feedback_block
                .lock()
                .unwrap()
                .is_blocked(Instant::now()),
            inferred_regime,
            bdp_bytes,
            inflight_cap_bytes,
            pacing_rate_bps: cc.pacing_rate() * 8.0,
            aqm_dropped_total: self.aqm_dropped_pkts.load(Ordering::Relaxed),
        }
    }

    fn on_rf_metrics(&self, rf: &crate::modem::health::RfMetrics) {
        let radio = strata_transport::congestion::RadioMetrics {
            rsrp_dbm: rf.rsrp_dbm,
            rsrq_db: rf.rsrq_db,
            sinr_db: rf.sinr_db,
            cqi: rf.cqi,
            timestamp: Some(quanta::Instant::now()),
        };
        self.congestion.lock().unwrap().on_radio_metrics(&radio);
    }

    fn set_probe_allowed(&self, allowed: bool) {
        self.congestion.lock().unwrap().set_probe_allowed(allowed);
    }

    fn complete_saturation_probe(&self, peak_bps: f64) {
        self.oracle.lock().unwrap().complete_probe(peak_bps);
        // Seed the congestion controller so btl_bw reflects the probe-measured
        // physical capacity, not just the delivery rate under scheduler allocation.
        self.congestion
            .lock()
            .unwrap()
            .seed_bandwidth(peak_bps / 8.0);
    }

    fn inject_ppd_pair(&self) {
        let pair = {
            let mut sender = self.sender.lock().unwrap();
            sender.inject_ppd_pair(1200) // use default MTU size
        };
        // Send directly, bypassing pacing, to keep packets back-to-back
        if !pair.is_empty() {
            let (total_bytes, pkts_sent) = self.send_batch(&pair);
            self.bytes_sent
                .fetch_add(total_bytes as u64, Ordering::Relaxed);
            self.packets_sent
                .fetch_add(pkts_sent as u64, Ordering::Relaxed);
        }
    }

    fn set_saturation_probe_active(&self, active: bool) {
        self.oracle.lock().unwrap().set_probe_active(active);
        let mut block = self.probe_feedback_block.lock().unwrap();
        *block = if active {
            // Block unconditionally while the probe runs; the cooldown
            // deadline is set only once it ends, below.
            ProbeFeedbackBlock::ProbeRunning
        } else {
            ProbeFeedbackBlock::Cooldown(Instant::now() + PROBE_FEEDBACK_COOLDOWN)
        };
    }

    fn set_failover_broadcast_active(&self, active: bool) {
        self.oracle.lock().unwrap().set_broadcast_active(active);
    }

    fn recv_bytes_delivered(&self) -> u64 {
        self.last_recv_bytes_delivered.load(Ordering::Relaxed)
    }

    fn recv_report_at(&self) -> Option<Instant> {
        Some(*self.last_recv_report_at.lock().unwrap())
    }

    fn set_fec_overhead(&self, ratio: f64) {
        // Keep the generation size (K) fixed and vary the repair count (R)
        // so `overhead ≈ R / K`. A fixed K keeps generation latency and
        // decode cost predictable; only the protection strength adapts.
        // R is clamped to [1, K]: at least one repair (FEC stays enabled,
        // per the requirement) and never more repairs than sources.
        const FEC_BASE_K: usize = 32;
        let r = ((FEC_BASE_K as f64) * ratio).round() as usize;
        let mut r = r.clamp(1, FEC_BASE_K);

        // Diagnostic isolation lever (default OFF): `STRATA_FEC=off` forces
        // zero repair symbols so a field run can prove whether FEC repair
        // is the source of "clean stats, corrupt video". Not a normal mode
        // — the default keeps FEC enabled (R ≥ 1).
        static FEC_DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let disabled = *FEC_DISABLED.get_or_init(|| {
            std::env::var("STRATA_FEC")
                .map(|v| v.eq_ignore_ascii_case("off") || v == "0")
                .unwrap_or(false)
        });
        if disabled {
            r = 0;
        }

        self.sender.lock().unwrap().set_fec_rate(FEC_BASE_K, r);
    }

    fn set_profile(&self, regime: Option<&str>) {
        let parsed = regime.and_then(strata_transport::congestion::PathRegime::parse_override);
        self.congestion.lock().unwrap().set_profile_override(parsed);
    }

    fn on_modem_flow_control(&self, slow_down: bool) {
        self.congestion
            .lock()
            .unwrap()
            .on_modem_flow_control(slow_down);
    }

    fn queue_building(&self) -> bool {
        self.congestion.lock().unwrap().queue_building()
    }

    fn recv_feedback(&self) -> usize {
        let mut processed = 0;
        let mut buf = [0u8; 2048];

        // Drain all pending feedback datagrams (non-blocking).
        loop {
            match self.socket.recv(&mut buf) {
                Ok(n) if n > 0 => {
                    if self.process_feedback(&buf[..n]).is_ok() {
                        processed += 1;
                    }
                }
                _ => break,
            }
        }

        #[cfg(feature = "bursty_diag")]
        if processed > 0 {
            tracing::info!(
                target: "strata::bursty_diag",
                link_id = self.id,
                queue_depth = processed,
                "Feedback drained"
            );
        }

        // Send periodic Pings for RTT measurement.
        let mut rtt = self.rtt.lock().unwrap();
        if rtt.needs_ping() {
            let ts = self.clock.lock().unwrap().now_us();
            let ping = rtt.make_ping(ts);
            let mut body = BytesMut::with_capacity(16);
            ping.encode(&mut body);
            let body_bytes = body.freeze();
            let header = PacketHeader::control(0, ts, body_bytes.len() as u16);
            let pkt = Packet {
                header,
                payload: body_bytes,
            };
            let encoded = pkt.encode();
            let _ = self.socket.send(&encoded);
        }

        processed
    }

    fn flush_paced(&self) {
        self.flush_paced();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    fn make_loopback_link(id: usize) -> TransportLink {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        socket.connect(addr).unwrap();
        TransportLink::new(id, socket, SenderConfig::default(), None)
    }

    #[test]
    fn link_reports_id() {
        let link = make_loopback_link(42);
        assert_eq!(link.id(), 42);
    }

    #[test]
    fn link_send_and_receive() {
        let link = make_loopback_link(0);
        let data = b"hello transport";
        let result = link.send(data);
        assert!(result.is_ok());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn metrics_after_send() {
        let link = make_loopback_link(1);
        link.send(b"test packet").unwrap();
        let metrics = link.get_metrics();
        assert!(metrics.observed_bytes > 0);
    }

    #[test]
    fn flush_fec_succeeds() {
        let link = make_loopback_link(2);
        for i in 0..10 {
            let data = format!("packet {i}");
            link.send(data.as_bytes()).unwrap();
        }
        let result = link.flush_fec();
        assert!(result.is_ok());
    }

    #[test]
    fn delivery_starved_link_is_crushed_but_stays_alive_and_recovers() {
        // Loopback socket with no feedback loop ⇒ packets_acked stays 0 and
        // no receiver report ever arrives: a perfect delivery starvation.
        let link = make_loopback_link(7);
        for i in 0..60 {
            link.send(format!("p{i}").as_bytes()).unwrap();
        }

        // Within the startup grace window: full participation.
        let m = link.get_metrics();
        assert!(m.alive, "link must never self-report dead");

        // Backdate the liveness timer past STARVED_STALE.
        *link.last_ack_or_report.lock().unwrap() =
            std::time::Instant::now() - (STARVED_STALE + std::time::Duration::from_secs(1));

        let m = link.get_metrics();
        assert!(
            m.alive,
            "a starved link must STILL be alive — no binary death; it is \
             only demoted so the probe rotation can re-admit it"
        );
        assert!(
            m.capacity_bps <= STARVED_CAPACITY_FLOOR_BPS,
            "starved link capacity must be crushed to the trickle floor, got {}",
            m.capacity_bps
        );

        // A single fresh ACK/report timestamp restores full capacity on the
        // very next tick — no sticky dead state.
        *link.last_ack_or_report.lock().unwrap() = std::time::Instant::now();
        let m = link.get_metrics();
        assert!(m.alive);
        assert!(
            m.capacity_bps > STARVED_CAPACITY_FLOOR_BPS || m.capacity_bps == 0.0, // 0.0 = no capacity estimate yet (loopback), still un-crushed
            "capacity must lift off the floor immediately once delivery \
             resumes, got {}",
            m.capacity_bps
        );
    }

    #[test]
    fn low_volume_link_not_falsely_starved() {
        // Below STARVED_MIN_SENT the link is exempt even if stale — there
        // simply hasn't been enough traffic to conclude it is starved.
        let link = make_loopback_link(8);
        link.send(b"hello").unwrap();
        *link.last_ack_or_report.lock().unwrap() =
            std::time::Instant::now() - (STARVED_STALE + std::time::Duration::from_secs(5));
        let m = link.get_metrics();
        assert!(m.alive, "links never self-report dead");
        assert!(
            m.capacity_bps == 0.0 || m.capacity_bps > STARVED_CAPACITY_FLOOR_BPS,
            "a barely-used link must not be crushed as starved, got {}",
            m.capacity_bps
        );
    }

    #[test]
    fn hard_blackhole_link_demoted_below_transient_starve_floor() {
        // Loopback ⇒ packets_acked stays 0 forever: a *hard* blackhole, not
        // a transient dip. It must be pinned to the much lower
        // hard-blackhole floor (≪ the 64 kbps transient-starve trickle) so
        // EDPF gives it essentially no media, while still staying alive for
        // the saturation-probe self-readmission path. Regression for field
        // run #5: link 0 sent 22k packets / 0 acked yet kept real share.
        let link = make_loopback_link(21);
        for i in 0..60 {
            link.send(format!("p{i}").as_bytes()).unwrap();
        }
        *link.last_ack_or_report.lock().unwrap() =
            std::time::Instant::now() - (STARVED_STALE + std::time::Duration::from_secs(1));
        let m = link.get_metrics();
        assert!(m.alive, "hard-blackholed link must still be alive");
        assert!(
            m.capacity_bps <= STARVED_HARD_BLACKHOLE_FLOOR_BPS,
            "a never-acked link must be pinned to the hard-blackhole floor \
             ({STARVED_HARD_BLACKHOLE_FLOOR_BPS}), not the transient trickle \
             ({STARVED_CAPACITY_FLOOR_BPS}); got {}",
            m.capacity_bps
        );
    }

    #[test]
    fn initial_metrics_are_sane() {
        let link = make_loopback_link(3);
        let metrics = link.get_metrics();
        assert_eq!(metrics.observed_bytes, 0);
        assert_eq!(metrics.phase, LinkPhase::Probe);
        // F6: regime is observable, unknown before any measurement.
        assert_eq!(metrics.inferred_regime.as_deref(), Some("unknown"));
        assert_eq!(metrics.bdp_bytes, 0.0);
        // Before BDP converges the bound is the bootstrap budget.
        assert_eq!(
            metrics.inflight_cap_bytes,
            PACED_QUEUE_BOOTSTRAP_BYTES as f64
        );
    }

    #[test]
    fn paced_queue_bound_drops_oldest_nonkeyframe_first() {
        use strata_transport::sender::OutputPacket;
        let link = make_loopback_link(11);

        let mk = |seq: u64, prio: Priority| OutputPacket {
            data: Bytes::from(vec![0u8; 1400]),
            priority: prio,
            sequence: seq,
            is_retransmit: false,
            is_fec_repair: false,
        };

        let mut q = std::collections::VecDeque::new();
        // Oldest packet is a keyframe; then many standard packets that
        // together blow past the bootstrap budget (140 KB ≈ 100 pkts).
        q.push_back(mk(0, Priority::Reference));
        for s in 1..400 {
            q.push_back(mk(s, Priority::Standard));
        }
        let before = q.len();
        link.enforce_paced_queue_bound(&mut q);

        let total: usize = q.iter().map(|p| p.data.len()).sum();
        assert!(
            total <= PACED_QUEUE_BOOTSTRAP_BYTES,
            "queue must be bounded to the BDP budget, got {total} bytes"
        );
        assert!(q.len() < before, "packets should have been dropped");
        // The keyframe (oldest, priority ≥ Reference) must survive while
        // standard packets are evicted.
        assert!(
            q.iter().any(|p| p.sequence == 0),
            "keyframe must be protected from the oldest-drop"
        );
    }

    /// On a short-RTT path the BDP collapses to bytes (loopback: ~60 B) and
    /// the old pure-BDP cap fell to the 64 KiB GSO floor — smaller than one
    /// IDR burst, so every keyframe burst was trimmed into mid-GOP holes
    /// (measured 2.3% self-inflicted loss over loopback). The drain-time
    /// bound must let a burst that clears within the sojourn budget survive.
    #[test]
    fn paced_queue_transient_burst_survives_short_rtt_path() {
        use strata_transport::sender::OutputPacket;
        let link = make_loopback_link(14);
        {
            let mut cc = link.congestion.lock().unwrap();
            cc.on_rtt_sample(200.0); // RTprop 0.2 ms — loopback-like
            cc.on_bandwidth_sample(300_000, 1_000_000, false); // ~2.4 Mbps
        }
        // ~90 KB burst (one IDR at a few hundred kbit): over the 64 KiB GSO
        // floor, but drains in ~0.3 s at pacing rate — inside the budget.
        let mut q = std::collections::VecDeque::new();
        for s in 0..64u64 {
            q.push_back(OutputPacket {
                data: Bytes::from(vec![0u8; 1400]),
                priority: Priority::Standard,
                sequence: s,
                is_retransmit: false,
                is_fec_repair: false,
            });
        }
        link.enforce_paced_queue_bound(&mut q);
        assert_eq!(
            q.len(),
            64,
            "a transient burst inside the drain-time budget must not be cut"
        );
    }

    #[test]
    fn paced_queue_bound_noop_when_under_budget() {
        use strata_transport::sender::OutputPacket;
        let link = make_loopback_link(12);
        let mut q = std::collections::VecDeque::new();
        for s in 0..10 {
            q.push_back(OutputPacket {
                data: Bytes::from(vec![0u8; 1400]),
                priority: Priority::Standard,
                sequence: s,
                is_retransmit: false,
                is_fec_repair: false,
            });
        }
        link.enforce_paced_queue_bound(&mut q);
        assert_eq!(q.len(), 10, "small queue must be left intact");
    }

    #[test]
    fn set_profile_overrides_inferred_regime() {
        let link = make_loopback_link(13);
        link.set_profile(Some("satellite"));
        assert_eq!(
            link.get_metrics().inferred_regime.as_deref(),
            Some("satellite")
        );
        link.set_profile(Some("auto"));
        assert_eq!(
            link.get_metrics().inferred_regime.as_deref(),
            Some("unknown"),
            "auto restores measurement-based inference"
        );
    }

    /// N9 regression: the probe-feedback block state must be an explicit,
    /// directly-inspectable variant at every stage, not something inferred
    /// by comparing an `Instant` against a far-future sentinel.
    #[test]
    fn probe_feedback_block_state_is_explicit_not_inferred() {
        let link = make_loopback_link(15);

        assert_eq!(
            *link.probe_feedback_block.lock().unwrap(),
            ProbeFeedbackBlock::Clear,
            "no probe has run yet"
        );
        assert!(!link.get_metrics().probe_active);

        link.set_saturation_probe_active(true);
        assert_eq!(
            *link.probe_feedback_block.lock().unwrap(),
            ProbeFeedbackBlock::ProbeRunning,
            "an active probe must be its own explicit state, not a deadline \
             far enough in the future to look permanent"
        );
        assert!(link.get_metrics().probe_active);

        link.set_saturation_probe_active(false);
        let now = Instant::now();
        match *link.probe_feedback_block.lock().unwrap() {
            ProbeFeedbackBlock::Cooldown(deadline) => {
                let remaining = deadline.saturating_duration_since(now);
                assert!(
                    remaining <= PROBE_FEEDBACK_COOLDOWN,
                    "cooldown deadline must be bounded by PROBE_FEEDBACK_COOLDOWN \
                     (~1.5s), not the old far-future (150s) sentinel; got \
                     {remaining:?} remaining"
                );
            }
            other => panic!("expected Cooldown state after probe ends, got {other:?}"),
        }
        assert!(link.get_metrics().probe_active, "still within cooldown");
    }

    #[test]
    fn rtt_throttle_passes_through_when_rtt_unknown() {
        // Cold start: no samples → min_rtt is f64::MAX, srtt is 0.
        assert_eq!(rtt_bufferbloat_throttle(0.0, f64::MAX), 1.0);
        assert_eq!(rtt_bufferbloat_throttle(1_000.0, f64::MAX), 1.0);
        assert_eq!(rtt_bufferbloat_throttle(0.0, 80_000.0), 1.0);
    }

    #[test]
    fn rtt_throttle_no_effect_at_baseline() {
        // At baseline RTT, no throttle.
        assert_eq!(rtt_bufferbloat_throttle(80_000.0, 80_000.0), 1.0);
        // Within normal jitter zone (up to 1.5×), no throttle.
        assert_eq!(rtt_bufferbloat_throttle(120_000.0, 80_000.0), 1.0);
    }

    #[test]
    fn rtt_throttle_engages_on_queue_buildup() {
        // srtt = 2× baseline → throttle = 1.5/2.0 = 0.75.
        let t = rtt_bufferbloat_throttle(160_000.0, 80_000.0);
        assert!((t - 0.75).abs() < 1e-9, "expected 0.75 got {t}");

        // srtt = 3× baseline → throttle = 0.5.
        let t = rtt_bufferbloat_throttle(240_000.0, 80_000.0);
        assert!((t - 0.5).abs() < 1e-9, "expected 0.5 got {t}");

        // srtt = 4× baseline (field-test scenario: 364ms vs 87ms baseline)
        // → 1.5/4 = 0.375, safely inside the [0.25, 1.0] range.
        let t = rtt_bufferbloat_throttle(360_000.0, 90_000.0);
        assert!((t - 0.375).abs() < 1e-9, "expected 0.375 got {t}");
    }

    #[test]
    fn rtt_throttle_clamps_at_floor() {
        // Severe bloat: srtt = 10× baseline. Raw formula → 0.15, clamped 0.25.
        let t = rtt_bufferbloat_throttle(800_000.0, 80_000.0);
        assert!((t - 0.25).abs() < 1e-9, "expected 0.25 (clamped) got {t}");
    }

    #[test]
    fn rtt_throttle_rejects_nonsense_inputs() {
        // Negative srtt (should never happen but guard anyway).
        assert_eq!(rtt_bufferbloat_throttle(-100.0, 80_000.0), 1.0);
        // Zero min_rtt would cause div-by-zero.
        assert_eq!(rtt_bufferbloat_throttle(100_000.0, 0.0), 1.0);
        // NaN/Inf should not crash.
        assert_eq!(rtt_bufferbloat_throttle(f64::NAN, 80_000.0), 1.0);
        assert_eq!(rtt_bufferbloat_throttle(80_000.0, f64::INFINITY), 1.0);
    }
}
