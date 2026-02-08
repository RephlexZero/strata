use crate::topology::Namespace;
use std::io;

#[derive(Debug, Clone, Default)]
pub struct GemodelConfig {
    pub p: f32,     // Probability Good -> Bad (%)
    pub r: f32,     // Probability Bad -> Good (%)
    pub one_h: f32, // Loss probability in Good state (%)
    pub one_k: f32, // Loss probability in Bad state (%)
}

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
}

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

    // 2. Build netem command arguments
    // command: tc qdisc add dev <interface> root netem ...
    let mut args_storage: Vec<String> = vec![
        "qdisc".to_string(),
        "add".to_string(),
        "dev".to_string(),
        interface.to_string(),
        "root".to_string(),
        "netem".to_string(),
    ];

    if let Some(delay) = config.delay_ms {
        args_storage.push("delay".to_string());
        args_storage.push(format!("{}ms", delay));

        if let Some(jitter) = config.jitter_ms {
            if jitter > 0 {
                args_storage.push(format!("{}ms", jitter));
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
    }

    if let Some(corrupt) = config.corrupt_percent {
        args_storage.push("corrupt".to_string());
        args_storage.push(format!("{}%", corrupt));
    }

    if let Some(rate) = config.rate_kbit {
        args_storage.push("rate".to_string());
        args_storage.push(format!("{}kbit", rate));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn check_privileges() -> bool {
        match Command::new("ip").arg("netns").output() {
            Ok(o) => o.status.success(),
            Err(_) => false,
        }
    }

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

        apply_impairment(&ns1, "veth_a", config).expect("Failed to apply impairment");

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
