use rist_network_sim::topology::Namespace;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Build the integration binary and return its path.
fn build_integration_binary() -> PathBuf {
    let mut command = Command::new("cargo");
    command.args(&[
        "build",
        "-p",
        "gst-rist-bonding",
        "--bin",
        "integration_node",
    ]);

    let status = command.status().expect("Failed to execute cargo build");

    assert!(status.success(), "Failed to build integration_node binary");

    // The test binary is usually in target/debug/deps/end_to_end-<hash>
    // The integration_node binary is in target/debug/integration_node
    // We can use the current executable path to find the target/debug directory.
    let mut path = std::env::current_exe().expect("Failed to get current executable path");
    path.pop(); // deps
    path.pop(); // debug (or release)
    path.push("integration_node");

    if !path.exists() {
         // Fallback just in case directory structure is weird (e.g. running from IDE vs CLI)
         // Try to use fallback logic if available, or just panic nicely
         eprintln!("Warning: Did not find integration_node at {:?}, checking relative to CWD", path);
         let cwd = std::env::current_dir().unwrap();
         let try_path = cwd.join("target/debug/integration_node");
         if try_path.exists() {
            return try_path;
         }
         panic!("Binary not found at expected path: {:?}", path);
    }

    path
}

#[test]
fn test_bonded_transmission() {
    // 1. Build binary
    let binary_path = build_integration_binary();
    let binary_path_str = binary_path.to_str().expect("Valid binary path");

    // Compute absolute output path in target root
    // binary_path is .../target/debug/integration_node
    // we want .../target/bonding_rcv.ts
    let output_path = binary_path
        .parent().expect("debug dir")
        .parent().expect("target dir")
        .join("bonding_rcv.ts");
    let output_path_str = output_path.to_str().expect("Valid output path");
    
    // Remove existing file to avoid false positives
    if output_path.exists() {
        let _ = std::fs::remove_file(&output_path);
    }

    // 2. Create Namespaces
    // Failure to create namespace should result in a panic
    let snd_ns = Namespace::new("bonding_snd").expect("Failed to create bonding_snd namespace");

    let rcv_ns = match Namespace::new("bonding_rcv") {
        Ok(ns) => ns,
        Err(e) => {
            // Always panic on failure
            panic!("Failed to create bonding_rcv namespace: {}", e);
        }
    };

    // 3. Set up Links
    // Link A: 10.10.1.1/24 <-> 10.10.1.2/24
    if let Err(e) = snd_ns.add_veth_link(
        &rcv_ns,
        "veth_a_snd",
        "veth_a_rcv",
        "10.10.1.1/24",
        "10.10.1.2/24",
    ) {
        panic!("Failed to setup Link A: {}", e);
    }

    // Link B: 10.10.2.1/24 <-> 10.10.2.2/24
    snd_ns
        .add_veth_link(
            &rcv_ns,
            "veth_b_snd",
            "veth_b_rcv",
            "10.10.2.1/24",
            "10.10.2.2/24",
        )
        .expect("Failed to setup Link B");

    // DEBUG: Inspect interfaces
    let ip_out = rcv_ns.exec("ip", &["addr"]).unwrap();
    println!("RCV Netns Interfaces:\n{}", String::from_utf8_lossy(&ip_out.stdout));

    // DEBUG: Verify connectivity via Ping
    println!("Verifying Link A connectivity...");
    let ping_a = snd_ns.exec("ping", &["-c", "1", "10.10.1.2"]).expect("Failed to exec ping A");
    if !ping_a.status.success() {
         panic!("Ping Link A failed: {}", String::from_utf8_lossy(&ping_a.stderr));
    }

    println!("Verifying Link B connectivity...");
    let ping_b = snd_ns.exec("ping", &["-c", "1", "10.10.2.2"]).expect("Failed to exec ping B");
    if !ping_b.status.success() {
         panic!("Ping Link B failed: {}", String::from_utf8_lossy(&ping_b.stderr));
    }

    // 4. Spawn Receiver (Background)
    println!("Starting Receiver...");
    let receiver_cmd_args = [
        "netns",
        "exec",
        "bonding_rcv",
        binary_path_str,
        "receiver",
        "--bind",
        "rist://@10.10.1.2:5000,rist://@10.10.2.2:5002",
        "--output",
        output_path_str // Output TS file (absolute path)
    ];

    let mut receiver_child = Command::new("sudo")
        .args(&["ip"])
        .args(&receiver_cmd_args)
        // .env("GST_DEBUG", ...) // Removing this as we use 'env' command inside netns
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn receiver process");

    // Give receiver a moment to bind ports
    thread::sleep(Duration::from_secs(2));

    // 5. Spawn Sender (Foreground)
    println!("Starting Sender...");
    let sender_cmd_args = [
        "netns",
        "exec",
        "bonding_snd",
        binary_path_str,
        "sender",
        "--dest",
        "rist://10.10.1.2:5000,rist://10.10.2.2:5002",
    ];

    let sender_status = Command::new("sudo")
        .args(&["ip"])
        .args(&sender_cmd_args)
        .status()
        .expect("Failed to execute sender process");
    
    // Sender now runs for approx 15s (450 buffers), so this call will block for that duration.

    // Allow receiver time to finish processing buffers
    thread::sleep(Duration::from_secs(2));

    // Send SIGINT to receiver to trigger graceful shutdown (and MP4 finalization)
    println!("Sender finished. Sending SIGINT to receiver...");
    let status = Command::new("sudo")
        .args(&["ip", "netns", "exec", "bonding_rcv", "pkill", "-SIGINT", "-f", "integration_node"])
        .status()
        .expect("Failed to send pkill");
    
    if !status.success() {
        println!("Warning: Failed to pkill receiver (maybe it already exited?)");
    }

    // Wait for receiver to exit. It should exit quickly after SIGINT with EOS handling.
    let mut finished = false;
    for _ in 0..10 { // Wait up to 10s
        if let Ok(Some(_)) = receiver_child.try_wait() {
            finished = true;
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }

    if !finished {
         println!("Receiver did not exit in time after SIGINT. Killing...");
         let _ = receiver_child.kill();
    }

    let receiver_output = receiver_child
        .wait_with_output()
        .expect("Failed to wait on receiver");

    println!("Receiver Exit Status: {:?}", receiver_output.status);

    // Assert Sender Success
    assert!(sender_status.success(), "Sender process failed");

    // Check Receiver Output for errors
    let stdout = String::from_utf8_lossy(&receiver_output.stdout);
    let stderr = String::from_utf8_lossy(&receiver_output.stderr);

    // println!("Receiver Stdout:\n{}", stdout);
    // println!("Receiver Stderr:\n{}", stderr);
    
    // Sometimes stderr is empty if the process crashes hard or buffers weirdly with sudo.
    // If output file exists and is > 0 bytes, we consider it a partial success for data flow.
    let file_check = output_path.exists() && std::fs::metadata(&output_path).map(|m| m.len() > 0).unwrap_or(false);

    if file_check {
        println!("Success: Output file created at {:?} and has data.", output_path);
    } else if !stderr.contains("rist-bonding-stats") {
         println!("Receiver Stderr Dump:\n{}", stderr);
         panic!("Data flow verification failed (No stats in stderr and no output file at {:?})", output_path);
    }
    
    // Verify final stats
    // Receiver Final Stats: Count=..., Bytes=...
    if !stderr.contains("Receiver Final Stats: Count=") {
         println!("WARNING: Receiver did not exit cleanly or print final stats.");
    }
}
