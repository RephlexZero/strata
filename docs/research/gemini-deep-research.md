Multi-Path Bandwidth Estimation and Scheduling for Bonded Cellular Networks: A Comprehensive Architectural Analysis
Introduction and System Overview
The deployment of real-time, latency-sensitive video transport over bonded cellular networks requires the seamless aggregation of multiple heterogeneous communication links. The fundamental objective is to ensure high throughput, bounded latency, and continuous reliability in environments characterized by dynamic capacity fluctuations, cellular handovers, and variable signal strength. The specific architecture under analysis—designated as the Strata transport system—utilizes a Rust-based stack designed to distribute a single continuous video stream across two to six cellular links concurrently.
The Strata architecture is composed of several tightly integrated layers. At the transport layer, each physical cellular link is wrapped in a TransportLink that utilizes a BiscayController. This controller is a custom, single-path congestion control algorithm heavily inspired by Google’s Bottleneck Bandwidth and Round-trip propagation time (BBR) protocol.1 The BiscayController maintains an active estimate of the bottleneck bandwidth (btl_bw) derived from delivery-rate samples taken over a 10-second sliding window, filtered through a 75th-percentile calculation. The physical transmission of packets on each link is governed by a pacing rate defined as the product of the btl_bw, a dynamic gain factor, and a drain factor.3 The controller transitions through a standard state machine, beginning in a calibration SlowStart phase before entering a steady-state ProbeBw phase, where phase-shifted probing allows links to independently probe at a 1.25× gain without overlapping.5
Above the transport layer, a Deficit Weighted Round Robin (DWRR) scheduler performs packet-level multi-path distribution. The scheduler operates on a credit-based system where each link accumulates byte credits at a rate strictly proportional to its effective_bps. In the current design, this effective_bps is directly derived from the BiscayController's btl_bw (specifically, capacity_bps = pacing_rate × 8.0). Packets are deterministically routed to the link possessing the highest current credit balance, with credits capped by a 15ms live burst window to prevent micro-burst queueing delays. An intelligence overlay, the BondingScheduler, further refines this process using a Blocking Estimation-based (BLEST) filter to pre-filter links and prevent Head-of-Line (HoL) blocking based on live Round Trip Time (RTT) and capacity metrics.6
While mathematically elegant, this tightly coupled architecture currently exhibits a critical, systemic instability characterized by a "winner-takes-all" throughput oscillation. This report exhaustively analyzes the root cause of this instability, evaluates the state of the art in production multi-path systems, deconstructs the theoretical limitations of capacity probing, and proposes a decoupled, non-saturating capacity estimation oracle to resolve the architectural deadlock.
The Physics of the Winner-Takes-All Oscillation
The fundamental flaw in the current Strata architecture stems from a conceptual conflation between two distinct network metrics: the allocated traffic rate and the physical link capacity.3
Under a standard single-path BBR deployment, the sender actively attempts to push as much traffic as the network path will sustain.4 Because the application demand typically exceeds or equals the path capacity, the network bottleneck becomes saturated. In this saturated state, the delivery rate measured via TCP or UDP acknowledgment (ACK) feedback accurately reflects the true physical bottleneck rate of the link.5 Consequently, the btl_bw converges to the true tc (traffic control) physical limit.
However, in a multi-path environment managed by a proportional DWRR scheduler, the dynamics are radically altered.8 The total application demand (e.g., a 14 Mbps video stream) is intentionally distributed across an aggregate capacity that exceeds the demand (e.g., a 16 Mbps aggregate capacity spanning three links with tc limits of 3, 5, and 8 Mbps). Because the aggregate capacity exceeds the load, each individual link remains underutilized. A link's measured delivery rate is strictly mathematically bounded by the formula:

