Advanced Architectures for Bonded Multi-Path Real-Time Video Transport: A Technical Research Report for Project Strata
Executive Summary
The domain of real-time video transport is currently navigating a pivotal transition from throughput-oriented legacy protocols to deadline-aware, intelligent systems. For Strata, a Rust-based RIST implementation, the path to broadcast-grade resilience over heterogeneous networks (5G, LEO satellite, Wi-Fi 6E) requires transcending basic Dynamic Weighted Round Robin (DWRR) scheduling and standard congestion controls. This report synthesizes research from 2022–2026, analyzing cutting-edge techniques in multipath scheduling, coupled congestion control, forward error correction, and kernel-bypass networking.
Analysis of recent literature, including contributions from USENIX ATC '25, SIGCOMM '24, and LCN '24, highlights three critical architectural shifts necessary for Strata:
Reliability-Aware Scheduling: Moving beyond flow-agnostic distribution to schemes like STORM, which bifurcate traffic into reliable (I-frame/Audio) and unreliable (P-frame) queues to eliminate Head-of-Line (HOL) blocking.
Predictive Link Intelligence: Integrating deep learning models (e.g., BandSeer's Bi-LSTM) to predict cellular bandwidth fluctuations before they manifest as packet loss, replacing reactive probing with proactive adaptation.
Zero-Copy Data Planes: Leveraging AF_XDP and io_uring within the Rust ecosystem to bypass the kernel network stack, enabling 10Gbps+ line-rate processing essential for uncompressed or lightly compressed UHD streams.
The following comprehensive analysis details the theoretical foundations, implementation mechanics, and performance implications of these technologies, specifically tailored for integration into the Strata GStreamer plugin architecture.
1. Multi-Path Transport Scheduling
The scheduler is the decision engine of a bonded transport system. While Strata’s current DWRR implementation provides basic load balancing, recent research demonstrates that it is insufficient for guaranteeing the strict latency bounds required by live video in heterogeneous environments. The state-of-the-art has moved toward deadline-minimization and reliability-aware logic.
1.1 Reliability-Aware Scheduling: The STORM Architecture
One of the most significant recent contributions to multipath scheduling is STORM (Multipath QUIC Scheduler for Quick Streaming Media Transport), presented at USENIX ATC 2025.1 While designed for MPQUIC, its core logic is protocol-agnostic and highly applicable to RIST.
1.1.1 The Limitations of Uniform Scheduling
Traditional schedulers treat the retransmission of a lost packet with the same priority as the transmission of a new packet. In a video transport context, this is often suboptimal. If a packet belonging to a video frame that is already past its decoding deadline is retransmitted, it consumes valuable bandwidth on the bottleneck link without contributing to the user experience (QoE). This phenomenon creates a unique form of Head-of-Line (HOL) blocking where "zombie" packets delay "live" packets.
1.1.2 Dual-Queue (Dual-Q) Mechanism
STORM fundamentally alters the sender architecture by separating traffic into two distinct logical queues before they reach the transport layer's flow controller:
Reliable Queue (Block Streams): This queue is reserved for data that is critical for the reconstruction of the stream and must be delivered regardless of latency costs.
Content: Stream metadata (PAT/PMT/SPS/PPS), Audio packets (AAC/Opus frames), and Video Keyframes (IDR/I-frames).
Behavior: Infinite retries (up to a massive timeout) and aggressive FEC protection.
Unreliable Queue (Stream Streams): This queue holds data that is tolerant to loss or dropping.
Content: Inter-predicted frames (P-frames), Bi-directional frames (B-frames), and Enhancement Layers (in SVC).
Behavior: Limited retry budget or "fire-and-forget" semantics.
1.1.3 The Urgency-Priority Algorithm
The scheduler selects packets from these queues based on a composite score derived from an Urgency Factor and a Priority Weight (). The scoring function for a packet  can be conceptualized as:

Where:

 is a tunable parameter (recommended ).
When , the scheduler favors urgent packets to minimize latency, potentially sacrificing reliability (dropping I-frames). When , it prioritizes the Reliable Queue. Research indicates that a balanced  significantly reduces tail latency in 5G networks where link capacity fluctuates rapidly.2
Feature
Standard DWRR
STORM (Dual-Q)
Packet Handling
FIFO (First-In, First-Out)
Priority-based (Reliable vs Unreliable)
Retransmissions
Compete with new data
Prioritized based on frame type
HOL Blocking
High risk during burst loss
Eliminated for P-frames
Complexity
Low ()
Medium ( scanning)

1.2 The 4D-MAP Framework: Learning-Based Adaptation
4D-MAP (Multipath Adaptive Packet Scheduling), detailed in the Journal of Computer Science and Technology (2024) 3, introduces an online learning approach to scheduling, addressing the specific challenges of heterogeneous paths (e.g., combining Fiber with LTE).
1.2.1 LinUCB Online Learning
Unlike heuristic schedulers that rely on fixed rules (e.g., "always fill the lowest RTT pipe first"), 4D-MAP employs a Linear Upper Confidence Bound (LinUCB) algorithm. This reinforcement learning model maps the "Network State" vector (RTT variance, Packet Loss Rate, Available Bandwidth) to a "Reward" (QoE metric like VMAF or SSIM). Over time, the scheduler "learns" that certain network states (e.g., high jitter on LTE) correlate with poor performance for large I-frames, and adapts its dispatch logic accordingly.
1.2.2 The Four Novel Scheduling Primitives
4D-MAP defines four specific actions that Strata can implement:
Dispatch: The primary logic for assigning a packet to a path. It predicts the completion time on all paths and selects the one that maximizes the reward.
Duplicate: This extends beyond static redundancy. 4D-MAP triggers duplication only when the predicted loss probability of the primary path exceeds a dynamic threshold (e.g., 5%). This "Dynamic Bonding" saves bandwidth compared to static 1:1 mirroring.5
Discard: A critical "Quick Win" technique. The sender actively calculates the estimated arrival time (). If , the packet is dropped at the sender. This prevents "badput"—bandwidth used for data that arrives too late to be displayed.
Decompensate: In scenarios of severe aggregate congestion, this primitive proactively drops the tail end of a Group of Pictures (GOP) to ensure the header (I-frame) of the next GOP has sufficient bandwidth to pass.
1.3 Heterogeneous Path Scheduling: LDMP-FEC
LDMP-FEC (Low-Delay Multipath FEC) 6 specifically addresses the "Straggler Problem" in bonding: when a high-latency path (e.g., Satellite, 600ms RTT) is bonded with a low-latency path (e.g., Fiber, 20ms RTT). Standard striping causes massive reordering at the receiver, overflowing the jitter buffer.
1.3.1 EAT-Based Switching Logic
LDMP-FEC calculates the Expected Arrival Time (EAT) interval for the next packet on all available paths.
MinRTT Phase: If the fast path can deliver the packet and return before the slow path could deliver it once (i.e., non-overlapping delivery intervals), the scheduler dispatches exclusively to the fast path.
Round-Robin Phase: When the delivery intervals overlap (due to queuing delay building up on the fast path), the scheduler switches to Round-Robin to utilize the aggregate capacity.
This hybrid approach has been shown to reduce Out-of-Order (OFO) packets by 50% compared to pure MinRTT or pure Round-Robin strategies.6
1.4 Blocking Estimation (BLEST)
BLEST (Balanced Load and Elastic Subflow Transmission) 7 focuses on minimizing Head-of-Line blocking. It introduces a wait condition:
If the scheduler is about to send a packet on a Slow Path, it calculates: WaitTime = (FastPath_CWND_Full_Time).
If WaitTime + FastPath_RTT < SlowPath_RTT, it is faster to wait for the fast path to become free than to use the slow path immediately.
Implementation Note: This logic effectively "idles" high-latency links during non-congested periods, reserving them solely for redundancy or overflow traffic.
1.5 Rust Implementation Insights
For Strata, implementing these advanced schedulers requires moving away from simple iterators to stateful structs.
Quick Win: Implement the Discard primitive.
Mechanism: Add a deadline: Instant field to the internal packet struct. Before passing to socket.send(), check if Instant::now() + estimated_owd > packet.deadline { drop(packet); }.
Crate Availability:
quinn (Rust QUIC implementation) contains scheduling logic that can be adapted for RIST.
Reinforcement learning can be implemented using burn or tch-rs (PyTorch bindings), though a simple Rust match statement implementing the LinUCB decision tree is often sufficient and lower latency.
2. Congestion Control for Real-Time Media
Congestion Control (CC) in a bonded environment is doubly complex: it must estimate the capacity of each link without inducing bufferbloat, and it must coordinate (couple) the links to ensure fairness to other traffic sharing the bottleneck. Strata currently uses NADA, but newer algorithms offer distinct advantages.
2.1 BBRv3: Detailed Analysis for Real-Time
BBRv3 (Bottleneck Bandwidth and Round-trip propagation time, v3) 9 is Google's latest iteration, designed to fix the fairness issues of BBRv1 and the throughput issues of BBRv2.
2.1.1 The "0.95 RTTmin" Problem
Recent extensive evaluation (Net25 Papers, 2025) reveals a critical characteristic of BBRv3: it tends to maintain a queuing delay of approximately 0.95  RTTmin.9
On a fiber link (20ms RTT), this results in ~19ms of queue, which is acceptable.
On a cellular link (100ms RTT), this results in ~95ms of queue. For a real-time system targeting <200ms glass-to-glass latency, this bufferbloat is prohibitive.
Multi-Flow Contention: When multiple BBRv3 flows share a bottleneck (e.g., bonding multiple LTE modems on the same tower), the queuing delay often exceeds 1  RTTmin more than 50% of the time.9
2.1.2 Adaptation Strategy
While raw BBRv3 is unsuitable, its model-based approach (ProbeBW, ProbeRTT) is sound. Strata should implement a "BBR-RT" (Real-Time) variant:
Fixed Headroom: Instead of scaling the queue target with RTT, target a fixed queuing delay (e.g., 5-10ms) regardless of RTT.
Pacing Gain: Reduce the pacing gain during ProbeBW to be less aggressive (e.g., 1.1x instead of 1.25x) to prevent jitter spikes.
2.2 Coupled Congestion Control: wVegas and BALIA
When bonding links share a physical bottleneck, uncoupled algorithms (like running independent NADA instances) will compete with each other, leading to oscillation and packet loss. Coupled algorithms link the congestion windows.
2.2.1 Algorithm Comparison
LIA (Linked Increase Algorithm): Increases the aggregate window only if it is less aggressive than a single TCP flow. This is often too conservative for video, leading to under-utilization.
OLIA (Opportunistic LIA): Shifts traffic away from congested paths but can be unstable.
BALIA (Balanced LIA) 11: The current IETF standard for MPTCP. It balances responsiveness and fairness.
wVegas: A delay-based coupled algorithm. Recent research (2024) suggests wVegas is superior for video because it detects queuing delay before loss occurs.11 It uses the queuing delay as a congestion signal to perform fine-grained load balancing, satisfying the "Congestion Equality Principle."
Recommendation: Strata should replace or augment NADA with a coupled wVegas implementation. This is particularly effective for cellular links where bufferbloat is the primary enemy.
2.3 Learning-Based Control: PCC Proteus
PCC Proteus (Performance-oriented Congestion Control) 13 takes a fundamentally different approach. Instead of a hard-coded control law (like AIMD), it uses Online Learning (specifically, a Multi-Armed Bandit approach).
2.3.1 Utility Function
PCC Proteus optimizes a utility function defined by the user:

By setting a high penalty coefficient () for latency, PCC naturally discovers a sending rate that maximizes throughput without triggering bufferbloat. This is particularly robust in "non-standard" networks (like LTE with token bucket policers) where TCP assumptions break down.
2.3.2 Implementation
The pcc-uspace implementation (C++) 13 serves as a reference. Porting this to Rust involves creating a "Monitor Interval" struct that aggregates statistics for a short burst of packets, calculates the Utility, and updates the sending rate for the next burst.
2.4 Handling High-Variance Cellular Links
Cellular links (4G/5G) exhibit "bufferbloat" and "variable capacity" due to radio resource scheduling.
Technique: Use One-Way Delay (OWD) gradients rather than RTT. RTT includes the reverse path noise (ACKs), which on cellular can be highly asymmetric.
Rust Crate: copa (an implementation of the Copa congestion control algorithm in Rust) uses OWD gradients and is a strong candidate for cellular-specific paths.
3. Forward Error Correction (FEC) for Live Video
In a bonded environment, retransmission (ARQ) is often too slow for high-latency paths. FEC provides the necessary insurance.
3.1 Streaming Codes vs. Block Codes
3.1.1 Reed-Solomon (Block Code)
Mechanism: Divides data into blocks of  packets, generating  parity packets.
Constraint: The receiver must wait for the entire block window before decoding. This introduces a "Block Latency" floor.
Rust: reed-solomon-erasure crate. Highly optimized with SIMD (AVX2), but the block latency makes it suboptimal for Ultra-Low Latency (ULL).
3.1.2 RaptorQ (Streaming/Fountain Code)
Mechanism: A "Rateless" code. The encoder can generate an infinite stream of repair symbols from the source block. The receiver can decode as soon as it receives any set of symbols slightly larger than the source size ().
Advantage: Perfect for bonding. If Link 1 fails, Strata can simply flood Link 2 with repair symbols until the receiver signals completion. There is no need to negotiate a new block size.
Performance: Recent optimizations in the Rust raptorq crate 14 have achieved encoding speeds of ~1.2 Gbps using SIMD, making it viable for 4K video.
Recommendation: Use RaptorQ. The flexibility to "top up" redundancy dynamically outweighs the slight CPU overhead compared to Reed-Solomon.
3.2 Adaptive FEC: LDMP-FEC Approach
Static FEC (e.g., fixed 20% overhead) is wasteful. LDMP-FEC 6 adapts the redundancy ratio based on a Gilbert-Elliott Model (2-state Markov Chain: Good/Bad).
Logic:
Calculate transition probabilities  and  based on RIST RTCP feedback from the last 5 seconds.
Predict the packet loss rate for the next window.
Adjust the FEC overhead to target a residual loss rate of  or lower.
Quick Win: Implement a simplified "Burst Detector." If >3 consecutive packets are lost, immediately switch the model to "Bad" state and double FEC redundancy. Decay back to "Good" only after 5 seconds of clean transmission.
3.3 Unequal Error Protection (UEP)
Not all bits are equal. Losing an I-frame destroys the video for seconds (until the next GOP); losing a B-frame causes a minor glitch.
Technique: Strata should parse the MPEG-TS PES headers to identify frame types.
Policy:
High Priority (I-Frame/Audio): 50% FEC overhead + ARQ.
Low Priority (P/B-Frame): 10% FEC overhead, limited ARQ.
Benefit: Increases perceptual quality (VMAF) under congestion without increasing the total bitrate budget.
4. Media-Aware Transport Optimizations
Moving up the stack, "Media-Awareness" involves exposing video semantics to the transport layer, allowing for smarter decisions than "drop tail."
4.1 Scalable Video Coding (SVC) & AV1
AV1 SVC 16 is becoming standard in WebRTC and RIST workflows. It structures video into layers:
Base Layer (BL): Essential quality (e.g., 720p 30fps).
Enhancement Layers (EL): Improved quality (e.g., +1080p details, +30fps for 60fps total).
Implementation in Strata:
Header Parsing: Identify the Temporal ID (TID) in the AV1 RTP descriptor.
Congestion Response: When EstimatedBandwidth < CurrentBitrate, instead of uniform random drops, drop all packets with TID > 0.
Impact: This instantly reduces bitrate by ~30-50% (depending on the layering) while ensuring the decoder receives a valid, albeit lower frame-rate, stream. This avoids the "decoding cascade" error where dropping a reference frame corrupts future frames.
4.2 Frame-Deadline-Aware Scheduling (vStreamPth & ARMA)
Recent research like vStreamPth 18 and ARMA 20 formalizes Deadline-Awareness.
Concept: Every video frame has a CaptureTime and a MaxLatency (budget).

Transport Logic: Before pushing a packet to the NIC, check:

Action: If true, drop the packet. Sending it wastes bandwidth because it will arrive too late to be displayed.
Feedback: Ideally, notify the encoder to generate an IDR frame immediately, as the current GOP is now corrupted/incomplete.
4.3 CMAF Low-Latency Interaction
CMAF (Common Media Application Format) uses "Chunked Transfer Encoding" to send partial video segments (e.g., 200ms chunks) before the full segment (e.g., 2s) is complete.
Bonding Risk: A naive bonding proxy might buffer the entire HTTP/TCP stream to analyze it, destroying the low-latency property.
Solution: Strata must operate in Stream Mode, flushing buffers immediately upon detecting a chunk boundary (or simply byte-streaming) to preserve the "glass-to-glass" latency benefits of CMAF.22
5. Adaptive Bitrate and Encoder Signaling
The transport layer must exert "backpressure" on the video encoder to match the network capacity.
5.1 Deep Reinforcement Learning: Pensieve & Fugu
Pensieve 23 (SIGCOMM) popularized using Deep Reinforcement Learning (DRL) for client-side bitrate selection. Fugu 24 adapts this for the sender side.
Mechanism: Fugu uses a Transmission Time Predictor (TTP). It feeds the past history of "Time taken to send a frame" into a neural network to predict the transmission time of the next frame size.
Control: If PredictedTime > FrameInterval, Fugu signals the encoder to lower the bitrate.
Rust: Strata can integrate tensorflow or ort (ONNX Runtime) bindings to run a lightweight, pre-trained Fugu model. The inference overhead (CPU) is minimal for a small model compared to the video encoding itself.
5.2 Rate Signaling Mechanisms
How does Strata talk to the encoder (e.g., OBS, FFmpeg, Hardware Encoder)?
TWCC (Transport-wide Congestion Control): Defined in RFC 8888. The receiver sends detailed packet arrival times. The sender (Strata) calculates the bitrate cap. This is the modern standard used by WebRTC.
REMB (Receiver Estimated Maximum Bitrate): Legacy. The receiver calculates the rate and tells the sender.
Recommendation: Implement TWCC. It allows the sender (Strata) to own the congestion control logic, which is crucial for bonding (where the sender knows about the multiple links, but the receiver might just see a unified stream).
5.3 Smoothing Strategies
Directly feeding the estimated bandwidth to the encoder causes oscillation (visual "pumping").
Strategy: Use an Exponential Weighted Moving Average (EWMA).
Asymmetric Damping:
Rate Decrease: Signal immediately (Safety).
Rate Increase: Signal slowly (Stability).
SignaledRate = min(InstantBW, EWMA_BW).
6. Jitter Buffer and Playout Algorithms
The Jitter Buffer is the final line of defense. The NetEQ component in WebRTC serves as the industry gold standard.25
6.1 Relative Delay vs. Absolute Delay
WebRTC's NetEQ has evolved (2024-2025) to use Relative Delay to handle clock drift and asymmetrical RTTs common in cellular networks.
Old Metric: Delay = ArrivalTime - SendTime. (Sensitive to clock sync).
New Metric:

This measures how much "slower" the current packet is compared to the "fastest" packet seen recently. This value represents the pure network jitter queue, independent of clock offsets.
6.2 Target Playout Delay
Modern jitter buffers do not target a "packet count" (e.g., "keep 5 packets in buffer"). They target a Playout Delay (e.g., "render frames 50ms after capture").
Logic: TargetDelay = max(Filter(Jitter), MinDelay).
Adaptation: If the buffer level grows (due to network freeze followed by burst), the playout engine speeds up audio/video (Accelerate) to drain the buffer. If the buffer empties (Underrun), it slows down or stretch-plays (Preemptive Expand).
6.3 Partial Reliability and Gap Concealment
In live streaming, "Better Late Than Never" is false. "Better Never Than Late" is true.
Gap Strategy: If Packet N is missing and its deadline passes:
Skip: Advance the playout pointer to N+1.
Conceal: If audio, use Packet Loss Concealment (PLC) / Waveform Extension. If video, repeat the last frame (Freezing).
Transport: Send a "NACK Give-Up" message to the sender so it stops trying to retransmit Packet N.
7. Link Quality Prediction and Probing
Reactive bonding (switching after loss) is too slow. Predictive bonding is the goal.
7.1 BandSeer: Deep Learning for Bandwidth Prediction
BandSeer 26 (LCN 2024) demonstrates that cellular bandwidth is highly non-linear and context-dependent.
Architecture: Uses a Bi-LSTM (Bidirectional Long Short-Term Memory) network.
Inputs:
RSRP (Reference Signal Received Power).
RSRQ (Reference Signal Received Quality).
CQI (Channel Quality Indicator - reported by the modem).
MCS (Modulation and Coding Scheme).
Outcome: Accurately predicts "Future Bandwidth" 1-2 seconds ahead, outperforming simple regression models.
Implementation: Strata can use the tch-rs crate to run this LSTM model. It requires collecting modem metrics via a side-channel (e.g., libqmi or AT commands to the LTE/5G modem).
7.2 The Signal-Watermark Mechanism (STORM)
STORM 2 introduces a simpler, hardware-based trigger.
Watermark: Define a threshold (e.g., RSRP < -115 dBm).
Action: If the signal drops below this watermark, the link is immediately classified as "Unreliable."
Result: Strata stops scheduling P-frames (Unreliable Queue) on this link before the TCP/UDP stack starts seeing packet loss. It only uses the link for redundant FEC packets until the signal recovers.
8. Rust-Specific Systems Techniques
Rust provides unique capabilities for high-performance, safe networking.
8.1 AF_XDP: The 10Gbps Frontier
Standard Linux sockets (sendto/recvfrom) involve expensive context switches and memory copying. AF_XDP (Address Family XDP) 28 allows Rust code to interact directly with the NIC's DMA rings.
8.1.1 Architecture
UMEM: A shared memory region allocated by the user process, registered with the kernel.
Rings:
Fill Ring: User application places addresses of empty buffers here.
Rx Ring: Kernel/NIC places descriptors of received packets here.
Tx Ring: User places descriptors of packets to send here.
Completion Ring: Kernel returns descriptors of sent packets.
Performance: Capable of 10-20 Mpps (Million Packets Per Second) on a single core.
Rust Ecosystem:
aya: The premier library for eBPF and XDP in Rust. It simplifies loading the BPF programs that redirect packets to the AF_XDP socket.
Zero-Copy: The packet data remains in UMEM. Strata can parse the RIST/RTP headers in-place without memcpy.
8.2 io_uring and Async Runtimes
For scenarios where AF_XDP is too complex or hardware support is lacking, io_uring is the modern standard.
Glommio 29: A Thread-per-Core runtime for Rust. It uses io_uring for all I/O and binds threads to CPUs to maximize cache locality. This is ideal for a high-throughput video gateway/aggregator.
Monoio: Another high-performance runtime optimized for throughput.
Comparison: Glommio is better suited for storage-heavy or extremely high-PPS tasks. For a general-purpose bonding client, tokio-uring (integrating io_uring into the standard Tokio runtime) offers a better balance of ecosystem compatibility.
8.3 Zero-Copy Buffer Management
The Problem: Frequent allocation (Vec::new()) and cloning (.clone()) destroy performance in video processing.
Solution: Pool Allocators.
Pattern:
Allocate a massive BytesMut (e.g., 1MB) "Arena".
Read packets into this arena.
Use Bytes::slice() to create handles to individual packets. These are reference-counted pointers to the underlying arena.
Pass these slices through the RIST pipeline (Encryption -> FEC -> Bonding).
Zero-Copy: No data is copied until the final send (or never, if using AF_XDP).
Crates: bytes, zeropool.30
8.4 SIMD Optimization
Use Case: FEC encoding (Galois Field arithmetic) is computationally expensive (XORs).
Rust:
std::simd 31: The portable SIMD module (currently in nightly). It allows writing Simd<u8, 32> ^ Simd<u8, 32> to XOR 32 bytes in a single CPU cycle (AVX2).
Performance: Accelerates RaptorQ/Reed-Solomon encoding by 10x-20x compared to scalar loops.
9. Bonding and Multi-Path in Production Systems
9.1 Speedify: Modes of Operation
Speedify (Connectify) 32 is a consumer VPN bonding solution with relevant modes:
Speed Mode: Stripes packets across all links. Effective for bulk file transfer, dangerous for live video due to reordering.
Redundant Mode: Sends 100% duplication (Packet A on Link 1 AND Link 2). Maximum reliability, double the cost.
Streaming Mode: The "Holy Grail" for Strata. It uses dynamic duplication.
Logic: Detects "Stream" traffic (RTP/UDP). Monitors packet loss/jitter.
Trigger: If Loss > 1% OR Jitter > 20ms on Link 1, it temporarily enables Redundant Mode. When the link stabilizes, it reverts to single-path.
9.2 CellFusion: Network Coding
CellFusion (Alibaba, SIGCOMM '23) 34 uses XNC (Cross-Network Coding).
Scenario: Bonding 3 links.
Technique: Instead of sending Packet A on Link 1 and Packet B on Link 2 (where losing Link 1 loses A), it sends:
Link 1: 
Link 2: 
Link 3:  (XOR Parity)
Benefit: This provides protection for both A and B with only 50% overhead, unlike Redundant Mode's 100%.
9.3 RIST TR-07: Satellite Hybrid
VSF TR-07 35 defines a standard for Satellite Hybrid bonding.
Architecture:
Path 1 (Satellite): Unidirectional, high bandwidth (Multicast/DVB). Carries the main video.
Path 2 (Internet/LTE): Bidirectional, variable bandwidth.
Operation: The receiver detects gaps in the Satellite feed. It sends NACKs over the Internet path. The sender retransmits the missing packets over the Internet path.
Applicability: Essential for Starlink/OneWeb scenarios where brief obstructions cause outages.
10. Emerging Standards and Protocols
10.1 RTP over QUIC (RoQ)
RoQ (draft-ietf-avtcore-rtp-over-quic) 37 is the convergence of media and transport.
Stream Mapping:
Datagram Mode: Unreliable, low overhead. Equivalent to RIST/UDP.
Stream Mode: Reliable.
BBC Implementation (gst-roq):
Implements a "Stream-Per-Frame" mapping. Each video frame is a separate QUIC stream.
Advantage: If a frame is late, the application can issue RESET_STREAM. This cleanly cancels the transmission of that specific frame without affecting the TCP/QUIC context of subsequent frames. This effectively solves HOL blocking at the transport layer.
10.2 WebTransport
WebTransport 38 is the browser API for QUIC.
Relevance: It allows a bonding gateway to stream directly to a web browser with low latency (replacing HLS/DASH).
Rust: Implementable using quinn or h3. Strata could serve as a "WebTransport Ingest" node.
11. Conclusion and Implementation Roadmap
The research clearly indicates that the future of bonded video transport lies in deadline-awareness and cross-layer intelligence. Strata is well-positioned to adopt these technologies.
Prioritized Roadmap:
Immediate (Week 1 - Quick Wins):
Discard Primitive: Add logic to drop packets at the sender if Est_OWD > Deadline.
Relative Delay: Update the jitter buffer metric to normalized delay (NetEQ logic).
wVegas: Switch NADA to wVegas for better cellular handling.
Short Term (1-3 Months):
STORM Dual-Queue: Refactor the scheduler to segregate I-frames/Audio from P-frames.
RaptorQ: Integrate the raptorq crate for rateless FEC.
Signal Monitoring: Implement Netlink/ModemManager hooks for Signal-Watermark dropping.
Long Term (3-6 Months):
AF_XDP Backend: Develop a zero-copy data plane for Linux gateways.
BandSeer: Train and integrate an LSTM bandwidth predictor.
RoQ Support: Add an experimental output plugin for RTP-over-QUIC.
By systematically integrating these components, Strata can evolve from a robust RIST implementation into a state-of-the-art, intelligent video transport platform.
Table: Summary of Key Recommended Technologies
Area
Technology
Primary Benefit
Rust Implementation
Scheduling
STORM (Dual-Q)
Eliminates HOL blocking for P-frames
Custom crossbeam queues
Scheduling
4D-MAP (Discard)
Saves bandwidth on late packets
std::time::Instant checks
Congestion
wVegas
Low-delay coupled control
copa (conceptually similar)
FEC
RaptorQ
Rateless recovery for bonded links
raptorq crate
Systems
AF_XDP
10Mpps+ throughput
aya, libbpf-sys
Systems
io_uring
High IOPS efficiency
glommio, tokio-uring
Media
AV1 SVC
Layer-aware congestion response
gstreamer-rs (parsing)
Prediction
BandSeer
Proactive bandwidth adaptation
tch-rs (LSTM)

Citations: 1 - STORM Scheduler 3 - 4D-MAP 6 - LDMP-FEC 9 - BBRv3 14 - RaptorQ 28 - AF_XDP 37 - RoQ 26 - BandSeer 25 - NetEQ Relative Delay
Works cited
STORM: a Multipath QUIC Scheduler for Quick Streaming Media Transport under Unstable Mobile Networks | USENIX, accessed February 14, 2026, https://www.usenix.org/conference/atc25/presentation/hu-liekun
STORM: a Multipath QUIC Scheduler for Quick Streaming Media Transport under Unstable Mobile Networks - USENIX, accessed February 14, 2026, https://www.usenix.org/system/files/atc25-hu-liekun.pdf
4D-MAP:面向直播的多路径QUIC自适应报文调度, accessed February 14, 2026, https://jcst.ict.ac.cn/cn/article/cstr/32374.14.s11390-023-3204-z
A Low-Latency MPTCP Scheduler for Live Video Streaming in Mobile Networks | Request PDF - ResearchGate, accessed February 14, 2026, https://www.researchgate.net/publication/351856408_A_Low-Latency_MPTCP_Scheduler_for_Live_Video_Streaming_in_Mobile_Networks
(PDF) FALCON: Fast and Accurate Multipath Scheduling using Offline and Online Learning, accessed February 14, 2026, https://www.researchgate.net/publication/358144933_FALCON_Fast_and_Accurate_Multipath_Scheduling_using_Offline_and_Online_Learning
LDMP-FEC: A Real-Time Low-Latency Scheduling Algorithm for ..., accessed February 14, 2026, https://www.mdpi.com/2079-9292/14/3/563
329-traffic-splitting - Tor design proposals, accessed February 14, 2026, https://spec.torproject.org/proposals/329-traffic-splitting.html
5G-MANTRA: MULTI-ACCESS NETWORK TESTBED FOR RESEARCH ON ATSSS A Thesis by MATAN BRONER Submitted to the Graduate and Professiona - OAKTrust, accessed February 14, 2026, https://oaktrust.library.tamu.edu/bitstreams/b9bab243-8542-43ac-a943-651b47f4e508/download
Insights into BBRv3\'s Performance and Behavior by Experimental ..., accessed February 14, 2026, https://networking.ifip.org/2025/images/Net25_papers/1571125683.pdf
Promises and Potential of BBRv3 - PAM 2024, accessed February 14, 2026, https://pam2024.cs.northwestern.edu/pdfs/paper-59.pdf
Evaluating the Impact of Packet Scheduling and Congestion Control Algorithms on MPTCP Performance over Heterogeneous Networks - arXiv, accessed February 14, 2026, https://arxiv.org/pdf/2511.14550
FMPTCP: Achieving High Bandwidth Utilization and Low Latency in Data Center Networks, accessed February 14, 2026, http://staff.ustc.edu.cn/~kpxue/paper/TCom-FMPTCP-JiangpingHan-2024.01.pdf
PCCproject/PCC-Uspace: The userspace implementations of PCC. - GitHub, accessed February 14, 2026, https://github.com/PCCproject/PCC-Uspace
Why DF Raptor® is Better Than Reed-Solomon for Streaming Applications - Qualcomm, accessed February 14, 2026, https://www.qualcomm.com/media/documents/files/why-raptor-codes-are-better-than-reed-solomon-codes-for-streaming-applications.pdf
RaptorQ (RFC6330) and performance optimization in Rust - cberner.com, accessed February 14, 2026, https://www.cberner.com/2019/03/30/raptorq-rfc6330-rust-optimization/
5 Key Reasons AV1 Will Play a Big Role in WebRTC Streaming - Red5 Pro, accessed February 14, 2026, https://www.red5.net/blog/av1-webrtc-streaming/
Scalable Video Conferencing Using SDN Principles - arXiv, accessed February 14, 2026, https://arxiv.org/html/2503.11649v1
Toward High-Quality Real-Time Video Streaming: An Efficient Multi-Stream and Multi-Path Scheduling Framework, accessed February 14, 2026, https://www.computer.org/csdl/journal/nw/2025/04/10930812/25bqfrmrfk4
Streaming High-Quality Mobile Video with Multipath TCP in Heterogeneous Wireless Networks - ResearchGate, accessed February 14, 2026, https://www.researchgate.net/publication/283583978_Streaming_High-Quality_Mobile_Video_with_Multipath_TCP_in_Heterogeneous_Wireless_Networks
Towards End-to-End Latency Guarantee in MEC Live Video Analytics with App-RAN Mutual Awareness - Daehyeok Kim, accessed February 14, 2026, https://daehyeok.kim/assets/papers/arma-mobisys25.pdf
publications | Daehyeok Kim, accessed February 14, 2026, https://daehyeok.kim/publications/
Why Sub‑Second Latency Matters in Live Streaming—And How to Achieve It, accessed February 14, 2026, https://playboxtechnology.com/2025/05/why-sub%E2%80%91second-latency-matters-in-live-streaming-and-how-to-achieve-it/
Online Learning on the Programmable Dataplane - Kyle Simpson, accessed February 14, 2026, https://mcfelix.me/docs/dissertation.pdf
Learning in situ: a randomized experiment in video streaming - USENIX, accessed February 14, 2026, https://www.usenix.org/system/files/nsdi20-paper-yan.pdf
How WebRTC's NetEQ Jitter Buffer Provides Smooth Audio ..., accessed February 14, 2026, https://webrtchacks.com/how-webrtcs-neteq-jitter-buffer-provides-smooth-audio/
BandSeer: Bandwidth Prediction for Cellular Networks | Request PDF - ResearchGate, accessed February 14, 2026, https://www.researchgate.net/publication/384730954_BandSeer_Bandwidth_Prediction_for_Cellular_Networks
BandSeer: bandwidth prediction for cellular networks - TUHH Open Research, accessed February 14, 2026, https://tore.tuhh.de/entities/publication/534c1c90-6137-4f65-b26e-e75192402457
The Ultimate Guide to AF_XDP: High Performance Networking in ..., accessed February 14, 2026, https://medium.com/@shradhesh71/the-ultimate-guide-to-af-xdp-high-performance-networking-in-rust-0a5ca9e1377a
glommio vs tokio - compare differences and reviews? - LibHunt, accessed February 14, 2026, https://www.libhunt.com/compare-glommio-vs-tokio
ZeroPool — Rust utility // Lib.rs, accessed February 14, 2026, https://lib.rs/crates/zeropool
Single instruction, multiple data - Wikipedia, accessed February 14, 2026, https://en.wikipedia.org/wiki/Single_instruction,_multiple_data
Episodes Tagged with “Fedora” - LINUX Unplugged, accessed February 14, 2026, https://linuxunplugged.com/tags/fedora/rss
How Speedify Works - Operation and Functionality Overview, accessed February 14, 2026, https://support.speedify.com/article/273-how-speedify-works
CellFusion: Multipath Vehicle-to-Cloud Video Streaming with Network Coding in the Wild - Ennan Zhai, accessed February 14, 2026, https://ennanzhai.github.io/pub/cellfusion-sigcomm23.pdf
Session Details: SMPTE Media Technology Summit 2025, accessed February 14, 2026, https://summit.smpte.org/2025/session/3347383/evolving-tr-07-tr-08-integrating-jpeg-xs-temporal-differential-coding-for-next-gen-interoperability
VSF Releases Specification for RIST Satellite-Hybrid: In Band Method, accessed February 14, 2026, https://www.rist.tv/news/2025/8/7/vsf-releases-specification-for-rist-satellite-hybrid-in-band-method
GStreamer RTP-over-QUIC implementation - General Discussion ..., accessed February 14, 2026, https://discourse.gstreamer.org/t/gstreamer-rtp-over-quic-implementation/752
STANAG 4609 – ISR Video - ImpleoTV, accessed February 14, 2026, https://impleotv.com/2025/03/11/stanag-4609-isr-video/
