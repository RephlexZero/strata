Strata Transport Architecture: Next-Generation Bonded Cellular Optimization
1. Introduction
The transition of Strata Transport from a functional prototype to a production-grade bonded cellular solution necessitates a fundamental re-architecture of its core networking and media processing subsystems. Current reliance on theoretical capacity approximations and basic H.264 transport mechanisms is insufficient to cope with the stochastic nature of modern LTE and 5G uplink schedulers, nor does it leverage the bandwidth efficiencies of next-generation codecs like AV1 and HEVC. To achieve broadcast-quality reliability and latency, the system must evolve into an intelligent, media-aware transport fabric that tightly couples network telemetry with encoder rate control.
This research report provides an exhaustive technical analysis of the required subsystems to bridge the gap between current capabilities and the master plan. The analysis synthesizes data from cellular networking protocols, congestion control theory, GStreamer internals, and hyperscaler ingestion specifications to propose concrete implementation strategies. The report focuses on three critical pillars: precise per-link capacity estimation using model-based algorithms (BBRv3), robust dynamic encoder adaptation for multi-codec environments, and hierarchical, priority-aware packet scheduling that exploits the scalability features of AV1 and HEVC.
2. Advanced Bandwidth Estimation for Bonded UDP Links
The foundational challenge in bonded cellular transport is accurate per-link capacity estimation. The current implementation relies on a Mathis-formula approximation (Rate ~ MSS / (RTT * sqrt(Loss))), which was originally derived for standard TCP behavior over wired networks with varying packet loss. This model is fundamentally flawed for cellular uplinks because it conflates packet loss with congestion, whereas in cellular networks, the bottleneck is often the Radio Resource Control (RRC) state and the Time Division Scheduling (TDS) grants, not a simple router buffer drop.1
2.1 Limitations of Loss-Based Estimation in Cellular Networks
Using packet loss as a primary signal for capacity on cellular links leads to severe underutilization and instability. Modern cellular modems (LTE/5G) employ extensive buffering and Hybrid Automatic Repeat Request (HARQ) at the MAC layer to hide radio link errors. This results in "bufferbloat," where packets are queued rather than dropped when the radio link degrades. By the time a packet loss is detected by the application layer, the end-to-end latency has often already spiked to unacceptable levels (exceeding 500ms or 1s), rendering the stream unusable for live interaction.3
Mathis-style estimators interpret the absence of loss during these buffering periods as "infinite" capacity, causing the bitrate adaptation logic to ramp up continuously until a massive drop occurs—a "sawtooth" pattern that destabilizes video encoders and degrades Quality of Experience (QoE). Conversely, random radio interface errors that are not due to congestion can be misinterpreted as congestion signals, causing unnecessary throttle-down events. To resolve this, Strata Transport must decouple loss from congestion signals and adopt a rate-based estimation model that measures the physical delivery rate of the link.
2.2 BBR-Style Probing for Bonded UDP
The optimal approach for measuring true per-link capacity in a bonded environment is to implement a modified BBR (Bottleneck Bandwidth and Round-trip propagation time) state machine for each individual link within the bonding scheduler. Unlike TCP BBR, which controls a congestion window (cwnd) for a single flow, the bonding scheduler must maintain a model of the network pipe using two independent variables for each link: Bottleneck Bandwidth (BtlBw) and Round-Trip Propagation Time (RTprop).5
2.2.1 Adapting BBRv3 Logic to User-Space UDP
The adaptation of BBRv3 logic into the TransportLink::get_metrics() function requires shifting from kernel-space TCP structures to user-space UDP feedback loops. The core components of this adaptation include:
Delivery Rate Sampling: instead of relying on kernel TCP information, the bonding scheduler must track the delivery rate using custom application-layer feedback. When a packet is sent on Link , the sender records the send_time and the cumulative delivered_bytes. The receiver must echo back the arrival_time and the interval between packets in the acknowledgment (ACK). The sender then computes the delivery rate sample as:

