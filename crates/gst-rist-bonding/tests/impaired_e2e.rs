use plotters::prelude::*;
use serde_json::Value;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use rist_network_sim::topology::Namespace;
use rist_network_sim::impairment::{apply_impairment, ImpairmentConfig};

// Helper to spawn async process in namespace
fn spawn_in_ns(ns_name: &str, cmd: &str, args: &[&str]) -> std::process::Child {
    // Determine command wrapper based on privs / environment
    // Assuming sudo or root is available as per rist-network-sim
    std::process::Command::new("sudo")
        .args(&["ip", "netns", "exec", ns_name, cmd])
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit()) // Only show stderr
        .spawn()
        .expect("Failed to spawn process in ns")
}

#[test]
fn test_impaired_bonding_visualization() {
    // 0. Ensure integration_node is built
    let pkg_root = std::env::current_dir().unwrap();
    // Assuming we are in workspace root or crate root.
    // Let's rely on CARGO_BIN_EXE_integration_node if available, else derive it.
    let bin_path = if let Ok(p) = std::env::var("CARGO_BIN_EXE_integration_node") {
        std::path::PathBuf::from(p)
    } else {
        // Fallback for when running via cargo test in dev container
        pkg_root.join("../../target/debug/integration_node")
    };
    
    if !bin_path.exists() {
        // Fallback: build it
         let _ = std::process::Command::new("cargo")
            .args(&["build", "--bin", "integration_node"])
            .status();
    }
    
    // 1. Setup Topology
    if !std::process::Command::new("ip").arg("netns").output().map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("Skipping test: requires root/netns privileges");
        return;
    }

    // Use Arc to share namespaces across threads, so they don't Drop prematurely
    let ns_snd = Arc::new(Namespace::new("rst_bond_snd").unwrap());
    let ns_rcv = Arc::new(Namespace::new("rst_bond_rcv").unwrap());

    // Link 1: 10.0.1.1 <-> 10.0.1.2
    ns_snd.add_veth_link(&ns_rcv, "veth1_a", "veth1_b", "10.0.1.1/24", "10.0.1.2/24").unwrap();
    // Link 2: 10.0.2.1 <-> 10.0.2.2
    ns_snd.add_veth_link(&ns_rcv, "veth2_a", "veth2_b", "10.0.2.1/24", "10.0.2.2/24").unwrap();

    // Mgmt Link for Stats: Host (192.168.100.1) <-> ns_snd (192.168.100.2)
    let _ = std::process::Command::new("sudo").args(&["ip", "link", "del", "veth_mgmt_h"]).output();
    let _ = std::process::Command::new("sudo").args(&["ip", "link", "add", "veth_mgmt_h", "type", "veth", "peer", "name", "veth_mgmt_c"]).output();
    let _ = std::process::Command::new("sudo").args(&["ip", "link", "set", "veth_mgmt_c", "netns", "rst_bond_snd"]).output();
    
    let _ = std::process::Command::new("sudo").args(&["ip", "addr", "add", "192.168.100.1/24", "dev", "veth_mgmt_h"]).output();
    let _ = std::process::Command::new("sudo").args(&["ip", "link", "set", "veth_mgmt_h", "up"]).output();
    
    let _ = ns_snd.exec("ip", &["addr", "add", "192.168.100.2/24", "dev", "veth_mgmt_c"]);
    let _ = ns_snd.exec("ip", &["link", "set", "veth_mgmt_c", "up"]);

    // 2. Start Receiver (Background)
    println!("Starting Receiver...");
    let mut recv_child = spawn_in_ns(
        &ns_rcv.name,
        bin_path.to_str().unwrap(),
        &["receiver", "--bind", "rist://0.0.0.0:5000"]
    );

    // 3. Start Stats Collector (Thread)
    println!("Starting Collector...");
    let stats_socket = UdpSocket::bind("192.168.100.1:9000").expect("Failed to bind stats socket");
    stats_socket.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    
    let collected_data = Arc::new(Mutex::new(Vec::new()));
    let data_clone = collected_data.clone();
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let running_clone = running.clone();

    let collector_handle = thread::spawn(move || {
        let mut buf = [0u8; 65535];
        while running_clone.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok((amt, _)) = stats_socket.recv_from(&mut buf) {
                if let Ok(val) = serde_json::from_slice::<Value>(&buf[..amt]) {
                    let mut data = data_clone.lock().unwrap();
                    data.push(val);
                }
            }
        }
    });

    // 4. Start Sender (Background)
    println!("Starting Sender...");
    let mut send_child = spawn_in_ns(
        &ns_snd.name,
        bin_path.to_str().unwrap(),
        &[
            "sender", 
            "--dest", "rist://10.0.1.2:5000,rist://10.0.2.2:5000",
            "--stats-dest", "192.168.100.1:9000",
            "--bitrate", "5000"
        ]
    );

    // 5. Run Scenario (30s)
    let start = Instant::now();
    let chaos_ns = ns_snd.clone();
    
    println!("Running Chaos Scenario...");
    while start.elapsed().as_secs() < 30 {
        let t = start.elapsed().as_secs();

        // 0-10s: Good
        // 10-20s: Link 2 Loss 5%
        // 20-30s: Link 2 Limited to 1Mbps
        
        let mut config = ImpairmentConfig::default();
        if t >= 10 && t < 20 {
            config.loss_percent = Some(5.0);
        } else if t >= 20 {
            config.rate_kbit = Some(1000);
            config.delay_ms = Some(50); // Add delay to force queue buildup
        }
        
        // Apply to Link 2 interface (veth2_a inside sender ns)
        let _ = apply_impairment(&chaos_ns, "veth2_a", config);
        
        thread::sleep(Duration::from_secs(1));
    }

    println!("Scenario complete. Shutting down.");
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    
    let _ = send_child.kill();
    let _ = recv_child.kill();
    let _ = collector_handle.join();

    // Cleanup mgmt link
    let _ = std::process::Command::new("sudo").args(&["ip", "link", "del", "veth_mgmt_h"]).output();

    // 6. Plot Results
    let data = collected_data.lock().unwrap();
    if data.is_empty() {
        eprintln!("No stats collected! Check connectivity.");
    } else {
        println!("Collected {} stats points. Generating plot...", data.len());
        plot_results(&data);
    }
}

