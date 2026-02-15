//! Shared utility functions for the agent.

use std::time::Duration;

/// Parse a host:port from a URL and attempt a TCP connection with timeout.
///
/// Used for connectivity checks — cloud reachability and receiver reachability.
pub async fn check_tcp_reachable(url: &str, timeout_secs: u64) -> bool {
    let host = extract_host(url);
    if host.is_empty() {
        return false;
    }
    tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        tokio::net::TcpStream::connect(&host),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// Extract host:port from various URL schemes.
///
/// Handles `ws://`, `wss://`, `http://`, `https://`, `rist://`, `srt://`.
/// Strips the path portion (everything after the first `/` past the scheme).
fn extract_host(url: &str) -> String {
    let stripped = url
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_start_matches("rist://")
        .trim_start_matches("srt://");
    stripped.split('/').next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_host_from_urls() {
        assert_eq!(extract_host("ws://control:3000/agent/ws"), "control:3000");
        assert_eq!(
            extract_host("wss://cloud.example.com:443/ws"),
            "cloud.example.com:443"
        );
        assert_eq!(
            extract_host("rist://192.168.1.100:5000"),
            "192.168.1.100:5000"
        );
        assert_eq!(extract_host("srt://receiver:4000/stream"), "receiver:4000");
        assert_eq!(extract_host("http://localhost:3000/api"), "localhost:3000");
        assert_eq!(extract_host(""), "");
    }
}