This measurement must be passed through a windowed max-filter (typically over 6-10 RTTs) to estimate the BtlBw effectively, filtering out transient scheduling noise.6
RTprop Estimation: The scheduler must track the minimum Round-Trip Time (min_rtt) observed over a sliding window (typically 10-30 seconds). This RTprop represents the physical propagation delay plus the minimum radio scheduling latency when the network pipe is empty. It serves as the baseline for detecting bufferbloat; any RTT significantly above RTprop indicates queuing delay.3
The State Machine Implementation: Each TransportLink must run an independent state machine to actively probe the link:
STARTUP: The link creates an exponential increase in its pacing rate (doubling every RTT) to rapidly find the capacity ceiling. This phase ends when the delivery rate stops increasing despite an increase in sending rate.8
DRAIN: Once the ceiling is hit, the scheduler rapidly reduces the rate to clear the queues created during the STARTUP phase, ensuring low latency is restored.10
PROBE_BW: This is the steady-state phase where the scheduler cycles the pacing gain. It sequentially applies a gain of  (to probe for more bandwidth),  (to drain any resulting queue), and then holds at  (CRUISE) to utilize the estimated capacity. This cycle ensures the system constantly tests for available bandwidth without causing sustained congestion.11
PROBE_RTT: Periodically (e.g., every 10 seconds), if no new min_rtt has been seen, the scheduler drops the rate significantly (to just 4 packets in flight) for a short duration (200ms) to drain the pipe completely and refresh the RTprop estimation.8
2.2.2 Probing Coordination in Bonded Scheduling
A critical insight for bonded transport is that the PROBE_BW phases of the individual links must be coordinated. If multiple links enter the "pulse up" phase ( rate) simultaneously, the aggregate ingress rate might overwhelm the receiver's CPU or the network bottleneck at the receiving end, even if the individual cellular links have capacity.
Orchestration Logic: The global bonding scheduler acts as an orchestrator. It should ensure that only one link is in the PROBE_BW:UP phase at any given time. While one link pulses up to test capacity, other links should remain in CRUISE () or DRAIN phases to maintain aggregate stability.
Feedback Loop Integration: If Link A probes up and measures an increased delivery rate, the TransportLink object updates its internal capacity_bps metric. The global scheduler aggregates these capacities and signals the BitrateAdapter to ramp up the video encoder target, effectively closing the loop between network physical capacity and media generation.13
2.3 Cellular-Specific Probing Considerations
Cellular radio schedulers allocate resources in Transmission Time Intervals (TTI), which are typically 1ms for LTE and scalable down to 62.5µs for 5G NR.1 This discretized scheduling introduces inherent jitter that must be accounted for in the probing logic.
Probe Duration: A probe burst must be sufficiently long to span multiple TTIs to average out the scheduling jitter and granting delays. A probe duration of 20ms to 40ms (roughly equivalent to 1-3 video frames at 60fps) is recommended. Shorter probes risk hitting a "scheduling hole" (such as a DRX sleep cycle or high-priority control signaling block) and underestimating the link capacity.15
Probe Intensity: To trigger the radio scheduler to grant more Resource Blocks (RBs) and ramp up the Modulation and Coding Scheme (MCS), the probe must effectively fill the current grant. While BBR’s standard  pacing gain is generally sufficient, high-frequency bands like 5G mmWave may require a more aggressive short burst (e.g., ) to wake up the radio from power-save modes and trigger a bandwidth up-switch.16
Implementation Recommendation: The TransportLink class should implement a windowed max-filter of delivery rates derived from custom ACK feedback. A per-link BBR state machine should be employed, with the global scheduler phase-shifting the PROBE_BW cycles across links to prevent aggregate congestion while ensuring accurate, continuous capacity estimation.
3. Dynamic Bitrate Behavior in GStreamer Pipelines
Once the network capacity is accurately estimated, the system must dynamically adjust the video encoder's output bitrate. Changing the bitrate property on a running GStreamer element is a non-trivial operation that interacts deeply with the encoder's internal Rate Control (RC) and Video Buffering Verifier (VBV) models. The behavior and failure modes vary significantly across codec implementations (x264, x265, AV1).
3.1 x264enc (H.264) Behavior
GStreamer's x264enc element supports dynamic bitrate changes via the bitrate property, but proper configuration is essential to prevent artifacts and buffer violations.
Immediate Application: Changes to the bitrate property are applied almost immediately, typically on the next frame boundary, via the underlying x264_encoder_reconfig() function. This process does not require a keyframe insertion or a pipeline restart, making it suitable for real-time adaptation.4
VBV Interaction and Failure Modes: The interaction with the VBV buffer is the most critical failure mode. If the bitrate is dynamically increased (e.g., from 1 Mbps to 8 Mbps) but the vbv-buf-capacity (buffer size) remains fixed at a small value calculated for the initial low bitrate, the encoder will be constrained. It may fail to reach the target bitrate or produce severe quality fluctuations as it hits the buffer ceiling.
Constraint: In Constant Bitrate (CBR) or VBV-constrained Variable Bitrate (VBR) modes, vbv-maxrate and vbv-bufsize must logically scale with the target bitrate. However, changing vbv-bufsize dynamically is risky and can reset the rate control model, causing visual glitches.
Recommendation: The optimal strategy is to configure the vbv-buf-capacity based on the maximum expected bitrate of the session (e.g., 20 Mbps) during initialization. The bitrate property can then be modulated dynamically below this ceiling. This ensures the VBV never becomes an artificial bottleneck during ramp-ups while still protecting against massive overshoots.4
Ramp Speed: Ramping the bitrate too fast (e.g., jumping from 1 Mbps to 10 Mbps in a single step) can cause the encoder to flood the VBV, resulting in a packet burst that exceeds the physical link capacity before the BBR probe has confirmed it. Rate changes should be damped (e.g., max 10-20% increase per GOP) to maintain stability.19
3.2 x265enc (H.265/HEVC) Behavior
The x265enc element allows for dynamic reconfiguration, but it requires specific tuning for low-latency live streaming.
Dynamic Properties: Like x264, x265enc supports dynamic bitrate updates. The vbv-bufsize and vbv-maxrate properties control the upper bounds of the stream.
Latency Tuning: For live transmission, setting tune=zerolatency is mandatory. Without this, x265 introduces significant lookahead latency (often 40-100+ frames) to optimize B-frame placement and RC lookahead, which effectively breaks real-time interactivity.
Efficiency Gains: At target bitrates of 1-15 Mbps, x265 offers a significant efficiency advantage, delivering equivalent visual quality at approximately 60% of the bitrate required by H.264. This efficiency is crucial for bonded cellular scenarios where bandwidth is both expensive and volatile.21
3.3 AV1 Encoders (svtav1enc)
AV1 represents the frontier of coding efficiency but introduces new challenges regarding encoding latency and parameter mutability.
Element Selection: svtav1enc (Scalable Video Technology for AV1) is currently the only production-ready candidate for live GStreamer pipelines on commodity hardware. Other implementations like rav1e are too slow for real-time HD, and aomenc is generally too resource-intensive.
Dynamic Bitrate Support: svtav1enc exposes a target-bitrate (or bitrate in newer plugin versions) property. However, documentation and community findings suggest that changing this on-the-fly works best when the encoder is configured in Low Delay CBR mode (rc-mode=0 or similar depending on version) to avoid artifacts or latency spikes.23
Latency Considerations: Real-time AV1 encoding (e.g., 1080p30) on commodity ARM64 hardware (like Jetson Orin or Raspberry Pi 5) is on the edge of feasibility. On high-end platforms, svtav1enc with faster presets (8-12) can achieve real-time performance, but the encoding latency is typically 50-100ms higher than x264 due to the complexity of the partitioning and prediction decisions.25
Parameter Reconfiguration: Recent updates to GStreamer (1.24+) and SVT-AV1 have improved the robustness of dynamic parameter updates. However, it is crucial to verify if specific properties like level-of-parallelism or tile configuration require a pipeline restart or state transition (READY -> NULL) to change, which would interrupt the stream.26
Codec Abstraction Layer Strategy:
To manage these differences, Strata should implement a CodecController interface that abstracts the underlying element:
x264: Set bitrate directly. Initialize with a static, high vbv-buf-capacity.
x265: Set bitrate. Ensure tune=zerolatency is applied.
AV1: Use svtav1enc. Map bitrate changes to target-bitrate. continuously monitor encoding latency (via proctime GStreamer queries) and trigger a fallback to HEVC if the hardware stalls or latency exceeds a safety threshold.
4. Closed-Loop Encoder Adaptation Architectures
The master plan describes a BITRATE_CMD control packet, raising the question of where the intelligence for bitrate decisions should reside: the sender or the receiver.
4.1 Receiver-Side Estimation (Industry Standard)
Leading solutions from LiveU and TVU predominantly utilize receiver-side estimation.28
Rationale: The receiver possesses the "ground truth" of the transmission. It observes the actual packet arrival intervals, the specific patterns of packet loss (hole-punching), and the jitter introduced after traversing the entire network path. It can distinguish between sustainable "goodput" (decodable video) and raw throughput (which includes FEC overhead and retransmissions).
Mechanism: The receiver calculates the optimal safe bandwidth and sends a specific target bitrate command back to the sender. This simplifies the sender's logic to a basic "Receiver says X, I set X" model.
4.2 Sender-Side Estimation (NADA/GCC Model)
Protocols developed for WebRTC, such as NADA (RFC 8698) and Google Congestion Control (GCC), favor sender-side estimation.30
Rationale: The sender has intimate knowledge of the source constraints, such as the encoder's VBV fullness, frame sizes, and transmission queue depths. It can react more rapidly to local link up-events (e.g., a modem reporting a jump in signal strength) before the receiver can observe the resulting throughput increase.
Mechanism: The receiver sends raw telemetry data (ACKs, delay gradients, ECN marks) back to the sender. The sender then runs the congestion control algorithm (e.g., a Kalman filter or delay-gradient analyzer) to compute the target rate.32
4.3 The Strata Decision: Hybrid Approach
For Strata Transport, a Sender-Centric Control with Receiver-Assisted Telemetry is the recommended architecture.
Why? In a bonded system, packet scheduling decisions must happen at the sender. The sender needs to know the specific capacity of each link to effectively stripe packets. A single aggregate "target bitrate" from the receiver obscures the individual link performance, making it difficult to optimize the bonding ratios.
BitrateAdapter Logic:
Input Integration: The BitrateAdapter aggregates TransportLink metrics (per-link BBR estimates) and Receiver Reports (aggregate packet loss, one-way delay trends transmitted via BITRATE_CMD payloads containing metrics, not just commands).
Capacity Calculation: It calculates AggregateCapacity as the sum of all valid, stable per-link capacities.
Safety Margin: The TargetBitrate is set to AggregateCapacity  0.85. This 15% headroom is critical to accommodate FEC overhead, retransmissions, and network jitter without inducing congestion.
Degradation Stages: The adapter implements a state machine for degradation:
Normal: Bitrate = TargetBitrate.
Congestion: Increase FEC rate, reduce Bitrate.
Severe: Drop B-frames (reduces latency and bitrate).
Critical: Send Keyframes only (to force resync).
Comparison with NADA: While NADA is excellent for low-latency conferencing, its reliance on delay gradients can be noisy on cellular networks due to inherent radio jitter. A simpler "Capacity Sum minus Headroom" model, augmented by BBR's robust probing, provides greater stability for bonded cellular uplinks.34
5. Integration Testing Strategies for Real-Time Transport
Validating a complex bonded transport system requires deterministic, automated metrics rather than subjective visual checks. A rigorous test harness is essential for preventing regressions.
5.1 Metrics Without Decode (Transport Layer)
These metrics can be measured efficiently in Continuous Integration (CI) environments without the computational overhead of video decoding:
Throughput Stability: The variance of the delivery rate over time. A target variance of  indicates a stable adaptation loop.36
Packet Delivery Ratio (PDR): The ratio of total received packets to total sent packets. The target should be  after FEC recovery.37
Recovery Latency: The time elapsed from the injection of a link failure to the stabilization of the bitrate adaptation.
Reordering Depth: The maximum buffer size required to reorder packets at the receiver, which serves as a proxy for network jitter and scheduling efficiency.
5.2 Metrics With Decode (Video Quality)
VMAF (Video Multi-Method Assessment Fusion): This is the industry standard for perceptual quality. While computationally expensive, it provides a score that correlates highly with human perception.
Strategy: In CI, use the "Phone" model for mobile-targeted testing. To save compute time, calculate VMAF on keyframes only or use temporal subsampling (e.g., 1 fps) rather than every frame.38
Glass-to-Glass Latency: This is a critical metric for live transmission.
Measurement: Embed a high-precision timestamp (e.g., a QR code containing the capture_time or a distinct color pattern) into the source video frames. At the receiver, capture the rendered output and decode the timestamp. The latency is calculated as CaptureTime - RenderTime.
Automation: This can be automated using GstTracer or interpipes to inject and extract metadata through the pipeline. Alternatively, a specialized test source can generate frames with sequence numbers encoded directly in the pixel data for optical recognition at the sink.41
5.3 Critical Test Scenarios for strata-sim
A robust regression suite must cover the following "Top 5" scenarios to validate system resilience:
The "Cliff": Simulate the sudden loss of the highest-capacity link (e.g., a 10 Mbps link drops to 0 Mbps instantly).
Assertion: The stream must not disconnect. The bitrate must adapt to the remaining capacity within 2 GOPs (Group of Pictures).
The "Flapping" Link: One link oscillates rapidly between high capacity (5 Mbps) and low capacity (500 kbps) every 5 seconds.
Assertion: The scheduler identifies the instability and penalizes the link (using hysteresis) rather than causing the encoder bitrate to oscillate wildly.43
High Jitter / Bufferbloat: Inject 500ms of jitter on all links using tc-netem.
Assertion: The jitter buffer expands to handle the delay; video plays smoothly, albeit with higher latency, without stuttering.41
Packet Loss Burst: Inject a 20% packet loss burst for 2 seconds to simulate a handover or interference event.
Assertion: FEC and retransmission mechanisms recover the lost packets. No visual artifacts (macroblocking) appear in the decoded video.44
Bandwidth Ramp: Start the simulation with 1 Mbps capacity and ramp up to 20 Mbps over 30 seconds.
Assertion: The BBR probing logic correctly detects the increasing capacity, and the encoder ramps up to maximum quality/bitrate.4
Soak Testing: Additionally, a long-duration test (e.g., 1 hour) with randomized network impairments (based on a Markov model of cellular fading) should be run to monitor for memory leaks, resource exhaustion, and A/V sync drift.37
6. YouTube Live Ingestion: AV1, HEVC, and Capabilities
YouTube's support for next-generation codecs fundamentally alters the bandwidth efficiency equation for Strata, enabling higher quality at lower bitrates.
6.1 Ingestion Specifications
Protocols:
RTMP: The legacy standard. It strictly supports only H.264 video and AAC audio.45
Enhanced RTMP (RTMP v2): This is the modern evolution of the protocol, adding support for H.265 (HEVC) and AV1. It is the preferred target for low-latency ingest of high-efficiency codecs.46
HLS/DASH Ingest: These HTTP-based protocols support H.265 and AV1 but introduce significant latency because they are segment-based. They are suitable for 4K broadcast where latency is secondary, but poor for "live" newsgathering or interactive use cases.45
Bitrate Caps (2024/2025):
1080p60: Recommended range is 6-10 Mbps.
4K (2160p60): Supported up to 40-51 Mbps.
Implication: With AV1, a 1080p60 stream looks pristine at just 4-6 Mbps. This effectively doubles the "visual capacity" of a standard bonded uplink compared to H.264.49
6.2 Codec Efficiency Implications
Given an aggregate bonded capacity of approximately 15 Mbps (typical for 3 LTE links):
H.264: Capable of good 1080p60 (requires ~8-10 Mbps). 4K is effectively impossible as it requires 20+ Mbps for acceptable quality.
H.265 (HEVC): Delivers excellent 1080p60 at ~5-6 Mbps. 4K becomes feasible at ~12-15 Mbps.
AV1: Delivers pristine 1080p60 at ~4 Mbps. 4K streaming is comfortably feasible at ~10-12 Mbps.50
Strategic Conclusion: Implementing AV1 and HEVC support enables Strata to offer "4K Live Streaming over Cellular" as a competitive differentiator, whereas competitors relying solely on H.264 remain limited to 1080p.
6.3 Pipeline Changes for Enhanced RTMP
To support Enhanced RTMP in GStreamer:
Muxer: The standard flvmux must be replaced with eflvmux (Enhanced FLV Muxer), available in GStreamer 1.24 and later. This element supports the new FourCC codes and header extensions required for HEVC and AV1 signaling in RTMP.52
Parsers: The pipeline must include h265parse and av1parse after the encoder and before the muxer to ensure the stream is correctly formatted (converting between bytestream and packetized formats).
AV1 Framing: AV1 uses Open Bitstream Units (OBUs) rather than NAL units. The av1parse element must be configured to output stream-format=obu-stream to be compatible with the muxer.54
7. Smart Use of Spare Bandwidth: The Quality vs. Resilience Frontier
When the available link capacity exceeds the encoder's bitrate requirements (e.g., 15 Mbps capacity for a 6 Mbps AV1 stream), simply increasing the bitrate yields diminishing returns in visual quality (VMAF). The "Pareto Frontier" of QoE suggests that this excess bandwidth is better spent on reliability.
7.1 Redundancy Strategies
Selective Duplication (Clone Scheduling):
Strategy: Identify Critical packets (Sequence Headers, I-Frames, Audio) and duplicate them across the two best-performing links.
Cost: Moderate overhead (~10-20% of total bandwidth).
Benefit: Provides zero-latency recovery for the most essential data. If Link A drops an I-frame, Link B delivers it instantly, preventing video artifacts or freezing.56
Proactive FEC Injection:
Strategy: Instead of a fixed FEC rate (e.g., 10%), implement dynamic FEC that scales with spare bandwidth. If Capacity - Bitrate = 5 Mbps, the system can utilize up to 4 Mbps for generating repair symbols.
Benefit: This allows the stream to recover from massive burst losses (e.g., 50% packet loss on a link) without incurring the RTT delay of retransmissions.
Proactive Retransmission:
Strategy: If a link exhibits rising jitter (a precursor to loss), the sender can speculatively retransmit in-flight packets on a more stable link before a NACK is even received from the receiver.58
7.2 Decision Framework (BitrateAdapter)
The BitrateAdapter should operate in two distinct modes based on bandwidth availability:
Maximize Quality (Bandwidth Constrained): Minimize overhead. Push the encoder bitrate up to Capacity * 0.9 to get the best possible visual fidelity.
Maximize Reliability (Bandwidth Abundant): Cap the encoder bitrate at a "Visually Lossless" threshold (e.g., 8 Mbps for 1080p AV1). Allocate all remaining bandwidth capacity to Packet Duplication and high-rate FEC to ensure bulletproof delivery.
Trigger Logic:

