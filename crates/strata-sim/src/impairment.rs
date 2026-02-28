use crate::topology::Namespace;
use std::io;

/// Gilbert-Elliott (4-state) loss model parameters for `tc netem`.
#[derive(Debug, Clone, Default)]
pub struct GemodelConfig {
    pub p: f32,     // Probability Good -> Bad (%)
    pub r: f32,     // Probability Bad -> Good (%)
    pub one_h: f32, // Loss probability in Good state (%)
    pub one_k: f32, // Loss probability in Bad state (%)
}

/// Network impairment parameters applied via `tc netem`.
///
/// All fields are optional — only non-`None` parameters are passed to netem.
/// If all fields are `None`, any existing qdisc is removed (clearing impairments).
///
/// Use the provided presets ([`ImpairmentConfig::lte_urban`], etc.) as starting
/// points for realistic cellular simulation.  They include correlated loss,
/// normal-distributed jitter, packet corruption/reorder, and queue limits
/// calibrated to prevent unrealistic bufferbloat.
#[derive(Debug, Clone, Default)]
pub struct ImpairmentConfig {
    pub delay_ms: Option<u32>,
    pub jitter_ms: Option<u32>,
    /// Use normal distribution for jitter (requires kernel `normal` table).
    /// When `true`, `delay Xms Yms distribution normal` is emitted.
    /// Default `false` uses uniform jitter.
    pub delay_distribution_normal: bool,
    pub loss_percent: Option<f32>,
    pub loss_correlation: Option<f32>,
    pub gemodel: Option<GemodelConfig>, // If set, overrides loss_percent
    pub rate_kbit: Option<u64>,
    pub duplicate_percent: Option<f32>,
    pub reorder_percent: Option<f32>,
    pub reorder_correlation: Option<f32>,
    pub corrupt_percent: Option<f32>,
    /// Max packets in the netem queue.  Kernel default is 1000, which creates
    /// unrealistic multi-second buffers on slow links.  When `None` and
    /// `rate_kbit` is set, [`apply_impairment`] auto-computes a limit
    /// targeting ~100ms of buffering at the configured rate.
    pub limit: Option<u32>,

    /// LTE/5G TTI radio slot scheduling (`tc netem slot`).
    ///
    /// Models the radio scheduler's burst-then-silence rhythm: packets are
    /// held in the netem queue and released only at TTI slot boundaries.
    /// Both `slot_min_us` and `slot_max_us` must be `Some` for the `slot`
    /// directive to be emitted.  `slot_max_packets` and `slot_max_bytes`
    /// bound the per-slot release budget.
    ///
    /// LTE TTI = 1 ms → `slot_min_us: Some(1_000), slot_max_us: Some(2_000)`
    /// 5G NR slot ≈ 0.5 ms → `slot_min_us: Some(500), slot_max_us: Some(1_000)`
    pub slot_min_us: Option<u32>,
    pub slot_max_us: Option<u32>,
    pub slot_max_packets: Option<u32>,
    pub slot_max_bytes: Option<u32>,

    /// Modem firmware buffer size in kibibytes (`tc tbf burst`).
    ///
    /// When set together with `rate_kbit`, a `tbf` root qdisc is installed
    /// first (`handle 1:`).  The `tbf` owns rate shaping and models the
    /// fill-and-drain behaviour of a USB modem's internal Qualcomm buffer
    /// (~64 KiB typical).  Netem becomes the tbf child at `parent 1:1
    /// handle 10:` and handles delay, loss, and slot scheduling — but no
    /// longer emits a `rate` argument (that belongs to tbf).
    pub modem_buffer_kb: Option<u32>,
}

/// Assumed MTU for queue-depth calculations (bytes).
const ASSUMED_MTU: u64 = 1200;

/// Target queue depth in seconds for auto-computed `limit`.
///
/// Real cellular eNodeB / modem TX buffers hold 500ms–2s of data.
/// Using 100ms caused artificially shallow queues that triggered CC
/// drain reactions (RTT inflation from tiny buffer overflow) that
/// would never occur with real modem hardware.
const QUEUE_TARGET_SECS: f64 = 0.500;

