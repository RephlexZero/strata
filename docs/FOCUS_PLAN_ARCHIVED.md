# Focus Plan: Advanced Bonding Scheduler Implementation

**Objective:** Upgrade `rsristbondsink` from a simple Round-Robin scheduler to a congestion-aware, technically superior **Deficit Weighted Round Robin (DWRR)** system, integrated with an **Active Rate Control** loop.

## Current Technical Debt
1.  **Blind Scheduling:** Scheduler uses placeholder values; ignorant of real network conditions.
2.  **Passive Backpressure:** Currently relies on blocking `push()`, which causes late frame drops and video corruption rather than proactive adaptation.
3.  **Basic RR:** Simple rotation fails to account for packet size variations or exact bandwidth proportions.

## Implementation Strategy

### Phase 1: Real-Time Telemetry (The Eyes)
**Goal:** Bridge C-API stats to Rust structs.

1.  **`librist-sys` Verification:** Ensure `rist_stats` and `rist_stats_callback_set` are available.
2.  **Thread-Safe Stats Container:** 
    - Create `struct LinkStats` with `AtomicU64` / `Mutex` fields: `rtt_ms`, `bandwidth_bps`, `lost_packets`.
3.  **Callback Bridge:** Register `stats_callback` in `librist` to push C-struct data into the Rust `Arc<LinkStats>`.

### Phase 2: Signal Processing (The Brain)
**Goal:** Turn noisy raw stats into stable Weights.

1.  **EWMA Smoothing:**
    - Implement `Ewma<f64>` ($\alpha \approx 0.125$) for RTT and Capacity.
    - **Why:** Raw network stats jitter wildly; smoothing provides stable weights for the DWRR scheduler.
2.  **Link Health Score:**
    - $Weight = \text{SmoothedBandwidth} \times \text{HealthFactor}(Loss, RTT)$
    - If $RTT > Limit$ or $Loss > Threshold$, HealthFactor drops to 0.

### Phase 3: The Scheduler Algorithm (The Hands)
**Goal:** Implement **Deficit Weighted Round Robin (DWRR)** Dispatcher.

*Context:* Unlike standard DWRR (N queues -> 1 Link), we map 1 Queue -> N Links (Weighted Distribution).

1.  **Mechanism:**
    - Each Link has a `credit_balance` (bytes).
    - **Quantum Update:** Periodically (or per packet), add credits to each link proportional to its `Weight` (Estimated Bandwidth).
    - **Dispatch Decision:**
        - Iterate Links (Highest Credit First or RR).
        - If `Link.credits >= Packet.size`:
            - Send Packet.
            - `Link.credits -= Packet.size`.
            - `Link.pending_buffer` count increments.
        - Else: Skip Link (Accumulate credits for next turn).
2.  **Why DWRR?** It strictly enforces bandwidth proportions even with variable packet sizes (vital for bonding 10Mbps vs 2Mbps links accurately).

### Phase 4: Active Rate Control (The Voice)
**Goal:** Proactively tell the Encoder to throttle down before queues fill.

1.  **Global Capacity Estimator:**
    - $TotalSystemCapacity = \sum (Link_i.Capacity \times Link_i.Health)$
2.  **Upstream Signaling:**
    - If $TotalCapacity < CurrentBitrate \times 1.1$ (Headroom):
        - Emit a custom **GStreamer Bus Message** (`type="congestion-control"`).
        - Payload: `{ "recommended-bitrate": u32 }`.
3.  **Application Logic:**
    - Update `integration_node.rs` to listen for this message.
    - Dynamically set `bitrate` property on `x265enc` (or compatible encoder).
    - **Benefit:** Prevents "Buffer bloat" and "I-frame drops" by reducing quality *at the source*.

## Next Steps Plan
1.  [x] **Modify `rist-bonding-core/net/wrapper.rs`:** Implement `stats_callback` to receive `bandwidth` and `rtt`.
2.  [x] **Update `rist-bonding-core/net/link.rs`:** Add `Ewma` structs and `get_weight()` logic.
3.  [x] **Implement `rist-bonding-core/scheduler/dwrr.rs`:** Create the DWRR dispatcher state machine.
4.  [x] **Update `rsristbondsink`:** Wire DWRR into `render()` and implement the "Congestion Control" Bus Message emission.
5.  [x] **Verify:** Run `impaired_e2e` and verify that `integration_node` logs "Adjusting Bitrate..." when links are throttled.