8. Next-Gen Scheduler: AV1 and H.265 Capabilities
AV1 and H.265 introduce advanced coding structures that enable much smarter scheduling strategies than were possible with H.264.
8.1 AV1 OBU Priority Mapping
AV1 organizes video data into Open Bitstream Units (OBUs). The scheduler can parse the obu_header byte to identify the type of data and assign transport priorities accordingly.54
OBU Type
Content
Priority Class
Recommended Action
Sequence Header
Codec Configuration
Critical
Duplicate on all links. Never drop.
Frame Header
Frame Structure Info
Critical
Duplicate on best links.
Tile Group (Base)
Base Layer Video Data
Reference
Apply high FEC rates. Retransmit aggressively.
Tile Group (Enh)
Enhancement Layer Data
Standard
Standard FEC. Drop if congestion occurs.
Padding
Filler Data
Disposable
Drop immediately to save bandwidth.

8.2 Scalable Video Coding (SVC) Integration
Both AV1 and H.265 support Temporal Scalability (e.g., L1T2, L1T3 modes), which structures the video into a base layer (T0) and enhancement layers (T1, T2).
Scheduler Logic:
T0 Frames (Base Layer): These are essential for motion continuity and decoding subsequent frames. They must be scheduled on the most reliable link (lowest loss probability).
T2 Frames (High Framerate): These provide 60fps smoothness but are not strictly necessary for decoding. They can be scheduled on the highest capacity link, even if it has higher loss.
Congestion Response: If bandwidth drops suddenly, the scheduler can instantaneously drop T2 packets. This gracefully reduces the framerate (e.g., from 60fps to 30fps) without breaking the bitstream or causing artifacts, which is a much cleaner degradation than dropping B-frames in H.264.62
8.3 Rate-Distortion-Reliability Optimization
Future iterations of the scheduler should implement a joint optimization function:

