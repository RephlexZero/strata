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
