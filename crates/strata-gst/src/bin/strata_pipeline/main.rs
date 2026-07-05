//! strata-pipeline — the bonded video transport binary.
//!
//! Two modes: `sender` (encode/passthrough and transmit over bonded links)
//! and `receiver` (reassemble, then relay/record/monitor). The heavy lifting
//! lives in the `strata_pipeline/` modules:
//!
//! - `cli`      — clap flag surface (frozen; scripts and daemons depend on it)
//! - `sender`   — sender pipelines, adaptation envelope, stats relay
//! - `receiver` — receiver pipelines, HLS egress watchdog + generation rebuilds
//! - `gate`     — DeliveredStream / monotonic-DTS pad-probe gates
//! - `hotswap`  — control socket, source hot-swap, link toggling
//! - `stats`    — bonding-stats serialization, JSON→TOML, interface resolution
//! - `util`     — plugin registration, mux configuration helpers

use clap::Parser;

mod cli;
mod gate;
mod hotswap;
mod receiver;
mod sender;
mod stats;
mod util;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging for production use.
    // Controlled by RUST_LOG env var (e.g., RUST_LOG=info,strata_bonding=debug).
    strata_bonding::init();

    gst::init()?;

    match cli::Cli::parse().mode {
        cli::Mode::Sender(args) => sender::run_sender(&args),
        cli::Mode::Receiver(args) => receiver::run_receiver(&args),
    }
}