This formula mathematically balances the marginal visual gain of additional bitrate against the risk of buffering stalls due to insufficient protection, dynamically guiding the BitrateAdapter to the optimal operating point.65
9. Conclusion
To realize the production-grade Strata Transport described in the master plan, the following engineering roadmap is recommended:
Immediate: Replace the Mathis-based estimation with a UDP-BBRv3 probing mechanism. Implement the ProbeBW coordination logic to prevent bonded link self-congestion.
Short Term: Implement Enhanced RTMP support using eflvmux and svtav1enc to unlock 4K/AV1 streaming capabilities for YouTube.
Short Term: Wire the Receiver-Assisted Control Loop. Utilize receiver feedback for adaptation telemetry but retain the decision logic on the sender to effectively manage encoder VBV constraints.
Medium Term: Implement AV1 OBU Parsing within the scheduler. Map Sequence and Frame headers to the Critical priority class and enable packet duplication for these specific packets.
Long Term: Develop the Redundancy Pareto Framework. Enable the system to dynamically trade excess bandwidth for "infinite" reliability on high-capacity connections.
This architectural shift transforms Strata from a basic "dumb pipe" aggregator into an intelligent, media-aware transport fabric capable of outperforming legacy bonding solutions through modern codec integration and superior congestion control.
Works cited
The 5G channels with various TTI-durations. | Download Scientific Diagram - ResearchGate, accessed February 17, 2026, https://www.researchgate.net/figure/The-5G-channels-with-various-TTI-durations_fig3_357642218
5G NR Transmission Time Interval - Devopedia, accessed February 17, 2026, https://devopedia.org/5g-nr-transmission-time-interval
TCP BBRv3 for Wi-Fi 6 Networks - Emergent Mind, accessed February 17, 2026, https://www.emergentmind.com/topics/tcp-bbrv3
Performance Evaluation of TCP BBRv3 in Networks with Multiple Round Trip Times - MDPI, accessed February 17, 2026, https://www.mdpi.com/2076-3417/14/12/5053
draft-ietf-ccwg-bbr-03, accessed February 17, 2026, https://datatracker.ietf.org/doc/html/draft-ietf-ccwg-bbr-03
BBR: Congestion-Based Congestion Control - ACM Queue, accessed February 17, 2026, https://queue.acm.org/detail.cfm?id=3022184
Improved Control Mechanism of Bottleneck Bandwidth and Round-Trip Propagation Time v3 Congestion with Enhanced Fairness and Efficiency - MDPI, accessed February 17, 2026, https://www.mdpi.com/2673-4591/89/1/11
Optimization of BBR Congestion Control Algorithm Based on Pacing Gain Model - PMC, accessed February 17, 2026, https://pmc.ncbi.nlm.nih.gov/articles/PMC10181671/
BBR TCP (Bottleneck Bandwidth and RTT) - GÉANT federated confluence, accessed February 17, 2026, https://wiki.geant.org/spaces/EK/pages/121340614/BBR+TCP+Bottleneck+Bandwidth+and+RTT
Improvement of RTT Fairness Problem in BBR Congestion Control Algorithm by Gamma Correction - PMC, accessed February 17, 2026, https://pmc.ncbi.nlm.nih.gov/articles/PMC8234792/
oBBR: Optimize Retransmissions of BBR Flows on the Internet - USENIX, accessed February 17, 2026, https://www.usenix.org/system/files/atc23-bi.pdf
BBR-Based Congestion Control and Packet Scheduling for ..., accessed February 17, 2026, http://staff.ustc.edu.cn/~kpxue/paper/TVT-BBR-Wei-2021.01.pdf
Improved RTT Fairness of BBR Congestion Control Algorithm Based on Adaptive Congestion Window - MDPI, accessed February 17, 2026, https://www.mdpi.com/2079-9292/10/5/615
Ad-BBR: Enhancing Round-Trip Time Fairness and Transmission ..., accessed February 17, 2026, https://www.mdpi.com/1999-5903/17/5/189
(PDF) BBR Congestion Control Algorithms: Evolution, Challenges and Future Directions, accessed February 17, 2026, https://www.researchgate.net/publication/400077956_BBR_Congestion_Control_Algorithms_Evolution_Challenges_and_Future_Directions
TCP ROCCET: An RTT-Oriented CUBIC Congestion Control Extension for 5G and Beyond Networks - arXiv, accessed February 17, 2026, https://arxiv.org/html/2510.25281v1
(PDF) TCP BBR for ultra-low latency networking: challenges, analysis, and solutions, accessed February 17, 2026, https://www.researchgate.net/publication/339328122_TCP_BBR_for_ultra-low_latency_networking_challenges_analysis_and_solutions
Bug 621663 – x264enc: support changing bitrate property on the fly - GNOME Bugzilla, accessed February 17, 2026, https://bugzilla.gnome.org/show_bug.cgi?id=621663
x264enc - GStreamer, accessed February 17, 2026, https://gstreamer.freedesktop.org/documentation/x264/index.html
x264enc with VBV buffer - gstreamer-devel@lists.freedesktop.org, accessed February 17, 2026, https://gstreamer-devel.narkive.com/0wTPgjih/x264enc-with-vbv-buffer
Low bit rate H.265 encoder help - Jetson TX2 - NVIDIA Developer Forums, accessed February 17, 2026, https://forums.developer.nvidia.com/t/low-bit-rate-h-265-encoder-help/273389
Encoding Best Practices for Live Streaming in 2025 - Resi, accessed February 17, 2026, https://resi.io/blog/encoding-best-practices-for-live-streaming-in-2025/
svtav1enc - GStreamer, accessed February 17, 2026, https://gstreamer.freedesktop.org/documentation/svtav1/index.html
File gstreamer.changes of Package gstreamer - openSUSE Build Service, accessed February 17, 2026, https://build.opensuse.org/projects/GNOME:STABLE:48/packages/gstreamer/files/gstreamer.changes?expand=0
GStreamer Encoding Latency in NVIDIA Jetson Platforms - RidgeRun Developer Wiki, accessed February 17, 2026, https://developer.ridgerun.com/wiki/index.php/GStreamer_Encoding_Latency_in_NVIDIA_Jetson_Platforms
GStreamer 1.26 release notes - Freedesktop.org, accessed February 17, 2026, https://gstreamer.freedesktop.org/releases/1.26/
File: NEWS - Debian Sources, accessed February 17, 2026, https://sources.debian.org/src/gstreamer-editing-services1.0/1.26.2-1/NEWS/
LiveU Schedule | Scheduling & Orchestrating for Live Production, accessed February 17, 2026, https://www.liveu.tv/products/manage/schedule
Transmission Protocol Architecture: The Crucial Factor in Cellular Bonding Performance | by Tse Kevin | Dec, 2025 | Medium, accessed February 17, 2026, https://medium.com/@kevintse756/transmission-protocol-architecture-the-crucial-factor-in-cellular-bonding-performance-85580dff6c1c
RFC 8698 - Network-Assisted Dynamic Adaptation (NADA): A Unified Congestion Control Scheme for Real-Time Media - IETF Datatracker, accessed February 17, 2026, https://datatracker.ietf.org/doc/html/rfc8698
Cross: A Delay Based Congestion Control Method for RTP Media - arXiv, accessed February 17, 2026, https://arxiv.org/pdf/2409.10042
Network-Assisted Dynamic Adaptation (NADA): A Design Summary - IETF, accessed February 17, 2026, https://www.ietf.org/slides/slides-ccirtcws-network-assisted-dynamic-adaptation-xiaoqing-zhu-and-rong-pan-00.pdf
Performance analysis of adaptive streaming algorithms for a low-latency environment - Lund University Publications, accessed February 17, 2026, https://lup.lub.lu.se/student-papers/record/9090908/file/9091587.pdf
[1809.00304] Congestion Control for RTP Media: a Comparison on Simulated Environment, accessed February 17, 2026, https://arxiv.org/abs/1809.00304
Performance analysis of adaptive streaming algorithms for a low-latency environment - Lund University Publications, accessed February 17, 2026, https://lup.lub.lu.se/student-papers/record/9090908/file/9090916.pdf
Planet GStreamer - RSSing.com, accessed February 17, 2026, https://gstreamer11.rssing.com/chan-11311199/latest.php
A Comprehensive Survey of Wireless Time-Sensitive Networking (TSN): Architecture, Technologies, Applications, and Open Issues - arXiv, accessed February 17, 2026, https://arxiv.org/html/2312.01204v3
VMAF: The Journey Continues - Netflix Tech Blog, accessed February 17, 2026, https://netflixtechblog.com/vmaf-the-journey-continues-44b51ee9ed12
VMAF: The Journey Continues Industry Adoption, accessed February 17, 2026, https://mcl.usc.edu/wp-content/uploads/2018/10/2018-10-25-Netflix-Worked-with-Professor-Kuo-on-Video-Quality-Metric-VMAF.pdf
Netflix/vmaf: Perceptual video quality assessment based on multi-method fusion. - GitHub, accessed February 17, 2026, https://github.com/Netflix/vmaf
Glass-to-glass latency - Luxonis Forum, accessed February 17, 2026, https://discuss.luxonis.com/d/5316-glass-to-glass-latency
How to measure glass-to-glass video latency? - Vay, accessed February 17, 2026, https://vay.io/how-to-measure-glass-to-glass-video-latency/
IP Impairment Testing for LTE Networks - INASE, accessed February 17, 2026, https://www.inase.org/library/2015/barcelona/bypaper/ELECTR/ELECTR-15.pdf
Enhancing Video Network Resiliency With LTR and RS Code | At Scale Conferences, accessed February 17, 2026, https://atscaleconference.com/enhancing-video-network-resiliency-with-ltr-and-rs-code/
YouTube Live Streaming Ingestion Protocol Comparison - Google for Developers, accessed February 17, 2026, https://developers.google.com/youtube/v3/live/guides/ingestion-protocol-comparison
Multitrack Audio Capability in GStreamer FLV Plugin · Devlog ..., accessed February 17, 2026, https://centricular.com/devlog/2025-11/Multitrack-Audio-Capability-in-FLV/
YouTube Live - The Story of Enhancing RTMP | Nicole Quah, accessed February 17, 2026, https://www.youtube.com/watch?v=L3G_z6V0CgE
Delivering Live YouTube Content via HLS - Google for Developers, accessed February 17, 2026, https://developers.google.com/youtube/v3/live/guides/hls-ingestion
Choose live encoder settings, bitrates, and resolutions - YouTube Help, accessed February 17, 2026, https://support.google.com/youtube/answer/2853702?hl=en
AV1 vs H.265: Codec Comparison Guide [2026 Updated] - Red5 Pro, accessed February 17, 2026, https://www.red5.net/blog/av1-vs-h265/
AV1 - Wikipedia, accessed February 17, 2026, https://en.wikipedia.org/wiki/AV1
GStreamer 1.28 release notes - Freedesktop.org, accessed February 17, 2026, https://gstreamer.freedesktop.org/releases/1.28/
GstEncodingProfile - GStreamer, accessed February 17, 2026, https://gstreamer.freedesktop.org/documentation/pbutils/encoding-profile.html
AV1 Bitstream & Decoding Process Specification, accessed February 17, 2026, https://aomediacodec.github.io/av1-spec/
RTP Payload Format For AV1, accessed February 17, 2026, https://aomediacodec.github.io/av1-rtp-spec/v1.0.0.html
Redundant Packet Transmission - USENIX, accessed February 17, 2026, https://www.usenix.org/sites/default/files/conference/protected-files/rpt-nsdi-talk-final.pdf
RFC 7198: Duplicating RTP Streams, accessed February 17, 2026, https://www.rfc-editor.org/rfc/rfc7198.html
Publication List, accessed February 17, 2026, https://www-lsm.naist.jp/~kasahara/paper-e.html
MPRTP: Multipath considerations for real-time media | Request PDF - ResearchGate, accessed February 17, 2026, https://www.researchgate.net/publication/260685906_MPRTP_Multipath_considerations_for_real-time_media
AV1 decoder model - Andrey Norkin, accessed February 17, 2026, https://norkin.org/research/av1_decoder_model/index.html
A Technical Overview of AV1 - Wikimedia Commons, accessed February 17, 2026, https://upload.wikimedia.org/wikipedia/commons/1/14/A_Technical_Overview_of_AV1.pdf
Scalable Video Coding (SVC) Extension for WebRTC - W3C, accessed February 17, 2026, https://www.w3.org/TR/webrtc-svc/
Temporal Scalability - BlogGeek.me, accessed February 17, 2026, https://bloggeek.me/webrtcglossary/temporal-scalability/
Mastering the AV1 SVC chains - Medooze - Medium, accessed February 17, 2026, https://medooze.medium.com/mastering-the-av1-svc-chains-a4b2a6a23925
Tradeoff between effective packet loss rate and FEC redundancy level on... - ResearchGate, accessed February 17, 2026, https://www.researchgate.net/figure/Tradeoff-between-effective-packet-loss-rate-and-FEC-redundancy-level-on-one-side-PSNR_fig5_279160662
TAROT: Towards Optimization-Driven Adaptive FEC Parameter Tuning for Video Streaming - arXiv, accessed February 17, 2026, https://arxiv.org/pdf/2602.09880
