//! Shared application state.

use std::sync::Arc;

use dashmap::DashMap;
use sqlx::PgPool;
use tokio::sync::broadcast;

use strata_common::auth::JwtContext;
use strata_common::protocol::DashboardEvent;

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
    /// Broadcast channel for dashboard WebSocket subscribers.
    pub dashboard_tx: broadcast::Sender<DashboardEvent>,
}

/// Handle to a connected sender agent.
pub struct AgentHandle {
    /// Channel to send control messages to this agent's WebSocket task.
    pub tx: tokio::sync::mpsc::Sender<String>,
    /// The sender's hostname (for display).
    #[allow(dead_code)] // Used when status display is wired up
    pub hostname: Option<String>,
}

impl AppState {
    pub fn new(pool: PgPool, jwt: JwtContext) -> Self {
        let (dashboard_tx, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                pool,
                jwt,
                agents: DashMap::new(),
                dashboard_tx,
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
