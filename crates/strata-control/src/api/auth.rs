//! Authentication endpoints.
//!
//! POST /api/auth/register — create a new user
//! POST /api/auth/login    — exchange credentials for a JWT

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use strata_common::auth;
use strata_common::ids;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
}

// ── Register ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub user_id: String,
    pub email: String,
}

async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<RegisterResponse>), ApiError> {
    // Validate
    if body.email.is_empty() || !body.email.contains('@') {
        return Err(ApiError::bad_request("invalid email"));
    }
    if body.password.len() < 8 {
        return Err(ApiError::bad_request(
            "password must be at least 8 characters",
        ));
    }

    // Hash password
    let password_hash =
        auth::hash_password(&body.password).map_err(|e| ApiError::internal(e.to_string()))?;

    let user_id = ids::user_id();

    // Insert
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, role) VALUES ($1, $2, $3, 'operator')",
    )
    .bind(&user_id)
    .bind(&body.email)
    .bind(&password_hash)
    .execute(state.pool())
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") || e.to_string().contains("unique constraint") {
            ApiError::conflict("email already registered")
        } else {
            ApiError::internal(e.to_string())
        }
    })?;

    tracing::info!(user_id = %user_id, email = %body.email, "user registered");

    Ok((
        StatusCode::CREATED,
        Json(RegisterResponse {
            user_id,
            email: body.email,
        }),
    ))
}

// ── Login ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub user_id: String,
    pub role: String,
}

async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    // Look up user
    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, password_hash, role FROM users WHERE email = $1",
    )
    .bind(&body.email)
    .fetch_optional(state.pool())
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
    .ok_or_else(|| ApiError::unauthorized("invalid email or password"))?;

    let (user_id, password_hash, role) = row;

    // Verify password
    let valid = auth::verify_password(&body.password, &password_hash)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if !valid {
        return Err(ApiError::unauthorized("invalid email or password"));
    }

    // Issue JWT
    let now = Utc::now().timestamp();
    let claims = auth::Claims {
        sub: user_id.clone(),
        iss: "strata-control".into(),
        exp: now + 3600, // 1 hour
        iat: now,
        role: role.clone(),
        owner: None,
    };
    let token = state
        .jwt()
        .create_token(&claims)
        .map_err(|e| ApiError::internal(e.to_string()))?;

    tracing::info!(user_id = %user_id, "user logged in");

    Ok(Json(LoginResponse {
        token,
        user_id,
        role,
    }))
}

// ── Error type ──────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg.into(),
        }
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: msg.into(),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}
