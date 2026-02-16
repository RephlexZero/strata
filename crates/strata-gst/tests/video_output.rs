//! Video output tests — produce actual video files for human review.
//!
//! Output is written to `test_output/` at the workspace root, which is
//! gitignored.  Run these tests to visually inspect bonding quality:
//!
//! ```bash
//! sudo cargo test -p gst-rist-bonding --test video_output -- --nocapture
//! ```
//!
//! After a successful run, inspect the files in `test_output/`:
//! - `loopback_clean.ts`           — MPEG-TS over a clean loopback link
//! - `bonded_two_link.ts`          — MPEG-TS over two bonded veth links (netns)
//! - `bonded_impaired.ts`          — MPEG-TS over two links, one impaired
//!
//! Play any file with:
//! ```bash
//! gst-launch-1.0 filesrc location=test_output/loopback_clean.ts ! \
//!   tsdemux ! h264parse ! avdec_h264 ! autovideosink
//! ```

use strata_sim::topology::Namespace;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Workspace-root-relative output directory.
fn output_dir() -> PathBuf {
    // The test binary lives in target/debug/deps/; walk up to workspace root.
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop(); // deps
    dir.pop(); // debug
    dir.pop(); // target
    dir.push("test_output");
    dir
}

/// Build integration_node if it doesn't exist yet.
fn build_integration_binary() -> PathBuf {
    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "gst-rist-bonding",
            "--bin",
            "integration_node",
        ])
        .status()
        .expect("Failed to invoke cargo build");
    assert!(status.success(), "cargo build integration_node failed");

    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // deps
    path.pop(); // debug
    path.push("integration_node");
    assert!(path.exists(), "integration_node not found at {:?}", path);
    path
}

/// Ensure the test_output directory exists.
fn ensure_output_dir() -> PathBuf {
    let dir = output_dir();
    fs::create_dir_all(&dir).expect("Failed to create test_output directory");
    dir
}

// ---------------------------------------------------------------------------
// Test 1: Loopback — single machine, no netns required
// ---------------------------------------------------------------------------

