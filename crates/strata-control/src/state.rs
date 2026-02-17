//! Shared application state.

use std::sync::Arc;

use dashmap::{DashMap, DashSet};
use sqlx::PgPool;
use tokio::sync::{broadcast, oneshot};

use strata_common::auth::JwtContext;
use strata_common::protocol::{DashboardEvent, DeviceStatusPayload, StreamStatsPayload};

/// State shared across all request handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pub pool: PgPool,
    pub jwt: JwtContext,
    /// Connected sender agents, keyed by sender_id.
    pub agents: DashMap<String, AgentHandle>,
    /// Cached latest device status per sender, updated on each heartbeat.
    pub device_status: DashMap<String, DeviceStatusPayload>,
    /// Pending request-response calls to agents, keyed by request_id.
    pub pending_requests: DashMap<String, oneshot::Sender<serde_json::Value>>,
    /// Broadcast channel for dashboard WebSocket subscribers.
    pub dashboard_tx: broadcast::Sender<DashboardEvent>,
    /// Streams that have already transitioned to 'live' (avoids repeated
    /// UPDATE queries on every stats tick).
    pub live_streams: DashSet<String>,
    /// Cached latest stream stats per sender, keyed by sender_id.
    /// Updated on each `stream.stats` message from agents.
    pub stream_stats: DashMap<String, StreamStatsPayload>,
}

/// Handle to a connected sender agent.
pub struct AgentHandle {
    /// Channel to send control messages to this agent's WebSocket task.
    pub tx: tokio::sync::mpsc::Sender<String>,
    /// The sender's hostname (for display in future API responses).
    #[allow(dead_code)]
    pub hostname: Option<String>,
}

impl AppState {
    pub fn new(pool: PgPool, jwt: JwtContext) -> Self {
        let (dashboard_tx, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Inner {
                pool,
                jwt,
                agents: DashMap::new(),
                device_status: DashMap::new(),
                pending_requests: DashMap::new(),
                dashboard_tx,
                live_streams: DashSet::new(),
                stream_stats: DashMap::new(),
            }),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.inner.pool
    }

    pub fn jwt(&self) -> &JwtContext {
        &self.inner.jwt
    }

    pub fn agents(&self) -> &DashMap<String, AgentHandle> {
        &self.inner.agents
    }

    /// Get the cached device status map.
    pub fn device_status(&self) -> &DashMap<String, DeviceStatusPayload> {
        &self.inner.device_status
    }

    /// Get the pending requests map (for request-response patterns to agents).
    pub fn pending_requests(&self) -> &DashMap<String, oneshot::Sender<serde_json::Value>> {
        &self.inner.pending_requests
    }

    /// Streams that have already transitioned to 'live'.
    pub fn live_streams(&self) -> &DashSet<String> {
        &self.inner.live_streams
    }

    /// Cached latest stream stats per sender (keyed by sender_id).
    pub fn stream_stats(&self) -> &DashMap<String, StreamStatsPayload> {
        &self.inner.stream_stats
    }

    /// Broadcast a dashboard event to all subscribed browsers.
    pub fn broadcast_dashboard(&self, event: DashboardEvent) {
        // Ignore send errors (no subscribers).
        let _ = self.inner.dashboard_tx.send(event);
    }

    /// Subscribe to dashboard events (returns a receiver).
    pub fn subscribe_dashboard(&self) -> broadcast::Receiver<DashboardEvent> {
        self.inner.dashboard_tx.subscribe()
    }
}
