use std::process::Command;

/// Gate for the heavyweight netns/netem/sudo integration tests
/// (`tier3_netem`, `three_link_convergence`).
///
/// These spawn root-owned `dummy_node`/`strata-pipeline` processes inside Linux
/// network namespaces via `sudo`. They clean up on normal completion, but if the
/// test harness is interrupted (SIGKILL — e.g. a cancelled `cargo test`, a hung
/// pre-commit hook, or a CI timeout) the `Drop`/reap path never runs and the
/// root-owned children are orphaned to init, surviving long after the run. In a
/// devcontainer with passwordless sudo, `ip netns` succeeds, so they would
/// otherwise run on every plain `cargo test --workspace`.
///
/// They are therefore **opt-in**: set `STRATA_NETEM_TESTS=1` to run them
/// (alongside the privilege/tooling check). Without it the tests skip, so a
/// normal `cargo test --workspace` and the pre-commit hook never start — and
/// never leak — them. To run:
///
/// ```bash
/// STRATA_NETEM_TESTS=1 sudo -E cargo test -p strata-sim -- --nocapture
/// ```
pub fn check_privileges() -> bool {
    if std::env::var_os("STRATA_NETEM_TESTS").is_none() {
        return false;
    }
    match Command::new("ip").arg("netns").output() {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}
