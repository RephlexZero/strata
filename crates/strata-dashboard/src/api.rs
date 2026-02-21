//! HTTP API client for the Strata control plane.
//!
//! All functions use gloo-net to call the REST API with JSON bodies
//! and Bearer token auth. Base URL is relative (same origin).

use crate::types::*;
use gloo_net::http::Request;

/// Ergonomic result alias.
pub type ApiResult<T> = Result<T, String>;

fn auth_header(token: &str) -> String {
    format!("Bearer {token}")
}

/// Parse a non-2xx response into an error string.
async fn parse_error(resp: gloo_net::http::Response) -> String {
    let status = resp.status();
    match resp.json::<ApiErrorResponse>().await {
        Ok(e) => format!("{status}: {}", e.error),
        Err(_) => format!("HTTP {status}"),
    }
}

// ── Auth ────────────────────────────────────────────────────────────

pub async fn login(email: &str, password: &str) -> ApiResult<LoginResponse> {
    let body = LoginRequest {
        email: email.to_string(),
        password: password.to_string(),
    };
    let resp = Request::post("/api/auth/login")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Senders ─────────────────────────────────────────────────────────

pub async fn list_senders(token: &str) -> ApiResult<Vec<SenderSummary>> {
    let resp = Request::get("/api/senders")
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

pub async fn get_sender(token: &str, id: &str) -> ApiResult<SenderDetail> {
    let resp = Request::get(&format!("/api/senders/{id}"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

pub async fn create_sender(token: &str, name: Option<String>) -> ApiResult<CreateSenderResponse> {
    let body = CreateSenderRequest { name };
    let resp = Request::post("/api/senders")
        .header("Authorization", &auth_header(token))
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

pub async fn delete_sender(token: &str, id: &str) -> ApiResult<()> {
    let resp = Request::delete(&format!("/api/senders/{id}"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Streams ─────────────────────────────────────────────────────────

pub async fn list_streams(token: &str) -> ApiResult<Vec<StreamSummary>> {
    let resp = Request::get("/api/streams")
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

pub async fn get_stream(token: &str, id: &str) -> ApiResult<StreamDetail> {
    let resp = Request::get(&format!("/api/streams/{id}"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

pub async fn start_stream(
    token: &str,
    sender_id: &str,
    destination_id: Option<String>,
    source: Option<crate::types::SourceConfig>,
    encoder: Option<crate::types::EncoderConfig>,
) -> ApiResult<StartStreamResponse> {
    let body = StartStreamRequest {
        destination_id,
        source,
        encoder,
    };
    let resp = Request::post(&format!("/api/streams/start/{sender_id}"))
        .header("Authorization", &auth_header(token))
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

pub async fn stop_stream(token: &str, sender_id: &str) -> ApiResult<()> {
    let resp = Request::post(&format!("/api/streams/stop/{sender_id}"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Destinations ────────────────────────────────────────────────────

pub async fn list_destinations(token: &str) -> ApiResult<Vec<DestinationSummary>> {
    let resp = Request::get("/api/destinations")
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

pub async fn create_destination(
    token: &str,
    platform: &str,
    name: &str,
    url: &str,
    stream_key: Option<String>,
) -> ApiResult<CreateDestinationResponse> {
    let body = CreateDestinationRequest {
        platform: platform.to_string(),
        name: name.to_string(),
        url: url.to_string(),
        stream_key,
    };
    let resp = Request::post("/api/destinations")
        .header("Authorization", &auth_header(token))
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

pub async fn delete_destination(token: &str, id: &str) -> ApiResult<()> {
    let resp = Request::delete(&format!("/api/destinations/{id}"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Sender Management ───────────────────────────────────────────────

/// Get full sender status (hardware, network interfaces, system stats).
pub async fn get_sender_status(token: &str, id: &str) -> ApiResult<SenderFullStatus> {
    let resp = Request::get(&format!("/api/senders/{id}/status"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Unenroll a sender — resets its enrollment and issues a new token.
pub async fn unenroll_sender(token: &str, id: &str) -> ApiResult<UnenrollResponse> {
    let resp = Request::post(&format!("/api/senders/{id}/unenroll"))
        .header("Authorization", &auth_header(token))
        .header("Content-Type", "application/json")
        .body("{}")
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Enable a network interface on a sender.
pub async fn enable_interface(token: &str, sender_id: &str, iface: &str) -> ApiResult<()> {
    let resp = Request::post(&format!(
        "/api/senders/{sender_id}/interfaces/{iface}/enable"
    ))
    .header("Authorization", &auth_header(token))
    .header("Content-Type", "application/json")
    .body("{}")
    .map_err(|e| e.to_string())?
    .send()
    .await
    .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Disable a network interface on a sender.
pub async fn disable_interface(token: &str, sender_id: &str, iface: &str) -> ApiResult<()> {
    let resp = Request::post(&format!(
        "/api/senders/{sender_id}/interfaces/{iface}/disable"
    ))
    .header("Authorization", &auth_header(token))
    .header("Content-Type", "application/json")
    .body("{}")
    .map_err(|e| e.to_string())?
    .send()
    .await
    .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Lock a cellular interface to a specific band.
pub async fn lock_band(
    token: &str,
    sender_id: &str,
    iface: &str,
    band: Option<String>,
) -> ApiResult<()> {
    #[derive(serde::Serialize)]
    struct Body {
        band: Option<String>,
    }
    let resp = Request::post(&format!(
        "/api/senders/{sender_id}/interfaces/{iface}/lock_band"
    ))
    .header("Authorization", &auth_header(token))
    .json(&Body { band })
    .map_err(|e| e.to_string())?
    .send()
    .await
    .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Set priority for a network interface.
pub async fn set_priority(
    token: &str,
    sender_id: &str,
    iface: &str,
    priority: u32,
) -> ApiResult<()> {
    #[derive(serde::Serialize)]
    struct Body {
        priority: u32,
    }
    let resp = Request::post(&format!(
        "/api/senders/{sender_id}/interfaces/{iface}/priority"
    ))
    .header("Authorization", &auth_header(token))
    .json(&Body { priority })
    .map_err(|e| e.to_string())?
    .send()
    .await
    .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Set APN and SIM settings for a cellular interface.
pub async fn set_apn(
    token: &str,
    sender_id: &str,
    iface: &str,
    apn: Option<String>,
    sim_pin: Option<String>,
    roaming: Option<bool>,
) -> ApiResult<()> {
    #[derive(serde::Serialize)]
    struct Body {
        apn: Option<String>,
        sim_pin: Option<String>,
        roaming: Option<bool>,
    }
    let resp = Request::post(&format!("/api/senders/{sender_id}/interfaces/{iface}/apn"))
        .header("Authorization", &auth_header(token))
        .json(&Body {
            apn,
            sim_pin,
            roaming,
        })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Set receiver config on a sender (proxied to agent).
pub async fn set_sender_config(
    token: &str,
    sender_id: &str,
    receiver_url: Option<String>,
) -> ApiResult<crate::types::ConfigSetResponse> {
    #[derive(serde::Serialize)]
    struct Body {
        receiver_url: Option<String>,
    }
    let resp = Request::post(&format!("/api/senders/{sender_id}/config"))
        .header("Authorization", &auth_header(token))
        .json(&Body { receiver_url })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Run a connectivity test on a sender (proxied to agent).
pub async fn run_sender_test(
    token: &str,
    sender_id: &str,
) -> ApiResult<crate::types::TestRunResponse> {
    let resp = Request::post(&format!("/api/senders/{sender_id}/test"))
        .header("Authorization", &auth_header(token))
        .header("Content-Type", "application/json")
        .body("{}")
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Scan for new interfaces on a sender (proxied to agent).
pub async fn scan_sender_interfaces(
    token: &str,
    sender_id: &str,
) -> ApiResult<crate::types::InterfaceScanResponse> {
    let resp = Request::post(&format!("/api/senders/{sender_id}/interfaces/scan"))
        .header("Authorization", &auth_header(token))
        .header("Content-Type", "application/json")
        .body("{}")
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Stream Config (Hot Reconfig) ────────────────────────────────────

/// Update encoder / scheduler config on a live stream.
pub async fn update_stream_config(
    token: &str,
    sender_id: &str,
    body: &crate::types::StreamConfigUpdateRequest,
) -> ApiResult<()> {
    let resp = Request::post(&format!("/api/senders/{sender_id}/stream/config"))
        .header("Authorization", &auth_header(token))
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Switch the active video source on a running pipeline.
pub async fn switch_source(
    token: &str,
    sender_id: &str,
    body: &crate::types::SourceSwitchRequest,
) -> ApiResult<()> {
    let resp = Request::post(&format!("/api/senders/{sender_id}/source"))
        .header("Authorization", &auth_header(token))
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

/// List files on a sender device at the given path.
pub async fn list_files(
    token: &str,
    sender_id: &str,
    path: Option<&str>,
) -> ApiResult<crate::types::FileBrowserResponse> {
    let url = match path {
        Some(p) => format!(
            "/api/senders/{sender_id}/files?path={}",
            js_sys::encode_uri_component(p)
        ),
        None => format!("/api/senders/{sender_id}/files"),
    };
    let resp = Request::get(&url)
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json::<crate::types::FileBrowserResponse>()
            .await
            .map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Power Controls ──────────────────────────────────────────────────

/// Send a power command (reboot, shutdown, restart_agent) to a sender.
pub async fn power_command(token: &str, sender_id: &str, action: &str) -> ApiResult<()> {
    #[derive(serde::Serialize)]
    struct Body<'a> {
        action: &'a str,
    }
    let resp = Request::post(&format!("/api/senders/{sender_id}/power"))
        .header("Authorization", &auth_header(token))
        .json(&Body { action })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Configuration Export/Import ─────────────────────────────────────

/// Export the sender's full configuration as JSON.
pub async fn export_config(token: &str, sender_id: &str) -> ApiResult<serde_json::Value> {
    let resp = Request::get(&format!("/api/senders/{sender_id}/config/export"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Import a configuration JSON to a sender.
pub async fn import_config(
    token: &str,
    sender_id: &str,
    config: &serde_json::Value,
) -> ApiResult<()> {
    let resp = Request::post(&format!("/api/senders/{sender_id}/config/import"))
        .header("Authorization", &auth_header(token))
        .json(config)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Set data cap for a cellular interface.
pub async fn set_data_cap(
    token: &str,
    sender_id: &str,
    iface: &str,
    cap_mb: Option<u64>,
) -> ApiResult<()> {
    #[derive(serde::Serialize)]
    struct Body {
        data_cap_mb: Option<u64>,
    }
    let resp = Request::post(&format!(
        "/api/senders/{sender_id}/interfaces/{iface}/data_cap"
    ))
    .header("Authorization", &auth_header(token))
    .json(&Body {
        data_cap_mb: cap_mb,
    })
    .map_err(|e| e.to_string())?
    .send()
    .await
    .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Multi-Destination Routing ───────────────────────────────────────

/// Set active destinations for a live stream (fan-out).
pub async fn set_stream_destinations(
    token: &str,
    sender_id: &str,
    destination_ids: &[String],
) -> ApiResult<()> {
    #[derive(serde::Serialize)]
    struct Body<'a> {
        destination_ids: &'a [String],
    }
    let resp = Request::post(&format!("/api/senders/{sender_id}/stream/destinations"))
        .header("Authorization", &auth_header(token))
        .json(&Body { destination_ids })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Receiver Jitter Buffer ──────────────────────────────────────────

/// Configure the receiver jitter buffer.
pub async fn set_jitter_buffer(
    token: &str,
    sender_id: &str,
    mode: &str,
    static_ms: Option<u32>,
) -> ApiResult<()> {
    #[derive(serde::Serialize)]
    struct Body<'a> {
        mode: &'a str,
        static_ms: Option<u32>,
    }
    let resp = Request::post(&format!("/api/senders/{sender_id}/stream/jitter_buffer"))
        .header("Authorization", &auth_header(token))
        .json(&Body { mode, static_ms })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── OTA Updates ─────────────────────────────────────────────────────

/// Check for available firmware/software updates.
pub async fn check_updates(token: &str, sender_id: &str) -> ApiResult<crate::types::UpdateInfo> {
    let resp = Request::get(&format!("/api/senders/{sender_id}/updates/check"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Trigger an OTA update on a sender.
pub async fn trigger_update(token: &str, sender_id: &str) -> ApiResult<()> {
    let resp = Request::post(&format!("/api/senders/{sender_id}/updates/install"))
        .header("Authorization", &auth_header(token))
        .header("Content-Type", "application/json")
        .body("{}")
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Diagnostics ─────────────────────────────────────────────────────

/// Fetch live logs from the sender device.
pub async fn get_logs(
    token: &str,
    sender_id: &str,
    service: Option<&str>,
    lines: Option<u32>,
) -> ApiResult<crate::types::LogsResponse> {
    let mut url = format!("/api/senders/{sender_id}/logs?");
    if let Some(svc) = service {
        url.push_str(&format!("service={svc}&"));
    }
    if let Some(n) = lines {
        url.push_str(&format!("lines={n}&"));
    }
    let resp = Request::get(&url)
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Run a network tool (ping, traceroute, speedtest) on the sender.
pub async fn run_network_tool(
    token: &str,
    sender_id: &str,
    tool: &str,
    target: Option<&str>,
) -> ApiResult<crate::types::NetworkToolResult> {
    #[derive(serde::Serialize)]
    struct Body<'a> {
        tool: &'a str,
        target: Option<&'a str>,
    }
    let resp = Request::post(&format!("/api/senders/{sender_id}/diagnostics/network"))
        .header("Authorization", &auth_header(token))
        .json(&Body { tool, target })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Trigger a PCAP capture on the sender and return the download URL.
pub async fn capture_pcap(
    token: &str,
    sender_id: &str,
    duration_secs: u32,
) -> ApiResult<crate::types::PcapResponse> {
    #[derive(serde::Serialize)]
    struct Body {
        duration_secs: u32,
    }
    let resp = Request::post(&format!("/api/senders/{sender_id}/diagnostics/pcap"))
        .header("Authorization", &auth_header(token))
        .json(&Body { duration_secs })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── Alerting Rules ──────────────────────────────────────────────────

/// Get alerting rules for a sender.
pub async fn get_alert_rules(
    token: &str,
    sender_id: &str,
) -> ApiResult<Vec<crate::types::AlertRule>> {
    let resp = Request::get(&format!("/api/senders/{sender_id}/alerts"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Create or update an alerting rule.
pub async fn set_alert_rule(
    token: &str,
    sender_id: &str,
    rule: &crate::types::AlertRule,
) -> ApiResult<()> {
    let resp = Request::post(&format!("/api/senders/{sender_id}/alerts"))
        .header("Authorization", &auth_header(token))
        .json(rule)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Delete an alerting rule.
pub async fn delete_alert_rule(token: &str, sender_id: &str, rule_id: &str) -> ApiResult<()> {
    let resp = Request::delete(&format!("/api/senders/{sender_id}/alerts/{rule_id}"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}

// ── TLS Certificate Management ──────────────────────────────────────

/// Get TLS certificate status for a sender's local portal.
pub async fn get_tls_status(token: &str, sender_id: &str) -> ApiResult<crate::types::TlsStatus> {
    let resp = Request::get(&format!("/api/senders/{sender_id}/tls"))
        .header("Authorization", &auth_header(token))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(parse_error(resp).await)
    }
}

/// Generate or renew a self-signed TLS certificate.
pub async fn renew_tls_cert(token: &str, sender_id: &str) -> ApiResult<()> {
    let resp = Request::post(&format!("/api/senders/{sender_id}/tls/renew"))
        .header("Authorization", &auth_header(token))
        .header("Content-Type", "application/json")
        .body("{}")
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        Ok(())
    } else {
        Err(parse_error(resp).await)
    }
}
