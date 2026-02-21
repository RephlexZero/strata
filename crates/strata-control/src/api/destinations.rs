//! Destination management endpoints.
//!
//! GET    /api/destinations        — list destinations
//! POST   /api/destinations        — add a destination
//! PUT    /api/destinations/:id    — update a destination
//! DELETE /api/destinations/:id    — remove a destination

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use strata_common::ids;

use crate::api::auth::ApiError;
use crate::state::AppState;

use super::auth_extractor::AuthUser;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_destinations).post(create_destination))
        .route("/{id}", put(update_destination).delete(delete_destination))
}

// ── List Destinations ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DestinationSummary {
    pub id: String,
    pub platform: String,
    pub name: String,
    pub url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

async fn list_destinations(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<DestinationSummary>>, ApiError> {
    let rows = sqlx::query_as::<_, (String, String, String, String, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, platform, name, url, created_at FROM destinations WHERE owner_id = $1 ORDER BY created_at DESC",
    )
    .bind(&user.user_id)
    .fetch_all(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    let destinations = rows
        .into_iter()
        .map(|(id, platform, name, url, created_at)| DestinationSummary {
            id,
            platform,
            name,
            url,
            created_at,
        })
        .collect();

    Ok(Json(destinations))
}

// ── Create Destination ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateDestinationRequest {
    pub platform: String,
    pub name: String,
    pub url: String,
    pub stream_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateDestinationResponse {
    pub id: String,
}

async fn create_destination(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateDestinationRequest>,
) -> Result<(StatusCode, Json<CreateDestinationResponse>), ApiError> {
    user.require_role("admin")?;

    let id = ids::destination_id();

    sqlx::query(
        "INSERT INTO destinations (id, owner_id, platform, name, url, stream_key) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&id)
    .bind(&user.user_id)
    .bind(&body.platform)
    .bind(&body.name)
    .bind(&body.url)
    .bind(&body.stream_key)
    .execute(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;

    tracing::info!(destination_id = %id, platform = %body.platform, "destination created");

    Ok((StatusCode::CREATED, Json(CreateDestinationResponse { id })))
}

// ── Update Destination ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UpdateDestinationRequest {
    pub name: Option<String>,
    pub url: Option<String>,
    pub stream_key: Option<String>,
}

async fn update_destination(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    Json(body): Json<UpdateDestinationRequest>,
) -> Result<StatusCode, ApiError> {
    user.require_role("admin")?;

    // Build dynamic UPDATE (only set provided fields)
    let mut sets = Vec::new();
    let mut params: Vec<String> = Vec::new();
    let mut idx = 2; // $1 = id, $2 = owner_id

    if let Some(ref name) = body.name {
        idx += 1;
        sets.push(format!("name = ${idx}"));
        params.push(name.clone());
    }
    if let Some(ref url) = body.url {
        idx += 1;
        sets.push(format!("url = ${idx}"));
        params.push(url.clone());
    }
    if let Some(ref stream_key) = body.stream_key {
        idx += 1;
        sets.push(format!("stream_key = ${idx}"));
        params.push(stream_key.clone());
    }

    if sets.is_empty() {
        return Err(ApiError::bad_request("no fields to update"));
    }

    let sql = format!(
        "UPDATE destinations SET {} WHERE id = $1 AND owner_id = $2",
        sets.join(", ")
    );

    let mut query = sqlx::query(&sql).bind(&id).bind(&user.user_id);
    for param in &params {
        query = query.bind(param);
    }

    let result = query
        .execute(state.pool())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("destination not found"));
    }

    Ok(StatusCode::NO_CONTENT)
}

// ── Delete Destination ──────────────────────────────────────────────

async fn delete_destination(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    user.require_role("admin")?;

    let result = sqlx::query("DELETE FROM destinations WHERE id = $1 AND owner_id = $2")
        .bind(&id)
        .bind(&user.user_id)
        .execute(state.pool())
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("destination not found"));
    }

    tracing::info!(destination_id = %id, "destination deleted");

    Ok(StatusCode::NO_CONTENT)
}
