The fundamental physical constraint of network measurement is that one cannot definitively measure the capacity of a pipe whilst only trickling data through it. Bottleneck bandwidth (`btl_bw`) estimation inherently requires the queue at the bottleneck to be non-empty.

Your current architecture creates a parasitic feedback loop because the scheduler (DWRR) dictates the traffic load, and the congestion controller (Biscay) incorrectly interprets this application-limited load as the network's maximum physical capacity.

Here is the analysis of how production systems handle this, followed by the specific architectural remedy required for Strata.

### 1. Production Multi-Path Systems (MPTCP, MPQUIC)

Production multi-path systems separate congestion control (which ensures network safety) from packet scheduling (which optimises for application metrics).

* **Coupled Congestion Control:** Standard MPTCP algorithms like LIA (Linked Increases Algorithm) or OLIA couple the congestion windows of subflows to ensure the aggregate multipath flow does not take more capacity than a single-path flow would on the shared bottleneck. They shift traffic *away* from congested paths, but they do not inherently solve the bandwidth estimation problem under partial load.
* **MP-BBR Challenges:** MPTCP implementations using BBR have historically struggled with the exact issue you describe. When a path is under-utilised by the scheduler, BBR's `btl_bw` decays.
* **The Solution:** Production BBR implementations introduce the concept of an **app-limited phase**. If the sender is not putting enough data on the wire to fill the Bandwidth-Delay Product (BDP), the connection is flagged as app-limited. During this phase, `btl_bw` samples are explicitly ignored or used only to update a secondary, non-decaying maximum.

### 2. The Saturation Requirement and Probing

Periodic saturation is standard, but in a multi-path context, it must be managed via traffic shifting rather than aggregate overallocation.

To probe a specific link without violating the overall 14 Mbps video budget, the scheduler must temporarily over-index traffic to the target link while proportionally reducing traffic on the others. However, for low-latency video, inducing latency via macro-level saturation is highly detrimental.

### 3. Decoupling Capacity from Congestion Control

You must decouple the "Capacity Estimate" (the Oracle) from the "Congestion Control Pacing Rate".

* **Congestion Control (Biscay):** Should dictate the *maximum safe rate* a link can currently handle without causing undue queueing or packet loss.
* **Scheduler Oracle (DWRR):** Requires the *potential theoretical capacity* of the link to assign proportional weights.

Feeding the CC's safety limit back into the scheduler's weight allocation when the link is artificially starved is the root cause of your oscillation.

### 4. Inferring Bottleneck Capacity from Partial Load

You cannot reliably infer capacity from a smooth, low-rate stream. However, you can infer it using **Packet Dispersion Techniques** (specifically Packet Pairs or Packet Trains), which do not require sustained saturation.

If you send two packets back-to-back (a micro-burst), they will queue at the bottleneck link. The time gap between them arriving at the receiver ($\Delta T$) is determined by the bottleneck capacity ($C$) and the packet size ($L$):

$$C = \frac{L}{\Delta T}$$

By injecting occasional packet pairs (or short trains of 3-4 packets) into the normal traffic flow and having the receiver feedback the inter-arrival time, you can estimate the physical bottleneck link speed even if the average traffic over a 100ms window is only 2 Mbps.

### 5. The Simplest Architecture Change (The Bolt-on)

To resolve the winner-takes-all oscillation without rewriting the entire CC stack, implement the following bipartite solution:

**Part A: The App-Limited Guard (Biscay Modification)**

1. Track `bytes_in_flight` for each link.
2. Calculate the `target_cwnd` (BDP) as `btl_bw × min_rtt`.
3. If `bytes_in_flight < target_cwnd` (or a similar threshold, e.g., pacing rate queue is often empty), flag the BiscayController as `is_app_limited = true`.
4. **Crucial change:** When `is_app_limited` is true, *do not decay `btl_bw*`. If a delivery rate sample is lower than the current `btl_bw`, discard it. Only accept samples that increase `btl_bw`. This prevents the capacity estimate from collapsing when the DWRR scheduler starves the link.

**Part B: Micro-Burst Probing (Scheduler Modification)**
Instead of relying on the 1.25x `ProbeBw` gain (which requires sustained traffic to be effective), modify the DWRR scheduler to occasionally dispatch a "probe train".

1. Every few seconds, when a link is under-utilised, allow the DWRR scheduler to send 3-4 packets to that link entirely back-to-back (bypassing CC pacing for just those packets, or pacing them at line rate).
2. The resulting ACK delivery rate for that specific micro-burst will provide a true capacity sample.
3. Because `is_app_limited` prevents the baseline from decaying between probes, the DWRR scheduler will maintain stable, proportional credits.

This isolates the scheduling logic from the artificial constraints of partial-load CC pacing whilst maintaining the mathematical integrity of the underlying BBR architecture.

Would you like me to detail the implementation of the `is_app_limited` state logic within your Rust `BiscayController`?