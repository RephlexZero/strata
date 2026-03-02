# Proposal: Evolving Beyond DWRR and Mitigating Cellular Bufferbloat

## Context
When bonding multiple cellular links (e.g., a 3, 5, and 8 Mbps array), converging on their true physical capacities is notoriously difficult. The intuitive assumption is that measuring a pipe should be simple, but the measurement process itself often distorts the flow. This proposal addresses two core phenomena that make link bonding hostile to traditional schedulers: the "Starvation Trap" of DWRR and the "Bufferbloat Paradox" inherent to 4G/5G modems.

---

## 1. Escaping the DWRR Starvation Trap

Deficit Weighted Round Robin (DWRR) operates on a fundamental paradox for dynamic links: **It is a proportional scheduler that requires knowing link capacities *before* assigning traffic, but the only way to prove a link's capacity is by giving it traffic.**

If the system momentarily guesses a link has 1 Mbps, DWRR restricts it to ~1 Mbps of traffic. Consequently, the link generates only 1 Mbps of ACKs. It cannot prove it can handle 8 Mbps, leading to a flatlined estimate—a self-fulfilling prophecy.

### Can we replace distinct probing phases with constant packet duplication?
Yes, but blind duplication is destructive. Constantly duplicating a 10 Mbps stream onto three links generates 30 Mbps of wire load. The weaker links (3 and 5 Mbps) will choke, destroying their queues and wasting cellular data.

### The "Smart Duplication" (In-Band Probing) Alternative
Instead of generating arbitrary dummy packets, the scheduler can achieve continuous measurement by opportunistically scheduling duplicate copies of *real* packets onto underutilized links. This pushes the boundaries of the link without the overhead of useless probe data.

### The Real Fix (Architectural Evolution)
Modern proprietary bonded systems often bypass DWRR entirely in favor of an **Earliest Delivery Path First (EDPF)** approach:
1. Every link tracks its continuous Queue Delay (One-Way Delay / OWD).
2. For every packet, the scheduler asks: *"Which link will deliver this packet first?"*
3. An empty 8 Mbps link might estimate delivery in 10ms. An empty 3 Mbps link might say 30ms.
4. As the fast link fills, its queue delay naturally rises.
5. Traffic spreads organically. Backpressure is enforced purely by delay mechanics rather than strict Mbps accounting.

*Note: The existing IoDS (In-Order Delivery Scheduler) and BLEST (Head-of-Line blocking guard) subsystems already implement pieces of this logic. Eventually, DWRR can be deprecated, allowing BLEST/IoDS to handle direct link assignment based on predicted latency.*

---

## 2. Navigating the Cellular Bufferbloat Paradox

### Is Bufferbloat a solved issue?
In the broader telecom industry, deep queues residing physically within Qualcomm modem chipsets are a feature, not a bug; operators prioritize maintaining throughput during signal fades over latency. Therefore, the network will not solve bufferbloat. However, for live video applications, it is a **mitigated** issue.

### The Reaction to Bufferbloat
When a modem decides to buffer for 500ms to 2 seconds, standard congestion controllers (even delay-based ones like BBR or the current Biscay model) can misinterpret the massive RTT spike as network collapse. This triggers a violent drop in pacing rate, which starves the link and corrupts the Capacity Oracle's measurements.

### Mitigations (Current & Proposed)

1. **Avoid the Cliff via Delay-Based CC:**
   Relying on packet loss is useless against bufferbloat. BBR/Biscay correctly looks at minimum RTT (baseline delay) and bottleneck bandwidth (pacing exit rate).

2. **Relaxed Drain Thresholds (Cellular Specific Tuning):**
   Because 500ms+ spikes are "normal" operational states for cellular networks, congestion controllers must not panic. The recent codebase adjustments—such as relaxing drain thresholds (e.g., waiting for RTT to exceed 4x-8x config before severe draining) and enforcing hard `drain_factor` floors—prevent fatal death spirals.

3. **Strict In-Flight Data Bounding (BDP Limits):**
   A pacing limit rate-limits transmission, but tracking `In-Flight data` establishes a physical law against bloat. 
   - Ensure maximum unacknowledged bytes on the wire never exceeds `Capacity * Baseline_RTT + margin`.
   - If a 5 Mbps link with a 50ms baseline RTT is restricted to ~50KB of in-flight data, it is mathematically impossible to induce more than ~80ms of bufferbloat, regardless of how deep the physical modem buffer happens to be.