impl ImpairmentConfig {
    /// Compute a reasonable netem `limit` (in packets) for the configured rate,
    /// targeting [`QUEUE_TARGET_SECS`] of buffering.  Returns `None` if
    /// `rate_kbit` is not set.
    pub fn auto_limit(&self) -> Option<u32> {
        self.rate_kbit.map(|rate| {
            let bytes_per_sec = rate as f64 * 1000.0 / 8.0;
            let pkts = (bytes_per_sec * QUEUE_TARGET_SECS / ASSUMED_MTU as f64).ceil() as u32;
            pkts.max(10) // floor — allow at least a small burst
        })
    }

    /// Derive a return-path (ACK direction) config from this forward-path config.
    ///
    /// The return path gets the same delay/jitter/loss characteristics but
    /// **no rate limit** — ACKs are small and the bottleneck capacity only
    /// constrains the data direction.  This produces realistic symmetric
    /// RTT measurements and ACK-path loss.
    pub fn return_path_config(&self) -> Self {
        Self {
            delay_ms: self.delay_ms,
            jitter_ms: self.jitter_ms,
            delay_distribution_normal: self.delay_distribution_normal,
            loss_percent: self.loss_percent,
            loss_correlation: self.loss_correlation,
            // No rate limit on the return path
            rate_kbit: None,
            // No corruption/reorder/dup — these are data-path phenomena
            corrupt_percent: None,
            duplicate_percent: None,
            reorder_percent: None,
            reorder_correlation: None,
            gemodel: None,
            // Generous limit — ACKs are small, just need room for delay
            limit: Some(1000),
            // Slot scheduling and modem buffer are data-path-only phenomena
            slot_min_us: None,
            slot_max_us: None,
            slot_max_packets: None,
            slot_max_bytes: None,
            modem_buffer_kb: None,
        }
    }

    // ── Realistic cellular presets ───────────────────────────────────
    //
    // Delay values represent one-way (per-hop) delay.  When used with
    // [`apply_bidirectional_impairment`], both egress directions are
    // shaped, producing a full RTT of 2× the configured delay value.

    /// Average urban LTE uplink.
    ///
    /// * 8 Mbps rate (good signal, dedicated SIM)
    /// * 22 ms one-way delay (45 ms RTT)
    /// * ±8 ms jitter, normal distribution
    /// * 0.5% loss with 25% burst correlation
    /// * 0.05% corruption, 0.1% reorder
    pub fn lte_urban() -> Self {
        Self {
            rate_kbit: Some(8_000),
            delay_ms: Some(22),
            jitter_ms: Some(8),
            delay_distribution_normal: true,
            loss_percent: Some(0.5),
            loss_correlation: Some(25.0),
            corrupt_percent: Some(0.05),
            reorder_percent: Some(0.1),
            reorder_correlation: Some(20.0),
            slot_min_us: Some(1_000),
            slot_max_us: Some(2_000),
            slot_max_packets: Some(12),
            slot_max_bytes: Some(14_400),
            modem_buffer_kb: Some(64),
            ..Default::default()
        }
    }

    /// Poor / congested LTE uplink.
    ///
    /// * 5 Mbps rate
    /// * 30 ms one-way delay (60 ms RTT)
    /// * ±15 ms jitter, normal distribution
    /// * 2.0% loss with 30% burst correlation
    /// * 0.1% corruption, 0.3% reorder
    pub fn lte_poor() -> Self {
        Self {
            rate_kbit: Some(5_000),
            delay_ms: Some(30),
            jitter_ms: Some(15),
            delay_distribution_normal: true,
            loss_percent: Some(2.0),
            loss_correlation: Some(30.0),
            corrupt_percent: Some(0.1),
            reorder_percent: Some(0.3),
            reorder_correlation: Some(25.0),
            slot_min_us: Some(1_000),
            slot_max_us: Some(5_000),
            slot_max_packets: Some(8),
            slot_max_bytes: Some(9_600),
            modem_buffer_kb: Some(64),
            ..Default::default()
        }
    }

