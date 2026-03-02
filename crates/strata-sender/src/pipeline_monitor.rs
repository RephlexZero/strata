//! Pipeline child process monitor.
//!
//! Polls the strata-node child process every 500ms. If it exits
//! unexpectedly (crash, OOM, etc.), sends a `stream.ended` message
//! to the control plane so the dashboard updates immediately.

use std::sync::Arc;
use std::time::Duration;

use strata_common::protocol::{Envelope, StreamEndReason, StreamEndedPayload};

use crate::AgentState;

/// Run the pipeline monitor loop. Checks every 500ms whether the
/// child process has exited unexpectedly.
pub async fn run(state: Arc<AgentState>) {
    let mut interval = tokio::time::interval(Duration::from_millis(500));

    loop {
        interval.tick().await;

        if *state.shutdown.borrow() {
            return;
        }

        let mut pipeline = state.pipeline.lock().await;
        if let Some(exit_info) = pipeline.check_child_exit() {
            let exit_code = exit_info.exit_status.code();
            let reason = match exit_code {
                Some(0) => StreamEndReason::UserStop, // Clean EOS exit
                _ => StreamEndReason::PipelineCrash,
            };

            tracing::warn!(
                stream_id = %exit_info.stream_id,
                exit_code = ?exit_code,
                duration_s = exit_info.duration_s,
                "pipeline process exited unexpectedly"
            );

            let ended = StreamEndedPayload {
                stream_id: exit_info.stream_id,
                reason,
                duration_s: exit_info.duration_s,
                total_bytes: exit_info.total_bytes,
            };

            // Release the lock before sending
            drop(pipeline);

            let envelope = Envelope::new("stream.ended", &ended);
            if let Ok(json) = serde_json::to_string(&envelope) {
                let _ = state.control_tx.send(json).await;
            }
        }
    }
}