Because the traffic sent to the link is less than the physical capacity, the delivery rate simply mirrors the allocated traffic. Therefore, the BiscayController's btl_bw estimate converges on the traffic sent, not the tc capacity.3
This creates an uncontrollable positive feedback loop between the congestion controller and the scheduler 8:
The system initializes, and due to minor network jitter or initial SlowStart calibrations, Link A registers a marginally higher btl_bw than Links B and C.
The DWRR scheduler, reading this higher btl_bw, translates it into a higher effective_bps and grants Link A the majority of the byte credits.
Link A subsequently receives the largest share of the 14 Mbps video traffic.
The BiscayController on Link A measures this massive influx of traffic, sees a high delivery rate, and scales its btl_bw even higher.
The DWRR scheduler reads the newly inflated btl_bw, grants Link A even more credits, and effectively starves Links B and C.
Links B and C, receiving almost no traffic, measure a near-zero delivery rate. Their btl_bw plummets, further reducing their DWRR credits.
This continues until Link A's credit burst window expires, or Link A experiences minor queueing delay that temporarily dips its delivery rate. The scheduler then abruptly pivots to another link, which subsequently hoovers all the traffic and drives its own btl_bw to the maximum.
The observable result is a violent rotation of capacity estimates (e.g., cycling through 4700, 7300, and 11700 kbps) every 1 to 3 seconds. The DWRR scheduler fails to maintain a stable 3:5:8 load distribution, and instead, the time-averaged throughput equalizes to roughly ~3.5 Mbps per link, completely ignoring the heterogeneous tc limits.
Analysis of Failed Mitigations
Understanding why previous mitigation attempts failed provides critical insight into the necessary architectural requirements.
1. Exploration Credit Boost: Temporarily boosting one link's DWRR credits by 3× to force it to probe its capacity fails because it does not escape the feedback loop; it merely manualizes it. Forcing a link to take 3× traffic artificially inflates its btl_bw at the direct expense of the other links. When the boost cycles to the next link, the previous link is starved. Time-averaged throughput equalizes because the boost systematically rotates the starvation phase across all links.
2. Probed Capacity from packets_acked: Attempting to decouple the capacity estimate by counting packets_acked outside the BBR state machine failed due to the scheduler's refresh interval. If a link is currently starved of DWRR credits, no packets are sent; therefore, no ACKs arrive. An ACK counter cannot infer the capacity of an idle link.
3. Calibration Lock-In: Capturing the peak btl_bw during the initial SlowStart phase (when a capacity floor forces equal traffic distribution) correctly identifies the proportional physical capacities (e.g., yielding 4824, 6993, and 11360 kbps, mirroring the 3:5:8 ratio). However, utilizing these locked-in values solely for DWRR credit initialization is insufficient. The live btl_bw within the BiscayController continues to oscillate based on the actual fractional traffic received. Because secondary reactive modifiers (like bw_slope and penalty_factor) and the BLEST HoL blocking filter rely on the oscillating live btl_bw, the intelligence overlay continually miscalculates the physical serialization delay, inducing artificial routing disruptions.6
4. Bypassing Reactive Modifiers: Even if reactive modifiers are disabled, bounding the DWRR credits to calibrated capacities while allowing the live btl_bw to oscillate creates an internal state contradiction. The transport layer's physics guards assert failures (e.g., est_cap > 3× target) because the controller is attempting to pace traffic using an oscillating delivery rate while the scheduler is distributing traffic based on a static historical absolute.
These failures definitively prove that the capacity estimation mechanism cannot be a function of the actively allocated payload traffic.
Production Multi-Path Systems: MPTCP and MPQUIC
To design a robust resolution, it is instructive to examine how production multi-path systems, specifically Multi-Path TCP (MPTCP) and Multi-Path QUIC (MPQUIC), solve the per-path capacity estimation and scheduling problem.12
The Coupled Congestion Control Paradigm
Unlike the Strata architecture, which runs completely independent, uncoupled BBR instances on each link, standard production MPTCP architectures utilize Coupled Congestion Control algorithms.14 The primary directive of MPTCP is "resource pooling"—treating multiple disparate links as a single aggregated resource while ensuring fairness to competing single-path TCP flows.14
If standard, uncoupled TCP Reno or CUBIC flows were run across multiple paths, the MPTCP connection would unfairly consume more bandwidth on shared bottlenecks than a single-path user.14 To counteract this, MPTCP shifts traffic away from congested links and couples the congestion windows (CWND) of all active subflows.
The Linux kernel implementation of MPTCP supports several prominent coupled algorithms 15:

Algorithm
Primary Mechanism
Congestion Signal
LIA (Linked Increase Algorithm)
Couples the CWND increase across all subflows, scaling the increase inversely proportional to the aggregate window size to ensure fairness.
Packet Loss 15
OLIA (Opportunistic LIA)
Improves upon LIA by guaranteeing Pareto optimality. It actively shifts traffic to paths with larger unused capacities while maintaining friendliness.
Packet Loss, Window Size 16
BALIA (Balanced LIA)
Strikes a mathematical balance between responsiveness and friendliness, allowing the protocol to adapt more rapidly to dynamic network changes.
Packet Loss, RTT 15
wVegas (Weighted Vegas)
A delay-based algorithm that utilizes variations in queueing delay to detect congestion before packet loss occurs, distributing traffic based on delay sensitivity.
Delay (RTT fluctuations) 15

