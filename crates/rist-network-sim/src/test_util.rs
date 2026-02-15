use std::process::Command;

/// Check whether we have sufficient privileges (root/sudo) and tools (`ip`)
/// available to create network namespaces. Returns `false` if the test
/// environment cannot support namespace-based impairment tests.
pub fn check_privileges() -> bool {
    match Command::new("ip").arg("netns").output() {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}
