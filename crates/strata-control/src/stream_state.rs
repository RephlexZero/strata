//! Single owner of stream state transitions and DB↔reality reconciliation.
//!
//! Every write to `streams.state` in the control plane goes through this
//! module, so the legal transition set exists in exactly one place:
//!
//! ```text
//! (INSERT 'starting') ──→ live ──→ stopping ──→ ended
//!         │                │           │
//!         └───────────────→└──────────→└──→ ended / failed (error paths)
//!         live ←── ended                    (readopt — reconcile only)
//! ```
//!
//! A WebSocket drop is **not** a transition — "unobserved" is not "dead".
//! The media path doesn't touch the control plane, so pipelines keep
//! running through a control restart or a connectivity blip. Streams end
//! only when:
//! - a device reports it (`stream.ended` / `receiver.stream.ended`),
//! - an operator asks (stop + the force-end timeout),
//! - reconciliation establishes the pipeline is really gone (a heartbeat's
//!   `running_streams` doesn't list it, past [`STARTING_GRACE`]), or
//! - the sweeper finds the device unobserved past [`UNOBSERVED_GRACE`].

use chrono::Utc;
use sqlx::PgPool;

use strata_protocol::DashboardEvent;
use strata_protocol::models::StreamState;

use crate::state::AppState;

/// States in which a stream occupies device/receiver resources.
pub const ACTIVE_STATES: [&str; 3] = ["starting", "live", "stopping"];

/// How long a stream may sit in 'starting' before a heartbeat that doesn't
/// list it is believed. The pipeline takes a few seconds to spawn, and a
/// heartbeat can race the stream.start command — a fresh 'starting' row
/// missing from `running_streams` is expected, not evidence of death.
pub const STARTING_GRACE: chrono::Duration = chrono::Duration::seconds(30);

/// How long a device may go unseen before the sweeper ends its active
/// streams. Generous on purpose: reconnect backoff caps at 30 s + jitter,
/// so a healthy daemon is never away much longer than ~35 s.
pub const UNOBSERVED_GRACE: chrono::Duration = chrono::Duration::seconds(90);

/// How long a stream may sit in 'stopping' before the sweeper forces it to
/// 'ended'. Backstops the in-process force-end timer in api/streams.rs,
/// which does not survive a control-plane restart.
pub const STOPPING_GRACE: chrono::Duration = chrono::Duration::seconds(60);