Window-Based vs. Rate-Based Scheduling
The critical difference between MPTCP and the Strata architecture lies in how scheduling interacts with congestion control. The standard MPTCP algorithms listed above are window-based. They do not calculate an explicit "capacity estimate" in bits-per-second to feed into a proportional credit scheduler like DWRR.14
Instead, the MPTCP scheduler simply operates as an availability switch. When an application payload arrives, the scheduler inspects the current CWND and the number of packets in flight (inflight) for each subflow. If inflight < CWND for a specific subflow, the path is deemed available, and the packet is pushed to that socket. The paths naturally self-balance because their individual CWNDs expand and contract according to the physical Bandwidth-Delay Product (BDP) of their respective links.4 The scheduler does not need to know if a link is 3 Mbps or 8 Mbps; it only needs to know that the 8 Mbps link has a larger open window and will drain packets faster, thus naturally drawing more traffic.
Because Strata utilizes a rate-based pacing algorithm (BBR) coupled with a rate-based credit scheduler (DWRR), it fundamentally lacks this passive, self-balancing window dynamic.8 The DWRR scheduler explicitly requires a defined capacity_bps to assign credits.
Coupled Multi-Path BBR (C-MPBBR)
Recent academic efforts have attempted to port the BBR protocol to MPTCP, resulting in algorithms like Coupled Multipath BBR (C-MPBBR).18 C-MPBBR builds a network model by sequentially measuring Bottleneck Bandwidth (BtlBW), minimum RTT (minRTT), and Delivery Rate (DelRt).18
C-MPBBR ensures fairness by identifying subflows that share a common bottleneck (inferred when multiple subflows report identical BtlBW estimates) and dynamically dividing the total available bandwidth among them.18 To optimize scheduling, C-MPBBR utilizes a performance threshold. It calculates the highest bandwidth among all subflows and establishes a cutoff (e.g., 40% below the peak). If a subflow's delivery rate falls below this threshold for several successive ProbeBW states, the algorithm classifies the link as non-advantageous and closes it to prevent HoL blocking and queueing degradation.18
However, C-MPBBR still relies on the fundamental premise that the links are frequently saturated by the application load, allowing the ProbeBW phase to accurately determine the physical BtlBW.18 In the Strata video environment, where total demand (14 Mbps) is strictly less than aggregate capacity (16 Mbps), C-MPBBR would suffer from the exact same under-estimation flaw as the BiscayController.
The Saturation Dilemma in Real-Time Video
To resolve the estimation issue, one must evaluate whether "probing each path to saturation periodically" is the standard and necessary approach, and whether it can be accomplished without disrupting real-time video flows.
The Mechanics of BBR's ProbeBW
The BBR algorithm explicitly requires link saturation to discover the physical capacity limits.5 BBR assumes that the delivery rate of a connection will increase linearly with the sending rate until the physical bottleneck capacity is reached. Once the sending rate exceeds the bottleneck capacity, the delivery rate plateaus, and any excess data accumulates in the bottleneck router's queue, manifesting as an increase in RTT.5
To continuously probe for newly available capacity, BBR utilizes a steady-state ProbeBW phase. It cycles through an eight-phase pacing gain sequence: [1.25, 0.75, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0].5 Each phase typically lasts for one RTT.
Upward Probing (1.25× Gain): BBR intentionally paces data 25% faster than the currently estimated btl_bw. If the physical link has unused capacity, the network absorbs the higher rate, the measured delivery rate increases, and the btl_bw estimate scales upward.5
Downward Draining (0.75× Gain): If the link was already at its physical limit, the extra 25% data injected during the upward phase cannot traverse the bottleneck and instead forms a queue, increasing the RTT. The subsequent 0.75× drain phase reduces the pacing rate to allow this queue to dissipate, returning the RTT to the baseline minRTT.7
Cruising (1.0× Gain): BBR paces at exactly the btl_bw for six RTTs to maintain the ideal BDP without creating persistent queues.5
Why Phase-Shifted Probing Fails Under DWRR
In the Strata architecture, phase-shifted probing is implemented so that only one link enters the 1.25× gain phase at a time. However, this entirely fails to discover the physical tc limit because of the overarching DWRR scheduler.
If the 5 Mbps tc link is currently allocated 3 Mbps of traffic by the DWRR scheduler, its btl_bw estimate rests at 3 Mbps. When BBR enters the 1.25× ProbeBW phase, it sets its internal pacing rate to .
Crucially, pacing at 3.75 Mbps on a 5 Mbps link does not cause a queue to form. The network easily absorbs the traffic. Because there is no queue, there is no RTT spike. The BBR instance successfully registers a new delivery rate of 3.75 Mbps and updates its btl_bw accordingly. The DWRR scheduler observes this increase, awards the link more credits, and drives the allocation to 4 Mbps. The next probe hits 5 Mbps. Finally, the next probe hits 6.25 Mbps, which exceeds the 5 Mbps tc limit, causing a queue, an RTT spike, and a subsequent drain.
This illustrates that probing to saturation in a DWRR-controlled multi-path environment is not a clean, instantaneous measurement; it is a gradual, destructive creep that forces the scheduler into chaotic realignments.3
The Latency Cost for Real-Time Video
Furthermore, pushing a cellular link to complete saturation is inherently antagonistic to the requirements of real-time video.22 Saturation explicitly means pushing data faster than the link can serialize it, forcing the data into the base station's transmission buffer (bufferbloat).24
For bulk file transfers (like standard TCP or QUIC downloads), this temporary queueing is acceptable. For low-latency live video streaming, an RTT spike induced by intentional saturation can cause frames to miss their decoder presentation timestamps, resulting in visible stuttering, frame drops, and application-level failure.22 Therefore, relying on periodic physical saturation to estimate capacity is a fundamental anti-pattern for bonded cellular live streaming. The architecture requires a mechanism to "see" the ceiling without hitting it.26
Decoupling Capacity Estimation from Congestion Control Pacing
The failure of saturation probing leads to the core architectural question: Should the capacity estimate be decoupled from congestion control pacing?
The research strongly dictates that decoupling is strictly necessary in this paradigm.14 The current architecture suffers because it relies on a single metric (btl_bw) to perform two diametrically opposed functions:
Congestion Control (Pacing): Regulating the flow of data to prevent overwhelming the local hardware or the network pipe.
Scheduler Allocation (Credits): Determining the theoretical maximum proportion of traffic a link could handle if demanded.
To stabilize the system, these functions must be mathematically separated by differentiating between two specific network properties:
Available Physical Capacity (): The maximum theoretical bit rate that the cellular modem and network infrastructure can currently negotiate and transmit (e.g., the exact 3, 5, or 8 Mbps tc limits).29
Attainable Delivery Rate (): The actual throughput currently flowing over the link, as dictated by the application encoder rate and the scheduler's proportional distribution.10
In a decoupled architecture, the BBR-based BiscayController should exclusively manage the Attainable Delivery Rate (). Its sole responsibility is to pace the fractional traffic it is given by the DWRR scheduler smoothly onto the wire, maintaining a minimal BDP and low latency.28 If DWRR gives it 2.6 Mbps, BBR should pace at 2.6 Mbps and report a btl_bw of 2.6 Mbps. This is safe, correct behavior for a congestion controller.
Simultaneously, a completely independent Capacity Oracle must be implemented alongside the transport layer.13 This Oracle utilizes specialized, non-saturating algorithms to infer the Available Physical Capacity (). The Oracle exclusively feeds the  values to the DWRR scheduler for static credit allocation and to the BLEST intelligence overlay for accurate HoL blocking calculations.6
When DWRR relies on the stable  values, the credits lock into a perfect 3:5:8 ratio. The 14 Mbps video demand is proportionally split regardless of minor fluctuations in application bitrate, completely eliminating the positive feedback loop.8
Algorithms for Inferring Bottleneck Capacity Without Saturation
To build the decoupled Capacity Oracle, the system requires an established algorithm capable of inferring the bottleneck capacity from partial-load observations. The most robust and mathematically sound methodology for this is Packet Dispersion—specifically Packet Pair Dispersion (PPD) and Packet Train Dispersion (PTD).29
The Mathematics of Packet Pair Dispersion (PPD)
The Packet Pair technique estimates the capacity of a bottleneck link by analyzing the temporal spacing (dispersion) between two packets sent back-to-back, relying on the physical serialization delay induced by routers rather than queueing delay caused by saturation.29
Let the sender (the Strata transport layer) transmit two equal-sized probe packets of length  bits. These packets are injected into the network interface back-to-back. The initial time dispersion between them at the sender, , is dictated solely by the local hardware transmission rate and the bus speed.
As these packets traverse a network path consisting of  hops, they encounter various link capacities. Let the physical capacity of the -th hop be . The transmission delay (the time it takes to place the physical bits onto the wire) of a packet at hop  is defined as .33
Assuming an ideal scenario with no cross-traffic, the first probe packet arrives at the bottleneck link (defined as the link with the absolute minimum capacity along the path, ) and begins transmission. Because the bottleneck link is the slowest link in the path, it requires more time to serialize the packet than any of the preceding links.
Consequently, when the second probe packet arrives at the bottleneck router, the first packet is still being serialized. The second packet must wait in the router's hardware queue.29 As a result, the two packets exit the bottleneck link separated by exactly the transmission time of the first packet on that specific link. The dispersion immediately after the bottleneck becomes:

