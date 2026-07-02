//! Shared application state.

use std::sync::Arc;

use dashmap::{DashMap, DashSet};
use sqlx::PgPool;
use tokio::sync::{broadcast, oneshot};

use strata_common::auth::JwtContext;
use strata_common::protocol::{
    DashboardEvent, DeviceStatusPayload, ReceiverStatusPayload, StreamStatsPayload,
};

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
    /// Broadcast channel for dashboard WebSocket subscribers, tagged with
    /// the owning user's ID so each subscriber can filter to only their own
    /// resources (see `broadcast_dashboard`/`subscribe_dashboard`).
    pub dashboard_tx: broadcast::Sender<(String, DashboardEvent)>,
    /// Streams that have already transitioned to 'live' (avoids repeated
    /// UPDATE queries on every stats tick).
    pub live_streams: DashSet<String>,
    /// Cached latest stream stats per sender, keyed by sender_id.
    /// Updated on each `stream.stats` message from agents.
    pub stream_stats: DashMap<String, StreamStatsPayload>,
    /// In-memory alerting rules per sender.
    pub alert_rules: DashMap<String, Vec<serde_json::Value>>,
    /// Connected receiver daemons, keyed by receiver_id.
    pub receivers: DashMap<String, ReceiverHandle>,
    /// Cached latest receiver status per receiver, updated on each heartbeat.
    pub receiver_status: DashMap<String, ReceiverStatusPayload>,
}

/// Handle to a connected sender agent.
pub struct AgentHandle {
    /// Channel to send control messages to this agent's WebSocket task.
    pub tx: tokio::sync::mpsc::Sender<String>,
    /// The sender's hostname (for display in future API responses).
    #[allow(dead_code)]
    pub hostname: Option<String>,
}

/// Handle to a connected receiver daemon.
pub struct ReceiverHandle {
    /// Channel to send control messages to this receiver's WebSocket task.
    pub tx: tokio::sync::mpsc::Sender<String>,
    /// The receiver's hostname.
    #[allow(dead_code)]
    pub hostname: Option<String>,
}

/// Capacity of the dashboard broadcast channel. A lagging subscriber just
/// drops old events (`RecvError::Lagged`, handled in `ws_dashboard.rs`) —
/// this bounds how much slack a slow browser gets before that happens.
const DASHBOARD_BROADCAST_CAPACITY: usize = 1024;

impl AppState {
    pub fn new(pool: PgPool, jwt: JwtContext) -> Self {
        let (dashboard_tx, _) = broadcast::channel(DASHBOARD_BROADCAST_CAPACITY);
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
                alert_rules: DashMap::new(),
                receivers: DashMap::new(),
                receiver_status: DashMap::new(),
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

    /// In-memory alert rules per sender.
    pub fn alert_rules(&self) -> &DashMap<String, Vec<serde_json::Value>> {
        &self.inner.alert_rules
    }

    /// Connected receiver daemons.
    pub fn receivers(&self) -> &DashMap<String, ReceiverHandle> {
        &self.inner.receivers
    }

    /// Cached latest receiver status per receiver.
    pub fn receiver_status(&self) -> &DashMap<String, ReceiverStatusPayload> {
        &self.inner.receiver_status
    }

    /// Broadcast a dashboard event to all subscribed browsers, tagged with
    /// the ID of the user who owns the sender/receiver/stream it concerns.
    /// Subscribers filter to their own `owner_id` (see `ws_dashboard.rs`) —
    /// the channel itself is still global, but nothing is delivered across
    /// owners.
    pub fn broadcast_dashboard(&self, owner_id: impl Into<String>, event: DashboardEvent) {
        // Ignore send errors (no subscribers).
        let _ = self.inner.dashboard_tx.send((owner_id.into(), event));
    }

    /// Subscribe to dashboard events (returns a receiver). Each item is
    /// `(owner_id, event)` — the receiver must filter to the connected
    /// user's own `owner_id` before forwarding to the browser.
    pub fn subscribe_dashboard(&self) -> broadcast::Receiver<(String, DashboardEvent)> {
        self.inner.dashboard_tx.subscribe()
    }
}
