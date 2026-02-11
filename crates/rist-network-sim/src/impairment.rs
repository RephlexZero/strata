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
    /// When `true` and `rate_kbit` is set, a TBF (Token Bucket Filter) qdisc is
    /// installed as root to enforce the bandwidth limit by actually dropping
    /// excess packets, with netem chained as a child.
    ///
    /// When `false` (default), `rate_kbit` is passed to netem's `rate` parameter
    /// which only adds serialization delay without dropping packets.  This is
    /// suitable for tests that rely on AIMD convergence via RTCP feedback, where
    /// kernel-level drops (invisible to the application) would be counter-productive.
    pub tbf_shaping: bool,
}

/// Applies network impairment to an interface inside a namespace using `tc`.
///
/// Removes any existing root qdisc first, then configures the specified
/// delay, loss, duplication, reorder, and corruption parameters via netem.
///
/// When `rate_kbit` is set, a TBF (Token Bucket Filter) qdisc is installed as
/// the root to enforce the bandwidth limit by dropping excess packets, with
/// netem chained as a child for delay/loss/etc.  This is the standard Linux
/// approach for bandwidth shaping – plain `netem rate` only adds serialization
/// delay but never drops packets until the (very large) default queue limit is
/// reached.
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

    // 2. Determine whether we need TBF for bandwidth enforcement.
    //    When tbf_shaping is true and rate_kbit is set, we use:
    //      root -> tbf (rate enforcement) -> netem (delay/loss/etc.)
    //    Otherwise, we use:
    //      root -> netem (delay/loss/etc.)  [rate passed as netem param if set]
    let has_netem_params = config.delay_ms.is_some()
        || config.loss_percent.is_some()
        || config.gemodel.is_some()
        || config.duplicate_percent.is_some()
        || config.reorder_percent.is_some()
        || config.corrupt_percent.is_some();

    if config.tbf_shaping {
        let rate = config
            .rate_kbit
            .ok_or_else(|| io::Error::other("tbf_shaping requires rate_kbit to be set"))?;
        // Install TBF as root qdisc for bandwidth enforcement.
        // burst = max(rate_bytes_per_sec / 10, 1540) — enough for one MTU at minimum.
        // latency = 1s — large enough to absorb video I-frame bursts without
        // spurious drops, while still enforcing sustained rate limits.
        let rate_bytes_per_sec = rate * 1000 / 8;
        let burst = std::cmp::max(rate_bytes_per_sec / 10, 1540);
        let tbf_args: Vec<String> = vec![
            "qdisc".into(),
            "add".into(),
            "dev".into(),
            interface.into(),
            "root".into(),
            "handle".into(),
            "1:".into(),
            "tbf".into(),
            "rate".into(),
            format!("{}kbit", rate),
            "burst".into(),
            format!("{}", burst),
            "latency".into(),
            "1s".into(),
        ];
        let args: Vec<&str> = tbf_args.iter().map(|s| s.as_str()).collect();
        let output = ns.exec("tc", &args)?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "Failed to apply TBF qdisc: {}\nCommand: tc {}",
                String::from_utf8_lossy(&output.stderr),
                args.join(" ")
            )));
        }

        // Chain netem as child of TBF (if any netem params are specified).
        if has_netem_params {
            let mut netem_args: Vec<String> = vec![
                "qdisc".into(),
                "add".into(),
                "dev".into(),
                interface.into(),
                "parent".into(),
                "1:1".into(),
                "handle".into(),
                "10:".into(),
                "netem".into(),
            ];
            append_netem_params(&config, &mut netem_args, false);

            let args: Vec<&str> = netem_args.iter().map(|s| s.as_str()).collect();
            let output = ns.exec("tc", &args)?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "Failed to apply netem child qdisc: {}\nCommand: tc {}",
                    String::from_utf8_lossy(&output.stderr),
                    args.join(" ")
                )));
            }
        }
    } else {
        // No rate limiting — just use netem as root.
        let mut args_storage: Vec<String> = vec![
            "qdisc".into(),
            "add".into(),
            "dev".into(),
            interface.into(),
            "root".into(),
            "netem".into(),
        ];
        append_netem_params(&config, &mut args_storage, true);

        let args: Vec<&str> = args_storage.iter().map(|s| s.as_str()).collect();
        let output = ns.exec("tc", &args)?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "Failed to apply tc netem: {}\nCommand: tc {}",
                String::from_utf8_lossy(&output.stderr),
                args.join(" ")
            )));
        }
    }

    Ok(())
}

/// Appends the netem-specific parameters (delay, loss, duplicate, reorder,
/// corrupt) to an arg list.  When `include_rate` is true and `rate_kbit` is
/// set, the netem `rate` parameter is also appended (serialization delay only,
/// no actual enforcement).
fn append_netem_params(config: &ImpairmentConfig, args: &mut Vec<String>, include_rate: bool) {
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

    if include_rate {
        if let Some(rate) = config.rate_kbit {
            args.push("rate".into());
            args.push(format!("{}kbit", rate));
        }
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
