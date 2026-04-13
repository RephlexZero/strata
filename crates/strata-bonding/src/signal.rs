use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Install a process-wide shutdown handler that flips a shared running flag.
pub fn install_shutdown_flag(running: Arc<AtomicBool>) -> Result<(), ctrlc::Error> {
    ctrlc::set_handler(move || {
        tracing::info!("shutting down...");
        running.store(false, Ordering::Relaxed);
    })
}
