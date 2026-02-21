//! Dedicated Prometheus metrics HTTP server.
//!
//! When `--metrics_addr` is set, this starts a minimal HTTP server
//! that serves only the `/metrics` endpoint for Prometheus scraping.
//! This is separate from the portal so it can be on a different
//! port/interface (e.g. internal-only).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::AgentState;

/// Start the dedicated metrics server.
pub async fn run(state: Arc<AgentState>, addr: SocketAddr) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(state);

    tracing::info!(%addr, "prometheus metrics server listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics_handler(State(state): State<Arc<AgentState>>) -> impl IntoResponse {
    let links = state.latest_link_stats.read().await;
    let body = strata_common::metrics::render_prometheus(&links);
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}
