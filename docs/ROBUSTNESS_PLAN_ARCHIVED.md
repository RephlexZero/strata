# Robustness Plan: Advanced Bonding & "Race Car" Simulation

**Objective:** Prove system stability under realistic "Race Car" cellular conditions (bursty loss, high jitter, dynamic bandwidth) by upgrading the simulation harness and the bonding scheduler's intelligence.

## 1. Network Simulation Upgrade (`rist-network-sim`)
**Goal:** A highly configurable simulation crate that models complex network artifacts without requiring external scripts.

*   **Advanced Impairment Config:**
    *   Add **Markov/Gilbert-Elliot Loss** support (`tc netem loss gemodel p r 1-h 1-k`). This models "bursty" packet loss (e.g., driving through a tunnel or interference zone).
    *   Add **Corruption** (`tc netem corrupt`).
    *   Add **Reordering** (`tc netem delay ... reorder`).
    *   Add **Duplication** (`tc netem duplicate`).
*   **Dynamic API:**
    *   Ensure `apply_impairment(link_id, config)` can be called repeatedly at runtime to change conditions instantly.

## 2. Quality-Aware Scheduler (`rist-bonding-core`)
**Goal:** Evolve `DWRR` from "Blind Bandwidth" to "Effective Goodput" estimation.

*   **Effective Capacity Calculation:**
    *   Current: `Weight = Bandwidth`
    *   New: `Weight = Bandwidth * QualityFactor`
    *   `QualityFactor` components:
        *   **Loss Penalty:** `(1.0 - loss_rate)^2`. (Quadratic penalty for loss, as partial loss ruins video).
        *   **Jitter Penalty:** `1.0 / (1.0 + jitter_ms / 100.0)`. (High jitter reduces usable bandwidth).
*   **Circuit Breaker:**
    *   If `QualityFactor < Threshold` (e.g., loss > 20%), force `Weight` to 0 to suspend the link.
    *   Implement "Probe Mode": Periodically send a keepalive/probe on suspended links to check for recovery.

## 3. Active Rate Control Verification
**Goal:** Verify the system signals the encoder correctly during chaos.

*   **Logic Check:**
    *   Ensure `calculate_aggregate_capacity()` uses the *new* `EffectiveCapacity`, not raw bandwidth.
    *   If a link becomes "dirty" (high loss), the available bitrate should drop immediately, triggering the `congestion-control` message.

## 4. Test Suite: "The Race Car"
**Goal:** Reproducible, code-defined test scenarios in `tests/robustness.rs`.

*   **Scenario A: "The Tunnel"**
    *   Link 1: 5Mbps -> **100% Loss (Gemodel)** for 2s -> 5Mbps.
    *   Link 2: Stable 2Mbps.
    *   *Assert:* Transmission continues on Link 2. Rate signals drop to ~2Mbps.
*   **Scenario B: "The Chicane" (Multipath Interference)**
    *   Link 1: High Jitter (100ms +- 50ms), 5% Random Loss.
    *   Link 2: Low Jitter, 0% Loss.
    *   *Assert:* Scheduler favors Link 2 heavily despite Link 1 having "Bandwidth".
*   **Scenario C: "Doppler Reordering"**
    *   High reordering rates (25%).
    *   *Assert:* Receiver aggregator reassembles correctly (requires sufficient buffer).

## Next Steps
1.  [ ] **Sim Upgrade:** Update `rist-network-sim/src/impairment.rs` to support `gemodel`, `corrupt`, `reorder`.
2.  [ ] **Scheduler Logic:** Update `rist-bonding-core/src/scheduler/dwrr.rs` and `link.rs` with `EffectiveCapacity` math.
3.  [ ] **Test Harness:** Create `tests/robustness.rs` and implement the scenarios.
