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
) -> ApiResult<StartStreamResponse> {
    let body = StartStreamRequest { destination_id };
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

/// Helper: send a JSON POST without a typed body.
pub async fn _post_empty<T: serde::de::DeserializeOwned>(token: &str, url: &str) -> ApiResult<T> {
    let resp = Request::post(url)
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
