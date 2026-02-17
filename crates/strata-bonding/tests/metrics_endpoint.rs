//! Integration tests for the Prometheus metrics endpoint wired into BondingRuntime.
//!
//! Verifies that:
//! - `BondingRuntime::start_metrics_server()` starts an HTTP server
//! - `GET /metrics` returns valid Prometheus text with link metrics
//! - The server reflects live link state changes
//! - Non-metrics paths return 404

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use strata_bonding::config::{LinkConfig, SchedulerConfig};
use strata_bonding::metrics::MetricsServer;
use strata_bonding::runtime::BondingRuntime;

/// Helper: HTTP GET and return the full response string.
fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to metrics server");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

#[test]
fn runtime_metrics_server_serves_prometheus() {
    // Create a runtime with one link
    let rt = BondingRuntime::with_config(SchedulerConfig::default());
    let rcv_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let rcv_addr = rcv_socket.local_addr().unwrap();
    rt.add_link(LinkConfig {
        id: 1,
        uri: format!("{}", rcv_addr),
        interface: None,
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(250));

    // Start metrics server using the runtime's metrics handle
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut server =
        MetricsServer::start(bind, rt.metrics_handle()).expect("metrics server should start");
    let addr = server.addr();
    std::thread::sleep(Duration::from_millis(100));

    // GET /metrics should return 200 with Prometheus text
    let resp = http_get(addr, "/metrics");
    assert!(
        resp.starts_with("HTTP/1.1 200 OK"),
        "expected 200, got: {}",
        &resp[..resp.len().min(80)]
    );
    assert!(resp.contains("text/plain"));
    assert!(
        resp.contains("strata_link_rtt_ms"),
        "should have RTT metric"
    );
    assert!(
        resp.contains("strata_links_total"),
        "should have total metric"
    );
    assert!(resp.contains("link_id=\"1\""), "should show link 1");

    server.stop();
}

#[test]
fn runtime_metrics_reflect_link_changes() {
    let rt = BondingRuntime::with_config(SchedulerConfig::default());

    // Start with no links
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut server =
        MetricsServer::start(bind, rt.metrics_handle()).expect("metrics server should start");
    let addr = server.addr();
    std::thread::sleep(Duration::from_millis(100));

    // Initially: 0 links
    let resp = http_get(addr, "/metrics");
    assert!(resp.contains("strata_links_total 0"));

    // Add a link
    let rcv_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let rcv_addr = rcv_socket.local_addr().unwrap();
    rt.add_link(LinkConfig {
        id: 1,
        uri: format!("{}", rcv_addr),
        interface: None,
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(250));

    // Now: 1 link
    let resp = http_get(addr, "/metrics");
    assert!(
        resp.contains("strata_links_total 1"),
        "should show 1 link after add"
    );

    // Remove the link
    rt.remove_link(1).unwrap();
    std::thread::sleep(Duration::from_millis(250));

    // Back to 0
    let resp = http_get(addr, "/metrics");
    assert!(
        resp.contains("strata_links_total 0"),
        "should show 0 after remove"
    );

    server.stop();
}

#[test]
fn metrics_server_404_for_non_metrics_path() {
    let rt = BondingRuntime::with_config(SchedulerConfig::default());
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut server =
        MetricsServer::start(bind, rt.metrics_handle()).expect("metrics server should start");
    let addr = server.addr();
    std::thread::sleep(Duration::from_millis(100));

    let resp = http_get(addr, "/");
    assert!(
        resp.starts_with("HTTP/1.1 404"),
        "non-metrics path should 404"
    );

    server.stop();
}
