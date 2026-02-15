//! Strata Control Plane
//!
//! Single binary that runs:
//! - REST API for the web dashboard
//! - WebSocket endpoint for sender agents
//! - WebSocket endpoint for live dashboard updates
//! - Receiver worker process spawner

use std::net::SocketAddr;

use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use strata_control::{api, db, state, ws_agent, ws_dashboard};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Logging ─────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // ── Database ────────────────────────────────────────────────
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://strata@localhost/strata".into());

    let pool = db::connect(&database_url).await?;
    db::migrate(&pool).await?;

    // ── Dev seed data ───────────────────────────────────────────
    if std::env::var("DEV_SEED").is_ok() {
        db::seed_dev_data(&pool).await?;
    }

    // ── JWT context ─────────────────────────────────────────────
    let jwt_seed = std::env::var("JWT_SEED_B64").unwrap_or_else(|_| {
        tracing::warn!(
            "JWT_SEED_B64 not set — generating ephemeral key (tokens won't survive restart)"
        );
        let (_, seed) = strata_common::auth::JwtContext::generate();
        seed
    });
    let jwt = strata_common::auth::JwtContext::from_ed25519_seed(&jwt_seed)
        .map_err(|e| anyhow::anyhow!("invalid JWT seed: {e}"))?;

    // ── Shared state ────────────────────────────────────────────
    let state = state::AppState::new(pool, jwt);

    // ── Router ──────────────────────────────────────────────────
    // Dashboard: serve the trunk-built WASM SPA from a directory.
    // DASHBOARD_DIR defaults to ../strata-dashboard/dist (dev) or /app/dashboard (Docker).
    let dashboard_dir =
        std::env::var("DASHBOARD_DIR").unwrap_or_else(|_| "../strata-dashboard/dist".into());

    let spa_fallback = ServeFile::new(format!("{dashboard_dir}/index.html"));
    let dashboard_service = ServeDir::new(&dashboard_dir).not_found_service(spa_fallback);

    let app = Router::new()
        .nest("/api", api::router())
        .route("/agent/ws", axum::routing::get(ws_agent::handler))
        .route("/ws", axum::routing::get(ws_dashboard::handler))
        .fallback_service(dashboard_service)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state);

    // ── Listen ──────────────────────────────────────────────────
    let addr: SocketAddr = std::env::var("LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:3000".into())
        .parse()?;

    tracing::info!("strata-control listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