Because subsequent links in the path have higher capacities (by definition, since they are not the bottleneck), they serialize the packets faster than the bottleneck did. Therefore, the packets do not queue up behind each other on the remaining hops. The temporal spacing  is preserved all the way to the receiver.
The receiver records the arrival time of the packets and generates ACKs. When the ACKs return to the sender, the sender calculates the final output dispersion, . The bottleneck capacity can then be inferred instantly without ever saturating the link's overall bandwidth 29:

Handling Cross-Traffic and the Multimodal Distribution
In pristine, isolated simulation environments (such as basic tc limits without background noise), the equation  holds perfectly true. However, in live cellular networks, cross-traffic and the "Fluid Spray Effect" introduce significant measurement noise that must be filtered.33
If cross-traffic packets generated by other users in the cellular cell arrive at the bottleneck router in the infinitesimally small temporal window between the arrival of the first and second probe packets, the cross-traffic will be inserted between them.35 This increases the queueing delay of the second packet () relative to the first (), expanding the dispersion:

This expanded dispersion causes the basic mathematical formula to underestimate the physical capacity.29
Conversely, if cross-traffic delays the first packet at a post-bottleneck link, but clears just before the second packet arrives, the second packet catches up to the first. The packets are compressed together, leading to a diminished  and an overestimation of the capacity.29
Because of these complex queueing dynamics, empirical research demonstrates that packet pair dispersion in loaded paths does not yield a single constant value, but rather follows a multimodal distribution.29

Dispersion Scenario
Impact on Δout​
Resulting Capacity Estimate
No Cross-Traffic Interference

True Physical Capacity 29
Cross-Traffic Insertion at Bottleneck

Underestimation (Reflects Available BW) 33
Post-Bottleneck Compression

Overestimation 29

The true path physical capacity () is a distinct local mode within this distribution, though depending on the severity of the cross-traffic, it is not always the global mode.29
State-of-the-Art Packet Dispersion Tools
Network measurement science has developed several sophisticated algorithms to filter this multimodal distribution and extract the true capacity without intrusion:
Pathrate: Analyzes the multimodal distribution of both packet pair and longer packet train dispersion (PTD) to identify the specific local mode that corresponds to the physical capacity, distinguishing it from the Asymptotic Dispersion Rate (ADR) which represents available bandwidth.29
Spruce: Sends pairs of packets spaced exactly at the theoretical bottleneck transmission time. By measuring the expansion of the gap, it calculates the volume of cross-traffic without saturating the link.35
PathChirp: Uses exponentially spaced packet trains (chirps). It identifies regions where the inter-arrival times show a monotonically increasing trend to mathematically deduce the bottleneck without requiring a sustained saturation phase.36
bTrack: Exploits the quasi-invariant characteristic of the relative distance between input and output gaps of packet pairs of differing sizes. This allows for accurate estimation of available bandwidth under highly bursty cross-traffic conditions.38
For the Rust-based Strata architecture, implementing a sender-side logic that periodically forces two Maximum Transmission Unit (MTU) sized video packets to be flushed to the socket back-to-back—temporarily bypassing the BBR pacing interval for just those two packets—is the most computationally efficient method to sample  via PPD.29
Cross-Layer Cellular Telemetry
While packet dispersion provides an excellent, mathematically sound transport-layer inference of link capacity, bonded cellular links (LTE/5G) possess highly volatile physical layers subject to rapid fading, signal degradation, and handovers.40 Relying solely on ACKs arriving from the receiver means the capacity estimate is fundamentally delayed by at least one RTT.
Production bonded cellular hardware systems, such as Dejero's Smart Blending Technology and LiveU's Reliable Transport, achieve superior reliability because they do not rely exclusively on transport-layer ACKs. They ingest physical layer telemetry directly from the cellular modems to preemptively inform their scheduling algorithms.43
RSRP, RSRQ, and SINR Mapping
Cellular modems expose critical Radio Resource Control (RRC) telemetry metrics that correlate directly with the maximum achievable bandwidth of the physical air interface 46:
RSSI (Received Signal Strength Indicator): The total wideband power received by the modem, encompassing the desired signal, all adjacent cell interference, and thermal noise. RSSI alone is a poor indicator of capacity.49
RSRP (Reference Signal Received Power): The linear average of the power contributions of the specific resource elements that carry cell-specific reference signals. RSRP isolates the power of the desired signal from the background noise, providing an accurate indication of cell proximity and base radio link quality.47
RSRQ (Reference Signal Received Quality): A calculated metric that combines both signal strength and interference into a single ratio. It is strictly defined as:

