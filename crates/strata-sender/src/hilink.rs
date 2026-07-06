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
async fn http_get(host: &str, path: &str, cookie: Option<&str>) -> Option<String> {
    let fut = async {
        let mut stream = TcpStream::connect((host, 80)).await.ok()?;
        let cookie_hdr = cookie
            .map(|c| format!("Cookie: {c}\r\n"))
            .unwrap_or_default();
        let req =
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{cookie_hdr}Connection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::with_capacity(4096);
        stream.read_to_end(&mut buf).await.ok()?;
        let text = String::from_utf8_lossy(&buf);
        let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string())?;
        // Tolerate chunked encoding well enough for tag extraction — the
        // chunk-size lines never contain '<', so extract_tag still works.
        Some(body)
    };
    tokio::time::timeout(HTTP_TIMEOUT, fut).await.ok().flatten()
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
}