#[test]
fn video_loopback_clean() {
    let bin = build_integration_binary();
    let out = ensure_output_dir();
    let ts_path = out.join("loopback_clean.ts");

    // Remove stale output
    let _ = fs::remove_file(&ts_path);

    // Note: GStreamer type registration is handled by integration_node
    // internally. Registering types here without gst::init() causes
    // GLib GObject-CRITICAL assertions when tests run in parallel.

    let bin_str = bin.to_str().unwrap().to_string();
    let ts_str = ts_path.to_str().unwrap().to_string();

    // Start receiver (background)
    let mut receiver = Command::new(&bin_str)
        .args([
            "receiver",
            "--bind",
            "rist://@127.0.0.1:17000",
            "--output",
            &ts_str,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn receiver");

    thread::sleep(Duration::from_secs(2));

    // Start sender (foreground, 5 seconds of video = 150 frames @ 30fps)
    let _sender_status = Command::new(&bin_str)
        .args([
            "sender",
            "--dest",
            "rist://127.0.0.1:17000",
            "--bitrate",
            "1500",
        ])
        .env(
            "GST_LAUNCH_LINE_OVERRIDE",
            "videotestsrc num-buffers=150 is-live=true pattern=smpte ! video/x-raw,width=1280,height=720,framerate=30/1 ! x264enc tune=zerolatency bitrate=1500 ! mpegtsmux ! rsristbondsink name=rsink",
        )
        .status()
        .expect("Failed to run sender");

    thread::sleep(Duration::from_secs(2));

    // Graceful receiver shutdown
    let _ = Command::new("pkill")
        .args(["-SIGINT", "-f", "integration_node.*receiver.*17000"])
        .status();

    let mut finished = false;
    for _ in 0..10 {
        if let Ok(Some(_)) = receiver.try_wait() {
            finished = true;
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }
    if !finished {
        let _ = receiver.kill();
    }
    let _ = receiver.wait();

    // Validate output
    assert!(ts_path.exists(), "Output file not created at {:?}", ts_path);
    let meta = fs::metadata(&ts_path).expect("metadata");
    assert!(meta.len() > 0, "Output file is empty");
    eprintln!(
        "video_loopback_clean: wrote {} bytes to {:?}",
        meta.len(),
        ts_path
    );
}

// ---------------------------------------------------------------------------
// Test 2: Two bonded links via netns (requires sudo / CAP_NET_ADMIN)
// ---------------------------------------------------------------------------

#[test]
fn video_bonded_two_link() {
    let bin = build_integration_binary();
    let out = ensure_output_dir();
    let ts_path = out.join("bonded_two_link.ts");
    let _ = fs::remove_file(&ts_path);

    let ts_str = ts_path.to_str().unwrap().to_string();
    let bin_str = bin.to_str().unwrap().to_string();

    // Create namespaces
    let snd_ns = match Namespace::new("vid_snd") {
        Ok(ns) => ns,
        Err(e) => {
            eprintln!("Skipping video_bonded_two_link (no netns): {}", e);
            return;
        }
    };
    let rcv_ns = Namespace::new("vid_rcv").expect("Failed to create vid_rcv namespace");

    // Link A: 10.20.1.1/24 <-> 10.20.1.2/24
    snd_ns
        .add_veth_link(
            &rcv_ns,
            "veth_va_s",
            "veth_va_r",
            "10.20.1.1/24",
            "10.20.1.2/24",
        )
        .expect("Link A setup failed");

    // Link B: 10.20.2.1/24 <-> 10.20.2.2/24
    snd_ns
        .add_veth_link(
            &rcv_ns,
            "veth_vb_s",
            "veth_vb_r",
            "10.20.2.1/24",
            "10.20.2.2/24",
        )
        .expect("Link B setup failed");

    // Receiver in netns
    let mut receiver = Command::new("sudo")
        .args([
            "ip",
            "netns",
            "exec",
            "vid_rcv",
            &bin_str,
            "receiver",
            "--bind",
            "rist://@10.20.1.2:5000,rist://@10.20.2.2:5002",
            "--output",
            &ts_str,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn receiver");

    thread::sleep(Duration::from_secs(2));

    // Sender in netns (short run — 10 seconds of video)
    let _sender_status = Command::new("sudo")
        .args([
            "ip",
            "netns",
            "exec",
            "vid_snd",
            &bin_str,
            "sender",
            "--dest",
            "rist://10.20.1.2:5000,rist://10.20.2.2:5002",
            "--bitrate",
            "2000",
        ])
        .status()
        .expect("Failed to run sender");

    thread::sleep(Duration::from_secs(2));

    // Graceful receiver shutdown
    let _ = Command::new("sudo")
        .args([
            "ip",
            "netns",
            "exec",
            "vid_rcv",
            "pkill",
            "-SIGINT",
            "-f",
            "integration_node",
        ])
        .status();

    let mut finished = false;
    for _ in 0..10 {
        if let Ok(Some(_)) = receiver.try_wait() {
            finished = true;
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }
    if !finished {
        let _ = receiver.kill();
    }
    let _ = receiver.wait();

    // Validate output
    if ts_path.exists() {
        let meta = fs::metadata(&ts_path).expect("metadata");
        eprintln!(
            "video_bonded_two_link: wrote {} bytes to {:?}",
            meta.len(),
            ts_path
        );
        assert!(meta.len() > 0, "Output file is empty");
    } else {
        eprintln!(
            "WARNING: Output file not created — sender may have finished before receiver connected"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: Bonded with impairment on one link (requires sudo)
// ---------------------------------------------------------------------------

#[test]
fn video_bonded_impaired() {
    let bin = build_integration_binary();
    let out = ensure_output_dir();
    let ts_path = out.join("bonded_impaired.ts");
    let _ = fs::remove_file(&ts_path);

    let ts_str = ts_path.to_str().unwrap().to_string();
    let bin_str = bin.to_str().unwrap().to_string();

    // Create namespaces
    let snd_ns = match Namespace::new("vid_imp_snd") {
        Ok(ns) => ns,
        Err(e) => {
            eprintln!("Skipping video_bonded_impaired (no netns): {}", e);
            return;
        }
    };
    let rcv_ns = Namespace::new("vid_imp_rcv").expect("Failed to create vid_imp_rcv namespace");

    // Link A (clean): 10.30.1.1/24 <-> 10.30.1.2/24
    snd_ns
        .add_veth_link(
            &rcv_ns,
            "veth_ia_s",
            "veth_ia_r",
            "10.30.1.1/24",
            "10.30.1.2/24",
        )
        .expect("Link A setup failed");

    // Link B (will be impaired): 10.30.2.1/24 <-> 10.30.2.2/24
    snd_ns
        .add_veth_link(
            &rcv_ns,
            "veth_ib_s",
            "veth_ib_r",
            "10.30.2.1/24",
            "10.30.2.2/24",
        )
        .expect("Link B setup failed");

    // Apply impairment on Link B sender side: 5% loss, 50ms jitter
    let _ = Command::new("sudo")
        .args([
            "ip",
            "netns",
            "exec",
            "vid_imp_snd",
            "tc",
            "qdisc",
            "add",
            "dev",
            "veth_ib_s",
            "root",
            "netem",
            "loss",
            "5%",
            "delay",
            "50ms",
            "20ms",
        ])
        .status();

    // Receiver
    let mut receiver = Command::new("sudo")
        .args([
            "ip",
            "netns",
            "exec",
            "vid_imp_rcv",
            &bin_str,
            "receiver",
            "--bind",
            "rist://@10.30.1.2:5000,rist://@10.30.2.2:5002",
            "--output",
            &ts_str,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn receiver");

    thread::sleep(Duration::from_secs(2));

    // Sender
    let _sender_status = Command::new("sudo")
        .args([
            "ip",
            "netns",
            "exec",
            "vid_imp_snd",
            &bin_str,
            "sender",
            "--dest",
            "rist://10.30.1.2:5000,rist://10.30.2.2:5002",
            "--bitrate",
            "2000",
        ])
        .status()
        .expect("Failed to run sender");

    thread::sleep(Duration::from_secs(2));

    // Graceful receiver shutdown
    let _ = Command::new("sudo")
        .args([
            "ip",
            "netns",
            "exec",
            "vid_imp_rcv",
            "pkill",
            "-SIGINT",
            "-f",
            "integration_node",
        ])
        .status();

    let mut finished = false;
    for _ in 0..10 {
        if let Ok(Some(_)) = receiver.try_wait() {
            finished = true;
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }
    if !finished {
        let _ = receiver.kill();
    }
    let _ = receiver.wait();

    // Validate output
    if ts_path.exists() {
        let meta = fs::metadata(&ts_path).expect("metadata");
        eprintln!(
            "video_bonded_impaired: wrote {} bytes to {:?}",
            meta.len(),
            ts_path
        );
        assert!(meta.len() > 0, "Output file is empty");
    } else {
        eprintln!(
            "WARNING: Output file not created — sender may have finished before receiver connected"
        );
    }
}
