//! HTTP API client for the sender agent's local REST API.
//!
//! All requests go to the same origin (agent on :3001).

use crate::types::*;
use gloo_net::http::Request;

pub type ApiResult<T> = Result<T, String>;

// ── Status ──────────────────────────────────────────────────────────

pub async fn get_status() -> ApiResult<DeviceStatus> {
    let resp = Request::get("/api/status")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

// ── Enrollment ──────────────────────────────────────────────────────

pub async fn enroll(token: &str, control_url: Option<String>) -> ApiResult<EnrollResponse> {
    let body = EnrollRequest {
        enrollment_token: token.to_string(),
        control_url,
    };
    let resp = Request::post("/api/enroll")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        let status = resp.status();
        let err = resp.json::<serde_json::Value>().await.ok();
        let msg = err
            .and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from)))
            .unwrap_or_else(|| format!("HTTP {status}"));
        Err(msg)
    }
}

pub async fn unenroll() -> ApiResult<UnenrollResponse> {
    let resp = Request::post("/api/unenroll")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        let status = resp.status();
        let err = resp.json::<serde_json::Value>().await.ok();
        let msg = err
            .and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from)))
            .unwrap_or_else(|| format!("HTTP {status}"));
        Err(msg)
    }
}

// ── Config ──────────────────────────────────────────────────────────

pub async fn get_config() -> ApiResult<ConfigResponse> {
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

pub async fn set_config(
    receiver_url: Option<String>,
    control_url: Option<String>,
) -> ApiResult<ConfigSaveResponse> {
    let body = ConfigUpdate {
        receiver_url,
        control_url,
    };
    let resp = Request::post("/api/config")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

// ── Interface management ────────────────────────────────────────────

pub async fn enable_interface(name: &str) -> ApiResult<InterfaceToggleResponse> {
    let resp = Request::post(&format!("/api/interfaces/{name}/enable"))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

pub async fn disable_interface(name: &str) -> ApiResult<InterfaceToggleResponse> {
    let resp = Request::post(&format!("/api/interfaces/{name}/disable"))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

pub async fn scan_interfaces() -> ApiResult<InterfaceScanResponse> {
    let resp = Request::post("/api/interfaces/scan")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

// ── Connectivity test ───────────────────────────────────────────────

pub async fn run_test() -> ApiResult<TestResult> {
    let resp = Request::get("/api/test")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.ok() {
        resp.json().await.map_err(|e| e.to_string())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}
