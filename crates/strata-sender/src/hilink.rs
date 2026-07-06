//! Best-effort Huawei HiLink modem probe.
//!
//! HiLink modems (E3372 etc.) present as USB ethernet with a NAT gateway
//! (typically 192.168.8.1 / 192.168.9.1) exposing an XML HTTP API — the
//! same one scripts/band-lock.sh drives. Monitoring GETs need a session
//! cookie from `/api/webserver/SesTokInfo`.
//!
//! Everything here is strictly best-effort with short timeouts: a gateway
//! that isn't a HiLink modem just yields `None` and the caller caches the
//! failure so heartbeat scans don't hammer it.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Signal/carrier info read from a HiLink modem.
#[derive(Debug, Clone, Default)]
pub struct ModemInfo {
    pub carrier: Option<String>,
    pub technology: Option<String>,
    pub band: Option<String>,
    pub cell_id: Option<String>,
    /// RSRP in dBm (negative).
    pub signal_dbm: Option<i32>,
}

const HTTP_TIMEOUT: Duration = Duration::from_millis(1200);

/// Probe a gateway for HiLink modem status. Returns `None` if the gateway
/// doesn't speak the HiLink API (or is too slow).
pub async fn probe(gateway: &str) -> Option<ModemInfo> {
    let session = http_get(gateway, "/api/webserver/SesTokInfo", None).await?;
    let cookie = extract_tag(&session, "SesInfo")?;

    let signal = http_get(gateway, "/api/device/signal", Some(&cookie)).await;
    let plmn = http_get(gateway, "/api/net/current-plmn", Some(&cookie)).await;
    let status = http_get(gateway, "/api/monitoring/status", Some(&cookie)).await;

    if signal.is_none() && plmn.is_none() && status.is_none() {
        return None;
    }

    let mut info = ModemInfo::default();
    if let Some(xml) = signal {
        info.signal_dbm = extract_tag(&xml, "rsrp")
            .and_then(|v| v.trim_end_matches("dBm").trim().parse::<i32>().ok());
        info.band = extract_tag(&xml, "band").filter(|b| !b.is_empty() && b != "0");
        info.cell_id = extract_tag(&xml, "cell_id").filter(|c| !c.is_empty());
    }
    if let Some(xml) = plmn {
        info.carrier = extract_tag(&xml, "FullName")
            .or_else(|| extract_tag(&xml, "ShortName"))
            .filter(|c| !c.is_empty());
    }
    if let Some(xml) = status {
        info.technology = extract_tag(&xml, "CurrentNetworkTypeEx")
            .or_else(|| extract_tag(&xml, "CurrentNetworkType"))
            .and_then(|code| network_type_name(&code));
    }
    Some(info)
}

/// Map HiLink CurrentNetworkType(Ex) codes to a human label.
/// Only the codes seen on LTE-era sticks; unknown codes yield `None`
/// rather than a wrong label.
fn network_type_name(code: &str) -> Option<String> {
    let name = match code {
        "19" | "101" => "LTE",
        "1011" => "LTE+",
        "9" | "18" | "41" => "HSPA+",
        "4" | "5" | "6" | "7" | "8" => "3G",
        "1" | "2" | "3" => "2G",
        _ => return None,
    };
    Some(name.to_string())
}

/// Extract the text of the first `<tag>…</tag>` occurrence.
fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

/// Minimal HTTP/1.1 GET returning the response body. No TLS, no redirects —
/// HiLink modem UIs are plain HTTP on the LAN gateway.
///
/// Reads exactly `Content-Length` body bytes rather than waiting for EOF:
/// HiLink's httpd ignores our `Connection: close` request header and
/// replies `Connection: Keep-Alive` regardless, so `read_to_end()` would
/// block for its keep-alive timeout (tens of seconds) instead of ours,
/// making every probe silently time out and return `None`.
async fn http_get(host: &str, path: &str, cookie: Option<&str>) -> Option<String> {
    let fut = async {
        let mut stream = TcpStream::connect((host, 80)).await.ok()?;
        let cookie_hdr = cookie
            .map(|c| format!("Cookie: {c}\r\n"))
            .unwrap_or_default();
        let req =
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{cookie_hdr}Connection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.ok()?;
        read_http_body(&mut stream).await
    };
    tokio::time::timeout(HTTP_TIMEOUT, fut).await.ok().flatten()
}

/// Read an HTTP response's body off an already-connected stream, using the
/// `Content-Length` header rather than EOF to know when the body ends.
async fn read_http_body(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None; // closed before headers completed
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 16384 {
            return None; // headers unreasonably large — bail
        }
    };

    let content_length: usize = String::from_utf8_lossy(&buf[..header_end])
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        })
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);

    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break; // closed early — use what arrived
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let body_end = (header_end + content_length).min(buf.len());
    Some(String::from_utf8_lossy(&buf[header_end..body_end]).to_string())
}

/// Find the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tags() {
        let xml = "<response><rsrp>-95dBm</rsrp><band>8</band></response>";
        assert_eq!(extract_tag(xml, "rsrp").as_deref(), Some("-95dBm"));
        assert_eq!(extract_tag(xml, "band").as_deref(), Some("8"));
        assert_eq!(extract_tag(xml, "sinr"), None);
    }

    #[test]
    fn maps_network_types() {
        assert_eq!(network_type_name("101").as_deref(), Some("LTE"));
        assert_eq!(network_type_name("999"), None);
    }

    #[tokio::test]
    async fn probe_of_non_modem_returns_none() {
        // Nothing listens on this TEST-NET address; must time out to None.
        assert!(probe("192.0.2.1").await.is_none());
    }

    /// Reproduces the real HiLink bug: the server sends `Connection:
    /// Keep-Alive` and never closes the socket, regardless of what the
    /// client asked for. `read_http_body` must return the body promptly
    /// by honoring `Content-Length`, not hang waiting for EOF.
    #[tokio::test]
    async fn read_http_body_ignores_server_keep_alive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req_buf = [0u8; 1024];
            let _ = sock.read(&mut req_buf).await;
            let body = "<response><rsrp>-95dBm</rsrp></response>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nConnection: Keep-Alive\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            // Never close the socket — this is what real HiLink firmware does.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let mut client = tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(addr))
            .await
            .unwrap()
            .unwrap();
        client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();

        let body = tokio::time::timeout(Duration::from_secs(1), read_http_body(&mut client))
            .await
            .expect("read_http_body must not hang on a keep-alive connection")
            .expect("body should be parsed");
        assert_eq!(body, "<response><rsrp>-95dBm</rsrp></response>");
    }
}
