//! REST API route tree.

pub mod auth;
pub mod auth_extractor;
pub mod destinations;
pub mod senders;
pub mod streams;

use axum::Router;

use crate::state::AppState;

/// Build the `/api` router.
pub fn router() -> Router<AppState> {
    Router::new()
        .nest("/auth", auth::router())
        .nest("/senders", senders::router())
        .nest("/streams", streams::router())
        .nest("/destinations", destinations::router())
}
