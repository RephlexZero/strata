//! JWT bearer token extraction for authenticated routes.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::state::AppState;

/// Extractor that validates the `Authorization: Bearer <jwt>` header and
/// provides the authenticated user's ID and role.
pub struct AuthUser {
    pub user_id: String,
    #[allow(dead_code)] // Used when RBAC checks are added
    pub role: String,
}

impl<S> FromRequestParts<S> for AuthUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);

        let auth_header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthRejection::Missing)?;

        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or(AuthRejection::Missing)?;

        let claims = app_state
            .jwt()
            .verify_token(token)
            .map_err(|_| AuthRejection::Invalid)?;

        Ok(AuthUser {
            user_id: claims.sub,
            role: claims.role,
        })
    }
}

/// Trait to extract `AppState` from a generic state type.
/// axum provides this pattern for nested state.
pub trait FromRef<T> {
    fn from_ref(input: &T) -> Self;
}

impl FromRef<AppState> for AppState {
    fn from_ref(input: &AppState) -> Self {
        input.clone()
    }
}

pub enum AuthRejection {
    Missing,
    Invalid,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AuthRejection::Missing => (StatusCode::UNAUTHORIZED, "missing authorization header"),
            AuthRejection::Invalid => (StatusCode::UNAUTHORIZED, "invalid or expired token"),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
