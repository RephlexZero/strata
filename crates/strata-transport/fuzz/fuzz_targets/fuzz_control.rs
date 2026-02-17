#![no_main]

use libfuzzer_sys::fuzz_target;
use strata_transport::wire::ControlBody;

/// Fuzz every control packet decoder path.
///
/// ControlBody::decode dispatches on subtype byte to:
/// Ack, Nack, FecRepair, LinkReport, BitrateCmd, Ping, Pong, Session.
/// None of these must ever panic on arbitrary input.
fuzz_target!(|data: &[u8]| {
    let _ = ControlBody::decode(&mut &data[..]);
});
