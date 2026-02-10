use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// Monotonically increasing counter for generating unique test resource names.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Check whether we have sufficient privileges (root/sudo) and tools (`ip`)
/// available to create network namespaces. Returns `false` if the test
/// environment cannot support namespace-based impairment tests.
pub fn check_privileges() -> bool {
    match Command::new("ip").arg("netns").output() {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

/// Generates a unique namespace name with the given prefix.
///
/// Combines the prefix, process ID, and an atomic counter to avoid
/// collisions when tests run in parallel. Names are truncated to 15
/// characters to respect the Linux interface name limit.
pub fn unique_ns_name(prefix: &str) -> String {
    let seq = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    // Truncate to 15 chars (Linux netns name limit)
    let name = format!("{}_{:x}_{}", prefix, pid % 0xFFFF, seq);
    if name.len() > 15 {
        name[..15].to_string()
    } else {
        name
    }
}
