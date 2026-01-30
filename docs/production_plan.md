# Production Plan

## Goals
- Deliver stable, observable, and operable RIST bonding for production use.
- Minimize regressions with repeatable tests and deterministic scenarios.
- Provide clear configuration, safe defaults, and upgrade paths.

## Scope
- Core bonding runtime, scheduler, receiver reassembly.
- GStreamer sink integration and CLI integration_node.
- Network simulation harness and impairment scenarios.
- Documentation, deployment guidance, and operational checks.

## Architecture Readiness
- Typed config with validation and defaults.
- Runtime isolation between scheduler and sink threads.
- Receiver reassembly with bounded buffers and aggressive skip policy.
- Scenario-driven impairment testing with seeded determinism.

## Link Lifecycle & Recovery
- Define a link state machine: init → probe → warm → live → degrade → cooldown → reset.
- Explicit thresholds for promote/demote (RTT, loss, jitter, throughput variance).
- Cooldown/hysteresis timers to prevent oscillation on churn.
- Clear behavior on detach/reattach and IP changes (fast probe, gradual ramp).

## Reliability & Testing
- Unit tests for config, reassembly, scenario determinism.
- Integration tests for impaired scenarios and robustness.
- Regression suite with quantitative thresholds (latency p95, loss, reorder, capacity).
- CI: cargo test --workspace + targeted gst tests.

## Observability
- Stats emission over UDP with timestamped JSON.
- Versioned schema with fixed fields and a periodic heartbeat.
- Monotonic timestamps for ordering; wall-clock only for display.
- Track: capacity, loss, jitter, delay, queue depth, penalties.
- Plots for truth vs tracker in impairment runs.

## Deployment & Ops
- Minimal required privileges: netns/tc for sims; none for prod.
- Privilege matrix per component (CAP_NET_ADMIN/CAP_NET_RAW as needed).
- OS hints: link operstate, MTU probing where available.
- Resource budgets: memory caps on buffers, bounded queues.

## Configuration & Compatibility
- Stable TOML schema for config.
- Explicit defaults for bitrate, jitter, penalties, and pacing.
- Deprecation policy with migration notes.
- Compatibility tests for previous config versions.

## Failure Injection & Chaos
- Include at least one chaos scenario: interface flap, IP change, CPU contention, clock jump.
- Validate recovery time and stability after chaos events.

## Performance Budgets
- Define CPU budget per stream and global ceiling.
- Define memory footprint budget for buffering.
- Define target end-to-end latency budget and alert thresholds.

## Release Checklist
- All tests green.
- No warnings in cargo check.
- Docs updated: configuration, usage, troubleshooting.
- Versioned stats schema published.
- Regression thresholds validated.
- Tagged release with changelog and migration notes.

## Operational Playbooks
- Troubleshooting runbook: link instability, oscillation, loss spikes.
- Live tuning knobs: bitrate caps, penalties, cooldown durations.
- Diagnostic steps: compare truth vs tracker, check link state transitions.

## Risks & Mitigations
- Network variability: handled via scenario coverage and penalties.
- Buffer growth: bounded reassembly and skip policy.
- Telemetry drift: cross-check truth overlays in tests.

## Milestones
- M1: Stabilize config + runtime + reassembly (done).
- M2: Scenario-driven impairment tests (done).
- M3: Production docs + release checklist (this doc).

## Related Docs
- Privilege matrix: docs/privileges.md
- Ops playbook: docs/ops_playbook.md
- Regression thresholds: docs/regression_thresholds.md
- Config migration: docs/config_migration.md
- Performance budgets: docs/perf_budgets.md