fn plot_results(data: &[Value]) {
    let filename = "bandwidth_tracking.svg";
    let root = SVGBackend::new(filename, (1024, 768)).into_drawing_area();
    root.fill(&WHITE).unwrap();

    let t0 = data.first().and_then(|v| v["timestamp"].as_f64()).unwrap_or(0.0);
    
    let mut ts = Vec::new();
    let mut caps = Vec::new();
    let mut losses = Vec::new();

    for v in data {
        if let Some(t_abs) = v["timestamp"].as_f64() {
            let t = t_abs - t0;
            if t < 0.0 { continue; }
            
            // Convert capacity to Mbps
            let cap = v["total_capacity"].as_f64().unwrap_or(0.0) / 1_000_000.0;
            
            // Extract Link 2 metrics (assuming link 1 is present at "1" or similar)
            let l2_stats = &v["links"]["1"];
            let loss = l2_stats["loss"].as_f64().unwrap_or(0.0) * 100.0;

            ts.push(t);
            caps.push(cap);
            losses.push(loss);
        }
    }

    let mut chart = ChartBuilder::on(&root)
        .caption("Bonding Performance: Capacity vs Impairment", ("sans-serif", 30).into_font())
        .margin(20)
        .x_label_area_size(40)
        .y_label_area_size(40)
        .right_y_label_area_size(40)
        .build_cartesian_2d(0.0..32.0, 0.0..10.0) // Capacity 0-10 Mbps
        .unwrap()
        .set_secondary_coord(0.0..32.0, 0.0..20.0); // Loss 0-20%

    chart.configure_mesh()
        .x_desc("Time (s)")
        .y_desc("Total Capacity (Mbps)")
        .draw().unwrap();

    chart.configure_secondary_axes()
        .y_desc("Link 2 Loss (%)")
        .draw().unwrap();

    // Draw Capacity line (Blue)
    chart.draw_series(LineSeries::new(
        ts.iter().zip(caps.iter()).map(|(&t, &c)| (t, c)),
        &BLUE,
    )).unwrap()
    .label("Total Capacity (Mbps)")
    .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &BLUE));

    // Draw Loss line (Red) - Secondary Axis
    chart.draw_secondary_series(LineSeries::new(
        ts.iter().zip(losses.iter()).map(|(&t, &l)| (t, l)),
        &RED,
    )).unwrap()
    .label("Link 2 Loss (%)")
    .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &RED));

    chart.configure_series_labels()
        .background_style(&WHITE.mix(0.8))
        .border_style(&BLACK)
        .draw().unwrap();

    eprintln!("Plot saved to {}", filename);
}