where  is the number of Physical Resource Blocks (PRBs) over which the RSSI is measured (typically equal to the system bandwidth).47
SINR (Signal to Interference plus Noise Ratio): The ratio of the desired signal power to the sum of the power of all interfering signals and noise. This is the ultimate determinant of the link's ability to decode complex symbols.46
In LTE and 5G networks, the eNodeB/gNodeB base station dynamically dictates the physical capacity of the user's link by assigning a Modulation and Coding Scheme (MCS) based on the user equipment's reported RSRQ and SINR.41 The MCS dictates the constellation density (e.g., QPSK, 16-QAM, 64-QAM, 256-QAM), which directly governs how many bits can be transmitted per physical symbol.42

RSRQ Range
Signal Quality Indicator
Typical Modulation (MCS)
Predicted Capacity Impact
 dB
Excellent
64-QAM to 256-QAM
Maximum theoretical PRB allocation. Highest link capacity.47
 to  dB
Good
16-QAM to 64-QAM
Moderate data speeds. Stable capacity.47
 to  dB
Fair to Poor
QPSK
Marginal speeds. High probability of MAC-layer retransmissions (HARQ) dropping throughput severely.47

By extracting RSRQ and SINR via AT commands or ModemManager APIs directly from the Linux OS, the Strata architecture can establish a predictive upper-bound baseline for the Capacity Oracle. If the RSRQ on a link abruptly drops from -9 dB to -18 dB (e.g., due to the user entering a tunnel), the Oracle instantly knows the physical capacity has critically degraded.40 This allows the DWRR scheduler to revoke credits before the transport layer begins to experience packet loss or severe RTT spikes, effectively preventing HoL blocking before it occurs.
Sensor Fusion and Stability via Extended Kalman Filtering
Because both Packet Pair Dispersion (which is subject to high-frequency cross-traffic noise) and cellular telemetry (which is subject to rapid physical multipath fading) are inherently noisy signals, feeding raw, instantaneous samples directly into the DWRR scheduler will reintroduce instability.53 The capacity estimate will oscillate chaotically, destroying the stability of the credit allocation.
To provide the DWRR scheduler and BLEST filter with a stable, highly accurate, and responsive capacity estimate, the Capacity Oracle must fuse these inputs and filter them using an Extended Kalman Filter (EKF).53
The Kalman Filter is a recursive algorithm that estimates the internal state of a linear dynamic system from a series of noisy measurements. It operates in two distinct, continuous phases: predict and update.54
1. Prediction (Time Update) Phase
The filter first projects the current state estimate forward in time.


 is the a priori state estimate (the predicted true physical capacity, ).
 is the estimate error covariance (the calculated uncertainty of the capacity estimate).
 is the state transition model. For capacity, which generally remains stable unless a physical event occurs,  can be approximated as the identity matrix .
 is the process noise covariance matrix. This represents the variance in the actual physical state.55
2. Update (Measurement Update) Phase
When a new measurement arrives (e.g., a new PPD calculation from incoming ACKs), the filter calculates the Kalman Gain and updates the estimate.



 is the Kalman Gain. It dictates how much weight to give the new measurement versus the existing prediction.55
 is the raw, noisy measurement observation ( derived from ).
 is the observation model.
 is the measurement noise covariance (the variance inherent in the PPD calculation due to cross-traffic).56
