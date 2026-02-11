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
#[derive(Debug, Clone, Default)]
pub struct ImpairmentConfig {
    pub delay_ms: Option<u32>,
    pub jitter_ms: Option<u32>,
    pub loss_percent: Option<f32>,
    pub loss_correlation: Option<f32>,
    pub gemodel: Option<GemodelConfig>, // If set, overrides loss_percent
    pub rate_kbit: Option<u64>,
    pub duplicate_percent: Option<f32>,
    pub reorder_percent: Option<f32>,
    pub corrupt_percent: Option<f32>,
    /// Override the netem queue `limit` (in packets).  When `None` and
    /// `rate_kbit` is set, an appropriate limit is auto-calculated from the
    /// bandwidth-delay product (~2× BDP).  This keeps the queue finite so
    /// excess packets are dropped at the netem level — drops that are visible
    /// to the receiver and produce RTCP NACKs, enabling AIMD convergence.
    ///
    /// Set explicitly to fine-tune burst tolerance.  A larger limit absorbs
    /// more bursts but delays congestion detection; a smaller limit is more
    /// aggressive but may drop packets even within capacity.
    pub netem_limit: Option<u32>,
}

/// Applies network impairment to an interface inside a namespace using `tc netem`.
///
/// Removes any existing root qdisc first, then installs netem with the specified
/// delay, loss, rate, duplication, reorder, and corruption parameters.
///
/// When `rate_kbit` is set, the netem `rate` parameter adds serialization delay
/// and the queue `limit` is set to a finite value (auto-calculated from the
/// bandwidth-delay product if not explicitly provided).  This finite queue causes
/// netem to **drop excess packets** when the sender exceeds link capacity — drops
/// that are visible to the receiver and trigger RTCP NACKs, enabling AIMD
/// convergence.
pub fn apply_impairment(
    ns: &Namespace,
    interface: &str,
    config: ImpairmentConfig,
) -> io::Result<()> {
    // 1. Remove existing qdisc (best effort) to ensure clean state or update
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

    // 2. Build netem command: tc qdisc add dev <iface> root netem [limit N] ...
    let mut args_storage: Vec<String> = vec![
        "qdisc".into(),
        "add".into(),
        "dev".into(),
        interface.into(),
        "root".into(),
        "netem".into(),
    ];

    // Determine queue limit.  When rate_kbit is set, we need a finite limit to
    // enforce bandwidth by dropping excess packets.  Auto-calculate from BDP if
    // not explicitly provided.
    let limit = if let Some(explicit) = config.netem_limit {
        Some(explicit)
    } else if let Some(rate) = config.rate_kbit {
        // BDP-based auto limit: 2 × (rate × rtt) / MTU, minimum 20.
        // Uses one-way delay (half RTT) from config, doubled for full RTT.
        let rtt_ms = config.delay_ms.unwrap_or(20) as u64 * 2;
        let bdp_bytes = rate * 1000 / 8 * rtt_ms / 1000; // bytes in one BDP
        let mtu = 1400u64;
        let bdp_packets = bdp_bytes / mtu;
        Some(std::cmp::max(bdp_packets as u32 * 2, 20))
    } else {
        None
    };

    if let Some(lim) = limit {
        args_storage.push("limit".into());
        args_storage.push(lim.to_string());
    }

    append_netem_params(&config, &mut args_storage);

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

/// Appends the netem-specific parameters (delay, loss, rate, duplicate,
/// reorder, corrupt) to an arg list.
fn append_netem_params(config: &ImpairmentConfig, args: &mut Vec<String>) {
    if let Some(delay) = config.delay_ms {
        args.push("delay".into());
        args.push(format!("{}ms", delay));

        if let Some(jitter) = config.jitter_ms {
            if jitter > 0 {
                args.push(format!("{}ms", jitter));
            }
        }
    }

    if let Some(gemodel) = &config.gemodel {
        args.push("loss".into());
        args.push("gemodel".into());
        args.push(format!("{}%", gemodel.p));
        args.push(format!("{}%", gemodel.r));
        args.push(format!("{}%", gemodel.one_h));
        args.push(format!("{}%", gemodel.one_k));
    } else if let Some(loss) = config.loss_percent {
        args.push("loss".into());
        args.push(format!("{}%", loss));
        if let Some(corr) = config.loss_correlation {
            args.push(format!("{}%", corr));
        }
    }

    if let Some(dup) = config.duplicate_percent {
        args.push("duplicate".into());
        args.push(format!("{}%", dup));
    }

    if let Some(reorder) = config.reorder_percent {
        args.push("reorder".into());
        args.push(format!("{}%", reorder));
    }

    if let Some(corrupt) = config.corrupt_percent {
        args.push("corrupt".into());
        args.push(format!("{}%", corrupt));
    }

    if let Some(rate) = config.rate_kbit {
        args.push("rate".into());
        args.push(format!("{}kbit", rate));
    }
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