    /// Good-signal LTE uplink (rural/suburban, low contention).
    ///
    /// * 6 Mbps rate
    /// * 18 ms one-way delay (35 ms RTT)
    /// * ±5 ms jitter, normal distribution
    /// * 0.3% loss with 15% correlation
    pub fn lte_good() -> Self {
        Self {
            rate_kbit: Some(6_000),
            delay_ms: Some(18),
            jitter_ms: Some(5),
            delay_distribution_normal: true,
            loss_percent: Some(0.3),
            loss_correlation: Some(15.0),
            slot_min_us: Some(1_000),
            slot_max_us: Some(2_000),
            slot_max_packets: Some(14),
            slot_max_bytes: Some(16_800),
            modem_buffer_kb: Some(64),
            ..Default::default()
        }
    }

    /// 5G NSA uplink with good signal.
    ///
    /// * 40 Mbps rate
    /// * 12 ms one-way delay (25 ms RTT)
    /// * ±4 ms jitter, normal distribution
    /// * 0.1% loss with 10% correlation
    pub fn fiveg_good() -> Self {
        Self {
            rate_kbit: Some(40_000),
            delay_ms: Some(12),
            jitter_ms: Some(4),
            delay_distribution_normal: true,
            loss_percent: Some(0.1),
            loss_correlation: Some(10.0),
            slot_min_us: Some(500),
            slot_max_us: Some(1_000),
            slot_max_packets: Some(40),
            slot_max_bytes: Some(48_000),
            modem_buffer_kb: Some(128),
            ..Default::default()
        }
    }

    /// Idealised low-impairment link for unit/integration tests where
    /// you want to isolate transport logic without cellular noise.
    /// Still rate-limited but no loss, corruption, or reorder.
    pub fn ideal(rate_kbit: u64, delay_ms: u32) -> Self {
        Self {
            rate_kbit: Some(rate_kbit),
            delay_ms: Some(delay_ms),
            loss_percent: Some(0.0),
            ..Default::default()
        }
    }
}

