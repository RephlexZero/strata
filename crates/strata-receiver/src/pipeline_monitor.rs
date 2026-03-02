//! Pipeline child process monitor for the receiver daemon.
//!
//! Polls all running receiver pipelines every 500ms. If any exit
//! unexpectedly, sends `receiver.stream.ended` to the control plane
//! and releases the allocated ports.

use std::sync::Arc;
use std::time::Duration;

use strata_common::protocol::{Envelope, ReceiverStreamEndedPayload, StreamEndReason};

use crate::ReceiverState;

pub async fn run(state: Arc<ReceiverState>) {
    let mut interval = tokio::time::interval(Duration::from_millis(500));

    loop {
        interval.tick().await;

        if *state.shutdown.borrow() {
            return;
        }

        let exits = {
            let mut pipelines = state.pipelines.lock().await;
            pipelines.check_exits()
        };

        for exit_info in exits {
            let exit_code = exit_info.exit_status.code();
            let reason = match exit_code {
                Some(0) => StreamEndReason::UserStop,
                _ => StreamEndReason::PipelineCrash,
            };

            tracing::warn!(
                stream_id = %exit_info.stream_id,
                exit_code = ?exit_code,
                duration_s = exit_info.duration_s,
                "receiver pipeline exited unexpectedly"
            );

            // Release ports back to pool
            {
                let mut pool = state.port_pool.lock().await;
                pool.release(&exit_info.bind_ports);
            }

            // Remove from stats cache
            {
                let mut stats = state.latest_stats.write().await;
                stats.remove(&exit_info.stream_id);
            }

            let ended = ReceiverStreamEndedPayload {
                stream_id: exit_info.stream_id,
                reason,
                duration_s: exit_info.duration_s,
                total_bytes: exit_info.total_bytes,
            };

            let envelope = Envelope::new("receiver.stream.ended", &ended);
            if let Ok(json) = serde_json::to_string(&envelope) {
                let _ = state.control_tx.send(json).await;
            }
        }
    }
}
