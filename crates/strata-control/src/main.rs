//! Strata Control Plane
//!
//! Single binary that runs:
//! - REST API for the web dashboard
//! - WebSocket endpoint for sender agents
//! - WebSocket endpoint for live dashboard updates
//! - Receiver worker process spawner

use std::net::SocketAddr;

use axum::http::{header, Method};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use strata_control::{api, db, state, stream_state, ws_agent, ws_dashboard, ws_receiver};

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

    // ── Stream-state sweeper ────────────────────────────────────
    // Backstop for devices that never reconnect: a WS drop no longer
    // orphan-marks streams, so something must end them when the device is
    // genuinely gone (see stream_state.rs).
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(stream_state::SWEEP_INTERVAL);
            loop {
                tick.tick().await;
                stream_state::sweep(&state).await;
            }
        });
    }

    // ── Router ──────────────────────────────────────────────────
    // Dashboard: serve the trunk-built WASM SPA from a directory.
    // DASHBOARD_DIR defaults to ../strata-dashboard/dist (dev) or /app/dashboard (Docker).
    let dashboard_dir =
        std::env::var("DASHBOARD_DIR").unwrap_or_else(|_| "../strata-dashboard/dist".into());

    let spa_fallback = ServeFile::new(format!("{dashboard_dir}/index.html"));
    let dashboard_service = ServeDir::new(&dashboard_dir).not_found_service(spa_fallback);

    // ── CORS ────────────────────────────────────────────────────
    // The dashboard is served same-origin from this binary, so browsers need
    // no CORS headers in production. CORS_ALLOWED_ORIGINS opts in for
    // cross-origin dev setups (e.g. `trunk serve`): a comma-separated origin
    // list, or "*" for the old permissive posture.
    let cors = match std::env::var("CORS_ALLOWED_ORIGINS") {
        Err(_) => CorsLayer::new(),
        Ok(v) if v.trim() == "*" => {
            tracing::warn!("CORS_ALLOWED_ORIGINS=* — permissive CORS, do not expose to the internet");
            CorsLayer::permissive()
        }
        Ok(v) => {
            let origins: Vec<_> = v
                .split(',')
                .map(str::trim)
                .filter(|o| !o.is_empty())
                .map(|o| o.parse().map_err(|e| anyhow::anyhow!("invalid origin {o:?} in CORS_ALLOWED_ORIGINS: {e}")))
                .collect::<Result<_, _>>()?;
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        }
    };

    let app = Router::new()
        .nest("/api", api::router())
        .route("/metrics", axum::routing::get(api::metrics::handler))
        .route("/agent/ws", axum::routing::get(ws_agent::handler))
        .route("/receiver/ws", axum::routing::get(ws_receiver::handler))
        .route("/ws", axum::routing::get(ws_dashboard::handler))
        .fallback_service(dashboard_service)
        .layer(TraceLayer::new_for_http())
        .layer(cors)
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