/// Applies network impairment to an interface inside a namespace using `tc netem`.
///
/// Removes any existing root qdisc first, then configures the specified
/// delay, loss, rate-limit, duplication, reorder, and corruption parameters.
pub fn apply_impairment(
    ns: &Namespace,
    interface: &str,
    config: ImpairmentConfig,
) -> io::Result<()> {
    // 1. Remove existing qdisc (best effort) to ensure clean state or update
    // command: tc qdisc del dev <interface> root
    // We ignore errors here because the qdisc might not exist.
    let _ = ns.exec("tc", &["qdisc", "del", "dev", interface, "root"]);

    // If no config intended (clearing impairments), we are done.
    if config.delay_ms.is_none()
        && config.loss_percent.is_none()
        && config.rate_kbit.is_none()
        && config.gemodel.is_none()
        && config.duplicate_percent.is_none()
        && config.reorder_percent.is_none()
        && config.corrupt_percent.is_none()
    {
        return Ok(());
    }

    // 2. Optionally install a tbf root qdisc (modem firmware buffer model).
    //
    // When `modem_buffer_kb` and `rate_kbit` are both set, a tbf root qdisc
    // is installed first.  It owns rate shaping and models the fill-and-drain
    // behaviour of a USB modem's internal Qualcomm buffer (~64 KiB typical).
    // Netem then becomes its child at `parent 1:1 handle 10:` and handles
    // delay, loss, and slot scheduling — but does NOT emit a `rate` argument
    // (tbf already owns that).
    let use_tbf = config.modem_buffer_kb.is_some() && config.rate_kbit.is_some();

    if use_tbf {
        let rate = config.rate_kbit.unwrap();
        let burst_bytes = config.modem_buffer_kb.unwrap() as u64 * 1024;
        let tbf_args: Vec<String> = vec![
            "qdisc".to_string(),
            "add".to_string(),
            "dev".to_string(),
            interface.to_string(),
            "root".to_string(),
            "handle".to_string(),
            "1:".to_string(),
            "tbf".to_string(),
            "rate".to_string(),
            format!("{}kbit", rate),
            "burst".to_string(),
            format!("{}", burst_bytes),
            "latency".to_string(),
            "300ms".to_string(),
        ];
        let tbf_ref: Vec<&str> = tbf_args.iter().map(|s| s.as_str()).collect();
        let out = ns.exec("tc", &tbf_ref)?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "Failed to apply tc tbf: {}\nCommand: tc {}",
                String::from_utf8_lossy(&out.stderr),
                tbf_args.join(" ")
            )));
        }
    }

    // 3. Build netem arguments.
    //    When tbf is the root, netem is its child (parent 1:1 handle 10:).
    //    When tbf is absent, netem is the root qdisc.
    let mut args_storage: Vec<String> = vec![
        "qdisc".to_string(),
        "add".to_string(),
        "dev".to_string(),
        interface.to_string(),
    ];

    if use_tbf {
        args_storage.extend_from_slice(&[
            "parent".to_string(),
            "1:1".to_string(),
            "handle".to_string(),
            "10:".to_string(),
        ]);
    } else {
        args_storage.push("root".to_string());
    }
    args_storage.push("netem".to_string());

    // Queue limit: use explicit value, or auto-compute from rate.
    let effective_limit = config.limit.or_else(|| config.auto_limit());
    if let Some(limit) = effective_limit {
        args_storage.push("limit".to_string());
        args_storage.push(limit.to_string());
    }

    if let Some(delay) = config.delay_ms {
        args_storage.push("delay".to_string());
        args_storage.push(format!("{}ms", delay));

        if let Some(jitter) = config.jitter_ms
            && jitter > 0
        {
            args_storage.push(format!("{}ms", jitter));
            if config.delay_distribution_normal {
                args_storage.push("distribution".to_string());
                args_storage.push("normal".to_string());
            }
        }
    }

    if let Some(gemodel) = &config.gemodel {
        args_storage.push("loss".to_string());
        args_storage.push("gemodel".to_string());
        args_storage.push(format!("{}%", gemodel.p));
        args_storage.push(format!("{}%", gemodel.r));
        args_storage.push(format!("{}%", gemodel.one_h));
        args_storage.push(format!("{}%", gemodel.one_k));
    } else if let Some(loss) = config.loss_percent {
        args_storage.push("loss".to_string());
        args_storage.push(format!("{}%", loss));
        if let Some(corr) = config.loss_correlation {
            args_storage.push(format!("{}%", corr));
        }
    }

    if let Some(dup) = config.duplicate_percent {
        args_storage.push("duplicate".to_string());
        args_storage.push(format!("{}%", dup));
    }

    if let Some(reorder) = config.reorder_percent {
        args_storage.push("reorder".to_string());
        args_storage.push(format!("{}%", reorder));
        if let Some(corr) = config.reorder_correlation {
            args_storage.push(format!("{}%", corr));
        }
    }

    if let Some(corrupt) = config.corrupt_percent {
        args_storage.push("corrupt".to_string());
        args_storage.push(format!("{}%", corrupt));
    }

    // Rate: only when tbf is NOT handling it.
    if !use_tbf
        && let Some(rate) = config.rate_kbit
    {
        args_storage.push("rate".to_string());
        args_storage.push(format!("{}kbit", rate));
    }

    // TTI slot scheduling: models LTE/5G radio scheduler burst-then-silence.
    // Requires both slot_min_us and slot_max_us to be set.
    if let (Some(slot_min), Some(slot_max)) = (config.slot_min_us, config.slot_max_us) {
        args_storage.push("slot".to_string());
        args_storage.push(format!("{}us", slot_min));
        args_storage.push(format!("{}us", slot_max));
        if let Some(pkts) = config.slot_max_packets {
            args_storage.push("packets".to_string());
            args_storage.push(pkts.to_string());
        }
        if let Some(bytes) = config.slot_max_bytes {
            args_storage.push("bytes".to_string());
            args_storage.push(bytes.to_string());
        }
    }

    let args: Vec<&str> = args_storage.iter().map(|s| s.as_str()).collect();

    let output = ns.exec("tc", &args)?;

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Failed to apply tc netem: {}\nCommand: tc {}",
            String::from_utf8_lossy(&output.stderr),
            args.join(" ")
        )));
    }

    Ok(())
}