Application to Bonded Cellular Estimation
The power of the Kalman Filter in this specific architecture lies in dynamically tuning the matrices based on cross-layer telemetry.53
Under normal operation, the measurement noise  is set based on the historical variance of the PPD measurements. If a highly variant PPD sample arrives (e.g., due to a sudden, isolated burst of cross-traffic), the Kalman Gain  scales down. The filter relies more on its stable historical prediction, preventing the output capacity estimate from reacting wildly to network noise.54
However, if the physical layer telemetry reports a sustained drop in RSRQ (indicating a cellular handover or signal loss), the system dynamically increases the process noise matrix . This signals the Kalman Filter that the underlying physical state of the system has fundamentally shifted.53 The filter will subsequently increase the Kalman Gain , allowing the state estimate  to rapidly track downward to the new, lower capacity without waiting for extensive transport-layer confirmation.
This sensor fusion approach guarantees that the Capacity Oracle provides a capacity metric that is immune to high-frequency network noise but highly responsive to genuine physical degradation.
Proposed Architectural Blueprint for the Rust Stack
Synthesizing the theoretical analysis of MPTCP coupling, packet dispersion mathematics, cross-layer telemetry, and Kalman filtering provides the definitive answer to the final query: What is the simplest architecture change that bolts on a capacity oracle alongside the existing BBR stack?
The solution explicitly avoids rewriting the core BiscayController congestion logic. Instead, it re-routes the critical feedback loops between the transport layer, the intelligence overlay, and the scheduler.13
Step 1: Implement the PPD Capacity Oracle Module
Develop an independent, lightweight module (CapacityOracle) attached to the overarching TransportLink, sitting parallel to the BiscayController. This module utilizes a non-intrusive Packet Pair Dispersion mechanism.
Probing Logic: Modify the packet transmission routine at the transport interface. Approximately every 100ms to 200ms, force two MTU-sized video payload packets to be flushed to the socket strictly back-to-back. This temporarily overrides the BBR pacing interval for just those two packets to ensure a clean .29
Timestamping and ACK Tracking: Record the exact nanosecond departure timestamp of the first and second probe packets. When their corresponding ACKs arrive, record the reception timestamps. Ensure the receiver implementation generates immediate ACKs for these specific sequence numbers to avoid Delayed ACK interference.
Calculation: Calculate the inter-arrival delta: . Calculate the raw capacity sample: .33
Note regarding previous failures: The user query indicated that probing capacity from packets_acked failed because the counter did not increment between scheduler refresh intervals. The proposed PPD method bypasses this limitation. It does not rely on generic interval counters; it relies on exact nanosecond timestamping of specific sequence numbers injected directly into the active flow.
Step 2: Implement the Sensor Fusion Kalman Filter
Feed the continuous stream of  samples into a standard 1D Kalman Filter maintained per-link within the CapacityOracle.
Integration with Modem Telemetry: Utilize a background thread to periodically poll the host Linux OS (via ModemManager or AT commands) for the current RSRQ and SINR of each cellular modem.47
Dynamic Tuning: Map severe degradations in RSRQ (e.g., dropping below -15 dB) to a dynamic increase in the Kalman Filter's process noise matrix .53
Output: The final output of the Kalman Filter is the stable, true physical bottleneck capacity, designated as .
Step 3: Decouple DWRR and BLEST from the Congestion Controller
This is the critical step that definitively breaks the positive feedback loop.8 The architecture must alter the data source consumed by the upper layers.
DWRR Scheduler Reconfiguration: Modify the DWRR scheduler so that effective_bps is no longer derived from BiscayController.btl_bw. Instead, map effective_bps directly to the Oracle's  metric.8
Result: In the simulated tc environment, the Oracle will accurately output ~3 Mbps, 5 Mbps, and 8 Mbps. The DWRR credits will statically lock into a 3:5:8 ratio. The 14 Mbps video demand will be perfectly distributed (approx. 2.6 Mbps, 4.3 Mbps, and 7.1 Mbps). Because the DWRR credits are based on physical limits rather than allocated traffic, the rotation oscillation ceases entirely.
BLEST Filter Enhancement: The original BLEST algorithm is prone to misjudgments because it assumes link properties are static and relies heavily on RTT fluctuations.6 Enhance the BLEST implementation to factor in . By knowing the exact physical capacity, BLEST can accurately calculate the true hardware serialization delay, preventing artificial HoL blocking false positives and preserving the highest-capacity paths for critical payload data.6
Step 4: Allow BBR to Pace Autonomously
Do not force the Oracle's  estimate back into the BiscayController's internal btl_bw state machine.
The BBR algorithm's fundamental mathematical design requires it to pace traffic relative to what it is actively delivering in order to maintain its Bandwidth-Delay Product (BDP) queueing models without inducing bufferbloat.5
When the DWRR scheduler limits the 5 Mbps tc link to 4.3 Mbps of traffic, the BiscayController will measure a delivery rate of 4.3 Mbps, and its btl_bw will naturally settle near 4.3 Mbps. This is the structurally correct behavior for a localized congestion controller; it paces traffic smoothly at the mandated fractional rate without triggering artificial hardware delays.28
The independent BBR instance operates safely under the assumption that 4.3 Mbps is the network limit, keeping its inflight data small, its latency low, and its queue stable.30 This preserves the low-latency transport properties that make BBR highly desirable for live video, while the Oracle ensures the overarching multi-path routing remains mathematically balanced.
Conclusion
The winner-takes-all oscillation currently debilitating the Strata bonded cellular architecture is a textbook manifestation of a systemic design flaw: coupling a rate-based congestion controller's internal delivery metrics to a multi-path proportional scheduler under partial-load conditions. As the BBR-inspired BiscayController naturally scales its capacity estimates based on the fraction of traffic it is actively allocated, it creates an uncontrollable, recursive feedback loop with the DWRR credit assignment logic.8
Production multi-path systems mitigate this complexity either by relying on window-based coupled algorithms (such as LIA, OLIA, or wVegas) that inherently avoid rate-based feedback loops by design 15, or by explicitly tracking shared bottlenecks and enforcing static proportional lower bounds (such as C-MPBBR).18 However, in latency-sensitive live video transport where multiple links are highly heterogeneous and frequently underutilized, attempting to infer actual capacity by periodically saturating the link is profoundly counterproductive. Saturation intentionally induces bufferbloat, causing RTT spikes that violate the strict timing requirements of video decoders.22
The optimal architectural resolution necessitates the strict mathematical decoupling of the system's routing intelligence from its localized congestion control pacing.13 By bolting on a specialized Capacity Oracle utilizing Packet Pair Dispersion (PPD) algorithms 31—and systematically stabilizing those transport-layer measurements using a Kalman Filter driven by cross-layer physical telemetry (RSRQ/SINR) 47—the system can independently and non-intrusively derive the true tc physical limits of the constituent links.
Feeding this non-intrusively derived capacity into the DWRR scheduler and BLEST filter ensures that proportional load balancing remains statically stable, wholly independent of the encoder's fluctuating demand.6 Simultaneously, permitting the BiscayController to maintain its own localized, fractional btl_bw allows the transport layer to pace packets perfectly to the assigned scheduler load, maintaining the pristine, low-latency queue management that makes BBR the premier choice for modern video transport.7 This decoupled paradigm ensures high data reliability, mathematically sound capacity inference, and absolute scheduling stability across highly volatile, bonded cellular interfaces.
Works cited
draft-cardwell-iccrg-bbr-congestion-control-02.txt - IETF, accessed February 26, 2026, https://www.ietf.org/archive/id/draft-cardwell-iccrg-bbr-congestion-control-02.txt
aBBRate: Automating BBR Attack Exploration Using a Model-Based Approach - USENIX, accessed February 26, 2026, https://www.usenix.org/system/files/raid20-peterson.pdf
Overcoming TCP BBR Performance Degradation in Virtual Machines under CPU Contention, accessed February 26, 2026, https://arxiv.org/html/2601.05665v1
CCID5: An implementation of the BBR Congestion Control algorithm for DCCP and its impact over multi-path scenarios - arXiv, accessed February 26, 2026, https://arxiv.org/pdf/2106.15832
Optimization of BBR Congestion Control Algorithm Based on Pacing Gain Model - PMC, accessed February 26, 2026, https://pmc.ncbi.nlm.nih.gov/articles/PMC10181671/
Low Latency and High Data Rate (LLHD) Scheduler: A Multipath ..., accessed February 26, 2026, https://pmc.ncbi.nlm.nih.gov/articles/PMC9782081/
Reproducible Measurements of TCP BBR Congestion Control, accessed February 26, 2026, https://www.net.in.tum.de/fileadmin/bibtex/publications/papers/ComCom-2019-TCP-BBR.pdf
A Q-Learning Driven Energy-Aware Multipath Transmission Solution for 5G Media Services - IEEE Xplore, accessed February 26, 2026, https://ieeexplore.ieee.org/iel7/11/9789443/09702756.pdf
Cost-efficient multipath scheduling of video-on-demand traffic for the 5G ATSSS splitting function - City Research Online, accessed February 26, 2026, https://openaccess.city.ac.uk/id/eprint/32424/1/1-s2.0-S1389128624000501-main.pdf
Forecasting TCP's Rate to Speed up Slow Start - IEEE Xplore, accessed February 26, 2026, https://ieeexplore.ieee.org/iel7/8782664/9024218/09899695.pdf
A Link Status-Based Multipath Scheduling Scheme on Network Nodes - MDPI, accessed February 26, 2026, https://www.mdpi.com/2079-9292/13/3/608
A Stream-Aware MPQUIC Scheduler for HTTP Traffic in Mobile Networks - ResearchGate, accessed February 26, 2026, https://www.researchgate.net/publication/365116253_A_Stream-Aware_MPQUIC_Scheduler_for_HTTP_Traffic_in_Mobile_Networks
Design, Implementation and Evaluation of Congestion Control for Multipath TCP | USENIX, accessed February 26, 2026, https://www.usenix.org/conference/nsdi11/design-implementation-and-evaluation-congestion-control-multipath-tcp
RFC 6356 - Coupled Congestion Control for Multipath Transport Protocols, accessed February 26, 2026, https://datatracker.ietf.org/doc/rfc6356/
MPTCP Linux Kernel Congestion Controls - arXiv.org, accessed February 26, 2026, https://arxiv.org/pdf/1812.03210
D-OLIA: A Hybrid MPTCP Congestion Control Algorithm with Network Delay Estimation, accessed February 26, 2026, https://pmc.ncbi.nlm.nih.gov/articles/PMC8433826/
Mobility-Aware Congestion Control for Multipath QUIC in Integrated Terrestrial Satellite Networks - Electrical and Computer Engineering - University of Victoria, accessed February 26, 2026, https://www.ece.uvic.ca/~cai/tmc24-mmquic.pdf
(PDF) Coupled Multipath BBR (C-MPBBR): A Efficient Congestion Control Algorithm for Multipath TCP - ResearchGate, accessed February 26, 2026, https://www.researchgate.net/publication/344325324_Coupled_Multipath_BBR_C-MPBBR_A_Efficient_Congestion_Control_Algorithm_for_Multipath_TCP
Evaluating the Impact of Packet Scheduling and Congestion Control Algorithms on MPTCP Performance over Heterogeneous Networks - arXiv, accessed February 26, 2026, https://arxiv.org/html/2511.14550v1
arXiv:1901.09177v1 [cs.NI] 26 Jan 2019, accessed February 26, 2026, https://arxiv.org/pdf/1901.09177
BBR: Congestion-Based Congestion Control - ACM Queue, accessed February 26, 2026, https://queue.acm.org/detail.cfm?id=3022184
Cross-layer Network Bandwidth Estimation for Low-latency Live ABR Streaming - uconn, accessed February 26, 2026, https://nlab.engr.uconn.edu/papers/Shende2023_CLBE_MMSys_2023.pdf
Multi-path Mechanism for Audio / Video Streaming Based on Bandwidth Estimation, accessed February 26, 2026, http://paper.ijcsns.org/07_book/201302/20130205.pdf
Bufferbloat: Dark Buffers in the Internet - ACM Queue, accessed February 26, 2026, https://queue.acm.org/detail.cfm?id=2071893
Path Selection using Available Bandwidth Estimation in Overlay-based Video Streaming - Georgia Tech, accessed February 26, 2026, https://sites.cc.gatech.edu/fac/Constantinos.Dovrolis/Papers/manish-netw07.pdf
Internet of Drones: Improving Multipath TCP over WiFi with Federated Multi-Armed Bandits for Limitless Connectivity - MDPI, accessed February 26, 2026, https://www.mdpi.com/2504-446X/7/1/30
Beyond Concept Bottleneck Models: How to Make Black Boxes Intervenable? - NIPS, accessed February 26, 2026, https://proceedings.neurips.cc/paper_files/paper/2024/file/9a439efaa34fe37177eba00737624824-Paper-Conference.pdf
Learning to Harness Bandwidth with Multipath Congestion Control and Scheduling - IEEE Xplore, accessed February 26, 2026, https://ieeexplore.ieee.org/iel7/7755/4358975/09444785.pdf
What do packet dispersion techniques measure? - CAIDA.org, accessed February 26, 2026, https://www.caida.org/catalog/papers/2001_consti/consti.pdf
Performance Evaluation of TCP BBRv3 in Networks with Multiple ..., accessed February 26, 2026, https://www.mdpi.com/2076-3417/14/12/5053
(PDF) Estimating Available Bandwidth Using Packet Pair Probing - ResearchGate, accessed February 26, 2026, https://www.researchgate.net/publication/2831510_Estimating_Available_Bandwidth_Using_Packet_Pair_Probing
Packet dispersion techniques and a capacity estimation methodology Constantinos Dovrolis Parameswaran Ramanathan David Moore Geo - College of Computing, accessed February 26, 2026, https://faculty.cc.gatech.edu/~dovrolis/Papers/ton_dispersion.pdf
(PDF) Packet Dispersion Techniques and Capacity Estimation - ResearchGate, accessed February 26, 2026, https://www.researchgate.net/publication/242089487_Packet_Dispersion_Techniques_and_Capacity_Estimation
Algorithms and Requirements for Measuring Network Bandwidth - OSTI, accessed February 26, 2026, https://www.osti.gov/servlets/purl/813373
US7675856B2 - Bandwidth estimation in broadband access networks - Google Patents, accessed February 26, 2026, https://patents.google.com/patent/US7675856B2/en
Estimating Available Bandwidth Using Multiple Overloading Streams - Microsoft, accessed February 26, 2026, https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/04024177.pdf
PATHMON: A Methodology for Determining Available Bandwidth over an Unknown Network - Mitre, accessed February 26, 2026, https://www.mitre.org/sites/default/files/pdf/kiwior_pathmon.pdf
Feedback-assisted robust estimation of available bandwidth, accessed February 26, 2026, https://hlim.kentech.ac.kr/wiki/uploads/Publications/2009-comnet-available.pdf
TCP Westwood: Bandwidth Estimation for Enhanced Transport over Wireless Links - C3Lab, accessed February 26, 2026, https://c3lab.poliba.it/images/3/39/Mobi2001.pdf
Joint Rate and Channel Width Adaptation for 802.11 MIMO Wireless Networks - UCSB Computer Science, accessed February 26, 2026, https://sites.cs.ucsb.edu/~almeroth/papers/210.pdf
Practical Rate Adaptation for Very High Throughput ... - IEEE Xplore, accessed February 26, 2026, https://ieeexplore.ieee.org/iel5/7693/4656680/06415107.pdf
Cross-Layer Design in Wireless Local Area Networks(WLANs): Issues and Possible Solutions - ResearchGate, accessed February 26, 2026, https://www.researchgate.net/publication/391323978_Cross-Layer_Design_in_Wireless_Local_Area_NetworksWLANs_Issues_and_Possible_Solutions
Cellular Bonding Solutions for Live Streaming - Comparison - Speedify, accessed February 26, 2026, https://speedify.com/blog/better-streaming/cellular-bonding-for-live-streaming-solutions-comparison/
IP Bonding Explained: Why It Matters - LiveU, accessed February 26, 2026, https://www.liveu.tv/resources/blog/ip-bonding-explained-why-it-matters
Dejero Smart Blending Technology, accessed February 26, 2026, https://www.dejero.com/wp-content/uploads/2025/04/Dejero-Smart-Blending-Technology.pdf
Signal strength measure RSRP, RSRQ and SINR Reference for LTE & 5G Cheat Sheet, accessed February 26, 2026, https://poynting.tech/articles/antenna-faq/signal-strength-measure-rsrp-rsrq-and-sinr-reference-for-lte-cheat-sheet/
RSRP and RSRQ - Teltonika Networks Wiki, accessed February 26, 2026, https://wiki.teltonika-networks.com/view/RSRP_and_RSRQ
Understanding Signal Quality: RSSI, RSRP, and RSRQ - Eseye, accessed February 26, 2026, https://www.eseye.com/resources/iot-explained/understanding-signal-quality-rssi-rsrp-and-rsrq/
Sensor-Driven RSSI Prediction via Adaptive Machine Learning and Environmental Sensing, accessed February 26, 2026, https://www.mdpi.com/1424-8220/25/16/5199
LTE RSSI, RSRP and RSRQ Measurement - CableFree, accessed February 26, 2026, https://www.cablefree.net/wirelesstechnology/4glte/rsrp-rsrq-measurement-lte/
LTE RSRP, RSRQ, RSSNR and local topography profile data for RF propagation planning and network optimization in an urban propagation environment - PMC, accessed February 26, 2026, https://pmc.ncbi.nlm.nih.gov/articles/PMC6249519/
From RSSI to CSI: Indoor localization via channel response - Institutional Knowledge (InK) @ SMU, accessed February 26, 2026, https://ink.library.smu.edu.sg/cgi/viewcontent.cgi?article=5541&context=sis_research
Kalman filter based bandwidth estimation and predictive flow distribution for concurrent multipath transfer in wireless networks | Request PDF - ResearchGate, accessed February 26, 2026, https://www.researchgate.net/publication/261236705_Kalman_filter_based_bandwidth_estimation_and_predictive_flow_distribution_for_concurrent_multipath_transfer_in_wireless_networks
Kalman Filter Based Channel Estimation - International Journal of Engineering Research & Technology, accessed February 26, 2026, https://www.ijert.org/research/kalman-filter-based-channel-estimation-IJERTV3IS040284.pdf
Multipath Estimation of Navigation Signals Based on Extended Kalman Filter–Genetic Algorithm Particle Filter Algorithm - MDPI, accessed February 26, 2026, https://www.mdpi.com/2076-3417/15/7/3851
Kalman filter based channel estimation method in wireless communication and its performance optimization - Combinatorial Press, accessed February 26, 2026, https://combinatorialpress.com/article/jcmcc/Volume%20127/Volume%20127a/Kalman%20filter%20based%20channel%20estimation%20method%20in%20wireless%20communication%20and%20its%20performance%20optimization.pdf
A Stream-Aware MPQUIC Scheduler for HTTP Traffic in Mobile Networks, accessed February 26, 2026, http://staff.ustc.edu.cn/~kpxue/paper/TW-MPQUIC-yitaoxing-earlyaccess.pdf