/// Attempt a validated transition. The UPDATE's WHERE clause enforces the
/// legal source states for `to`, so a stale caller can't clobber a newer
/// state (e.g. a force-end timer firing after the stream already ended is
/// a no-op). Returns `Ok(true)` iff the row moved.
///
/// `error` is recorded for terminal transitions the control plane
/// *inferred* rather than observed — [`readopt`] keys off it.
pub async fn transition(
    pool: &PgPool,
    stream_id: &str,
    to: StreamState,
    error: Option<&str>,
) -> sqlx::Result<bool> {
    let allowed_from: &[&str] = match to {
        StreamState::Live => &["starting"],
        StreamState::Stopping => &["starting", "live"],
        StreamState::Ended | StreamState::Failed => &ACTIVE_STATES,
        // 'starting' is only ever created by INSERT; 'idle' is not a DB state.
        StreamState::Starting | StreamState::Idle => &[],
    };
    if allowed_from.is_empty() {
        tracing::error!(stream_id, to = %to, "refusing invalid stream transition target");
        return Ok(false);
    }

    let terminal = matches!(to, StreamState::Ended | StreamState::Failed);
    let result = sqlx::query(
        "UPDATE streams SET state = $1, \
         ended_at = CASE WHEN $2 THEN COALESCE(ended_at, $3) ELSE ended_at END, \
         error_message = COALESCE($4, error_message) \
         WHERE id = $5 AND state = ANY($6)",
    )
    .bind(to.to_string())
    .bind(terminal)
    .bind(Utc::now())
    .bind(error)
    .bind(stream_id)
    .bind(allowed_from)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Force a stream stuck in 'stopping' to 'ended' ("stop timeout"). Stricter
/// than [`transition`] — only 'stopping' is a legal source here, so a stream
/// that properly ended in the meantime is left alone.
pub async fn force_end_stopping(pool: &PgPool, stream_id: &str) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE streams SET state = 'ended', ended_at = $1, error_message = 'stop timeout' \
         WHERE id = $2 AND state = 'stopping'",
    )
    .bind(Utc::now())
    .bind(stream_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Reconcile-only: bring back a stream the control plane gave up on while
/// the device kept running it (WS blip → sweep, control restart, stop
/// timeout that never reached the device). Only ends the control plane
/// *inferred* (`error_message` set) are eligible — an end confirmed by the
/// device or requested by the user is enforced, not resurrected.
pub async fn readopt(pool: &PgPool, stream_id: &str) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "UPDATE streams SET state = 'live', ended_at = NULL, error_message = NULL \
         WHERE id = $1 AND state IN ('ended', 'failed') AND error_message IS NOT NULL",
    )
    .bind(stream_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Reconcile the DB against a sender heartbeat's `running_streams`.
///
/// - DB-active stream the agent is *not* running (past [`STARTING_GRACE`])
///   → ended, attributed.
/// - Agent-running stream the DB gave up on → readopted if the end was
///   inferred; otherwise the recorded intent wins and a `stream.stop` is
///   re-sent (the original stop may have been lost in a WS blip).
pub async fn reconcile_sender(app: &AppState, sender_id: &str, owner_id: &str, running: &[String]) {
    let db_active: Vec<(String, String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT id, state, started_at FROM streams \
         WHERE sender_id = $1 AND state = ANY($2)",
    )
    .bind(sender_id)
    .bind(&ACTIVE_STATES[..])
    .fetch_all(app.pool())
    .await
    .unwrap_or_default();

    let now = Utc::now();

    // DB says active, agent says not running.
    for (stream_id, state, started_at) in &db_active {
        if running.contains(stream_id) {
            continue;
        }
        if state == "starting" && started_at.map(|t| now - t < STARTING_GRACE).unwrap_or(true) {
            continue;
        }
        match transition(
            app.pool(),
            stream_id,
            StreamState::Ended,
            Some("not running on sender (reconciled)"),
        )
        .await
        {
            Ok(true) => {
                app.live_streams().remove(stream_id);
                tracing::warn!(
                    sender_id,
                    stream_id,
                    "reconcile: stream not running on sender — ended"
                );
                app.broadcast_dashboard(
                    owner_id,
                    DashboardEvent::StreamStateChanged {
                        stream_id: stream_id.clone(),
                        sender_id: sender_id.to_string(),
                        state: StreamState::Ended,
                        error: Some("not running on sender".into()),
                    },
                );
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(stream_id, error = %e, "reconcile transition failed"),
        }
    }

    // Agent says running, DB doesn't consider it active.
    for stream_id in running {
        if db_active.iter().any(|(id, _, _)| id == stream_id) {
            continue;
        }
        // Scope to this sender — never act on another sender's stream id.
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT state, error_message FROM streams WHERE id = $1 AND sender_id = $2",
        )
        .bind(stream_id)
        .bind(sender_id)
        .fetch_optional(app.pool())
        .await
        .unwrap_or(None);

        match row {
            Some((state, _)) if ACTIVE_STATES.contains(&state.as_str()) => {}
            Some(_) => match readopt(app.pool(), stream_id).await {
                Ok(true) => {
                    app.live_streams().insert(stream_id.clone());
                    tracing::info!(
                        sender_id,
                        stream_id,
                        "reconcile: readopted stream still running on sender"
                    );
                    app.broadcast_dashboard(
                        owner_id,
                        DashboardEvent::StreamStateChanged {
                            stream_id: stream_id.clone(),
                            sender_id: sender_id.to_string(),
                            state: StreamState::Live,
                            error: None,
                        },
                    );
                }
                Ok(false) => {
                    // Confirmed/user-requested end — enforce it.
                    tracing::warn!(
                        sender_id,
                        stream_id,
                        "reconcile: sender still running a stream that was deliberately ended — re-sending stream.stop"
                    );
                    send_agent_stop(app, sender_id, stream_id).await;
                }
                Err(e) => tracing::warn!(stream_id, error = %e, "reconcile readopt failed"),
            },
            None => {
                tracing::warn!(
                    sender_id,
                    stream_id,
                    "reconcile: sender reports a stream unknown to this control plane — re-sending stream.stop"
                );
                send_agent_stop(app, sender_id, stream_id).await;
            }
        }
    }
}

/// Reconcile the DB against a receiver heartbeat's `running_streams`.
///
/// Ends DB-active streams whose receiver pipeline is gone (the media path
/// is dead even if the sender keeps transmitting — the sender is told to
/// stop too), and stops receiver pipelines for streams the DB says are
/// done. Readoption is sender-driven only: a receiver pipeline without a
/// sending side is not a live stream.
pub async fn reconcile_receiver(
    app: &AppState,
    receiver_id: &str,
    owner_id: &str,
    running: &[String],
) {
    let db_active: Vec<(
        String,
        String,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT id, sender_id, state, started_at FROM streams \
         WHERE receiver_id = $1 AND state = ANY($2)",
    )
    .bind(receiver_id)
    .bind(&ACTIVE_STATES[..])
    .fetch_all(app.pool())
    .await
    .unwrap_or_default();

    let now = Utc::now();

    for (stream_id, sender_id, state, started_at) in &db_active {
        if running.contains(stream_id) {
            continue;
        }
        if state == "starting" && started_at.map(|t| now - t < STARTING_GRACE).unwrap_or(true) {
            continue;
        }
        match transition(
            app.pool(),
            stream_id,
            StreamState::Ended,
            Some("not running on receiver (reconciled)"),
        )
        .await
        {
            Ok(true) => {
                app.live_streams().remove(stream_id);
                tracing::warn!(
                    receiver_id,
                    stream_id,
                    "reconcile: stream not running on receiver — ended"
                );
                // The sender may still be transmitting into a dead socket.
                send_agent_stop(app, sender_id, stream_id).await;
                app.broadcast_dashboard(
                    owner_id,
                    DashboardEvent::StreamStateChanged {
                        stream_id: stream_id.clone(),
                        sender_id: sender_id.clone(),
                        state: StreamState::Ended,
                        error: Some("not running on receiver".into()),
                    },
                );
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(stream_id, error = %e, "reconcile transition failed"),
        }
    }

    for stream_id in running {
        if db_active.iter().any(|(id, _, _, _)| id == stream_id) {
            continue;
        }
        tracing::warn!(
            receiver_id,
            stream_id,
            "reconcile: receiver running a stream the DB considers done — sending receiver.stream.stop"
        );
        send_receiver_stop(app, receiver_id, stream_id).await;
    }
}

/// Periodic backstop for devices that never reconnect: ends active streams
/// whose sender has been unobserved past [`UNOBSERVED_GRACE`], and forces
/// 'stopping' streams older than [`STOPPING_GRACE`] to 'ended' (the
/// in-process force-end timer does not survive a control restart).
pub async fn sweep(app: &AppState) {
    let now = Utc::now();

    let candidates: Vec<(
        String,
        String,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT s.id, s.sender_id, sn.owner_id, sn.last_seen_at \
             FROM streams s JOIN senders sn ON sn.id = s.sender_id \
             WHERE s.state = ANY($1)",
    )
    .bind(&ACTIVE_STATES[..])
    .fetch_all(app.pool())
    .await
    .unwrap_or_default();

    for (stream_id, sender_id, owner_id, last_seen_at) in candidates {
        // A connected agent's heartbeats reconcile this stream — skip it.
        if app.agents().contains_key(&sender_id) {
            continue;
        }
        let unseen_long_enough = last_seen_at
            .map(|t| now - t > UNOBSERVED_GRACE)
            .unwrap_or(true);
        if !unseen_long_enough {
            continue;
        }
        match transition(
            app.pool(),
            &stream_id,
            StreamState::Ended,
            Some("sender unobserved (connection lost)"),
        )
        .await
        {
            Ok(true) => {
                app.live_streams().remove(&stream_id);
                tracing::warn!(
                    sender_id,
                    stream_id,
                    "sweep: sender unobserved past grace — stream ended"
                );
                app.broadcast_dashboard(
                    owner_id,
                    DashboardEvent::StreamStateChanged {
                        stream_id,
                        sender_id,
                        state: StreamState::Ended,
                        error: Some("sender unobserved (connection lost)".into()),
                    },
                );
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(stream_id, error = %e, "sweep transition failed"),
        }
    }

    // Stale 'stopping' rows (in-process force-end timer lost to a restart).
    let stuck: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT s.id, s.sender_id, sn.owner_id \
         FROM streams s JOIN senders sn ON sn.id = s.sender_id \
         WHERE s.state = 'stopping' AND s.started_at < $1",
    )
    .bind(now - STOPPING_GRACE)
    .fetch_all(app.pool())
    .await
    .unwrap_or_default();

    for (stream_id, sender_id, owner_id) in stuck {
        // Only force-end when the device isn't there to confirm — a
        // connected agent will send stream.ended itself.
        if app.agents().contains_key(&sender_id) {
            continue;
        }
        if let Ok(true) = transition(
            app.pool(),
            &stream_id,
            StreamState::Ended,
            Some("stop timeout (swept)"),
        )
        .await
        {
            app.live_streams().remove(&stream_id);
            app.broadcast_dashboard(
                owner_id,
                DashboardEvent::StreamStateChanged {
                    stream_id,
                    sender_id,
                    state: StreamState::Ended,
                    error: Some("stop timeout".into()),
                },
            );
        }
    }
}

/// Interval between [`sweep`] passes.
pub const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

async fn send_agent_stop(app: &AppState, sender_id: &str, stream_id: &str) {
    let Some(agent) = app.agents().get(sender_id) else {
        return;
    };
    let msg = strata_protocol::ControlMessage::StreamStop(strata_protocol::StreamStopPayload {
        stream_id: stream_id.to_string(),
        reason: "reconciliation".into(),
    });
    if let Ok(envelope) = strata_protocol::Envelope::from_message(&msg)
        && let Ok(json) = serde_json::to_string(&envelope)
        && agent.tx.send(json).await.is_err()
    {
        tracing::warn!(
            sender_id,
            stream_id,
            "reconcile stream.stop dropped: agent channel closed"
        );
    }
}

async fn send_receiver_stop(app: &AppState, receiver_id: &str, stream_id: &str) {
    let Some(rcv) = app.receivers().get(receiver_id) else {
        return;
    };
    let msg = strata_protocol::ReceiverControlMessage::StreamStop(
        strata_protocol::ReceiverStreamStopPayload {
            stream_id: stream_id.to_string(),
            reason: "reconciliation".into(),
        },
    );
    if let Ok(envelope) = strata_protocol::Envelope::from_message(&msg)
        && let Ok(json) = serde_json::to_string(&envelope)
        && rcv.tx.send(json).await.is_err()
    {
        tracing::warn!(
            receiver_id,
            stream_id,
            "reconcile receiver.stream.stop dropped: receiver channel closed"
        );
    }
}