/// Applies network impairment bidirectionally on a veth link.
///
/// Shapes the forward path (data: `fwd_ns`/`fwd_iface`) with the full config,
/// and the return path (ACKs: `ret_ns`/`ret_iface`) with delay + loss only
/// (no rate limit).  This produces realistic symmetric RTT measurements
/// and ACK-path loss that match real cellular behavior.
///
/// Without return-path shaping, ACKs travel at infinite speed with zero
/// loss, producing artificially low RTT measurements and an unrealistically
/// reliable feedback channel.
pub fn apply_bidirectional_impairment(
    fwd_ns: &Namespace,
    fwd_iface: &str,
    ret_ns: &Namespace,
    ret_iface: &str,
    config: ImpairmentConfig,
) -> io::Result<()> {
    let ret_config = config.return_path_config();
    apply_impairment(fwd_ns, fwd_iface, config)?;
    apply_impairment(ret_ns, ret_iface, ret_config)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::check_privileges;

    // Helper to extract ping time from output
    fn get_ping_time(output: &str) -> Option<f32> {
        // Output format example: "64 bytes from 1.2.3.4: icmp_seq=1 ttl=64 time=102 ms"
        for line in output.lines() {
            if let Some(idx) = line.find("time=") {
                let rest = &line[idx + 5..];
                // finding the next space
                if let Some(end) = rest.find(' ') {
                    let time_str = &rest[..end];
                    return time_str.parse::<f32>().ok();
                }
            }
        }
        None
    }

    #[test]
    fn test_impairment() {
        if !check_privileges() {
            eprintln!("Skipping test_impairment, insufficient privileges");
            return;
        }

        let ns1 = Namespace::new("rst_imp_a").expect("Failed to create ns1");
        let ns2 = Namespace::new("rst_imp_b").expect("Failed to create ns2");

        // Link them: 10.201.1.1 <-> 10.201.1.2
        ns1.add_veth_link(&ns2, "veth_a", "veth_b", "10.201.1.1/24", "10.201.1.2/24")
            .expect("Failed to create link");

        // Apply 100ms delay on ns1 side
        let config = ImpairmentConfig {
            delay_ms: Some(100),
            jitter_ms: Some(10),   // slight jitter
            rate_kbit: Some(5000), // 5Mbps
            loss_percent: None,
            ..Default::default()
        };

        if let Err(err) = apply_impairment(&ns1, "veth_a", config) {
            let msg = err.to_string();
            if msg.contains("qdisc kind is unknown") {
                eprintln!("Skipping test_impairment, netem qdisc not available");
                return;
            }
            panic!("Failed to apply impairment: {}", err);
        }

        // Ping
        let out = ns1
            .exec("ping", &["-c", "4", "-i", "0.2", "10.201.1.2"])
            .expect("Failed to exec ping");

        let stdout = String::from_utf8_lossy(&out.stdout);
        println!("Ping output:\n{}", stdout);

        if !out.status.success() {
            panic!("Ping failed: {}", String::from_utf8_lossy(&out.stderr));
        }

        let rtt = get_ping_time(&stdout).expect("Could not parse ping time");
        println!("Measured RTT: {} ms", rtt);

        assert!(
            rtt >= 95.0,
            "RTT {} ms is less than expected delay 100ms",
            rtt
        );
    }
}