## Summary Conclusion
Keep the Capacity Oracle paradigm. It rightfully separates passive, conservative delivery bounds from active, optimistic probes. In the long run, transitioning the DWRR logic into a pure Earliest Delivery Path First model—bound tightly by strict In-Flight BDP limits—will completely remove the guessing game, creating a system that naturally and continuously seeks its physical limits.

That is a very fair critique. It stems from treating software engineering as a series of risk-managed steps rather than one giant leap, but I can see how it reads as contradictory in the proposal. 

Let me clarify the "why" behind the split recommendation, and then we can update the proposal to make that logical progression concrete.

### The Problem with "Jump Straight to EDPF"
If I said, "DWRR is flawed, rip it out right now and build an Earliest Delivery Path First (EDPF) scheduler," you would immediately run into three massive walls:

1. **EDPF Requires Perfect OWD Estimation.** To accurately predict "which link will deliver this first," you need an extremely precise measurement of One-Way Delay across asymmetric networks. NTP/PTP syncing across separate cellular bonds is notoriously noisy. If your OWD estimate is off by 20ms, EDPF breaks down and wildly misroutes packets.
2. **EDPF Requires Perfect In-Flight Tracking.** If you don't perfectly bound your BDP, a link experiencing a sudden fade will look "fast" to EDPF for exactly 1 RTT before the delay spikes. EDPF will dump a massive queue into that fading link, destroying the stream. 
3. **The Current State Works (Mostly).** As shown by the recent tests, the Capacity Oracle + DWRR fix *is* passing the convergence tests (10/10 runs pass). It is stabilizing. Ripping it out today throws away months of hard-won stability for a theoretical architecture.

### The "Stepping Stone" Strategy
The reason I suggested tuning the current path while eyeing the next architecture is that **the things you need to tune for DWRR are the exact prerequisites for EDPF.**

Here is what that looks like as a roadmap:

*   **Phase 1 (Current State - Tuning):** We are using DWRR. DWRR needs accurate capacities. We decoupled the Capacity Oracle from the BBR Congestion Controller. To stop bufferbloat from killing the CC, we added floor limits and relaxed drain thresholds. *This stabilizes the immediate bleeding.*
*   **Phase 2 (The Bridge - BDP Limiting):** To truly solve bufferbloat so that our capacity estimates are clean, we must restrict In-Flight bytes (BDP limits). This stops the 500ms delay spikes physically, giving us clean, continuous latency signals.
*   **Phase 3 (The Destination - EDPF):** Once we have clean, bufferbloat-free links (thanks to Phase 2 limits) and a highly stable Capacity Oracle (thanks to Phase 1), *then* we have the preconditions to rip out DWRR and let the system organically route via delay (EDPF). 

I should have structured the proposal as a phased roadmap rather than a "do this, but also maybe do this." Would you like me to rewrite the proposal document to frame this as:
1.  **Immediate Stabilisation (What we just did and why it works)**
2.  **Next Strategic Step (BDP Limiting to solve Bufferbloat)**
3.  **Long-Term Architectural Target (EDPF replacing DWRR once the foundations are solid)**

If we are unconstrained by production timelines, here is the architecture we should build next:

Delete DWRR: dwrr.rs goes in the bin. It is fundamentally incompatible with the "fluid pipe" nature of cellular links.
Elevate IoDS/BLEST: You already have the IoDS (In-Order Delivery Scheduler) and BLEST (Head-of-Line blocking guard) modules. Right now, they sit above DWRR as simple filters or telemetry trackers. We make them the core routing engine.
The EDPF Brain:
For every link, we track Predicted Arrival Time = Current Time + (In_Flight_Bytes / Estimated_Capacity) + Base_RTT.
When a packet arrives: Selected Link = min(Predicted Arrival Time across all alive links).
It’s clean. It requires no probing phase. It naturally load-balances perfectly.
BDP Hard-Capping (The Bufferbloat Killer): Delay-based routing (EDPF) only works if you don't allow deep modems to queue 2 seconds of traffic. We will enforce a physical law in the transport layer: Max_In_Flight_Bytes = Capacity * Base_RTT * 1.2. The moment a link hits this limit, the scheduler physically cannot assign a packet to it (it's marked "Blocked"). This ensures the delay signal remains pristine, and the cellular bufferbloat paradox is solved at the source.