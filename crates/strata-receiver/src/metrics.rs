//! Dedicated Prometheus metrics HTTP server for the receiver daemon.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::ReceiverState;

pub async fn run(state: Arc<ReceiverState>, addr: SocketAddr) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(state);

    tracing::info!(%addr, "prometheus metrics server listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics_handler(State(state): State<Arc<ReceiverState>>) -> impl IntoResponse {
    let all_stats = state.latest_stats.read().await;
    let mut body = String::new();

    // Aggregate all stream stats into Prometheus format
    for (stream_id, links) in all_stats.iter() {
        for link in links {
            body.push_str(&format!(
                "strata_receiver_link_rtt_ms{{stream=\"{stream_id}\",link=\"{}\"}} {:.2}\n",
                link.interface, link.rtt_ms
            ));
            body.push_str(&format!(
                "strata_receiver_link_loss_rate{{stream=\"{stream_id}\",link=\"{}\"}} {:.4}\n",
                link.interface, link.loss_rate
            ));
            body.push_str(&format!(
                "strata_receiver_link_observed_bps{{stream=\"{stream_id}\",link=\"{}\"}} {}\n",
                link.interface, link.observed_bps
            ));
        }
    }

    let pipelines = state.pipelines.lock().await;
    body.push_str(&format!(
        "strata_receiver_active_streams {}\n",
        pipelines.active_count()
    ));
    body.push_str(&format!(
        "strata_receiver_max_streams {}\n",
        state.max_streams
    ));

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}
