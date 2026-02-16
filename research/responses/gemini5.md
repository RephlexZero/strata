Technical Report: Architectural Analysis and Strategic Roadmap for Next-Generation Open-Source Cellular Bonding (2024–2026)
1. Introduction: The Entropy of Unmanaged Networks and the Broadcast Imperative
The transmission of broadcast-quality video over unmanaged networks—specifically cellular infrastructure—represents one of the most hostile engineering challenges in modern telecommunications. Unlike fiber or satellite links, which offer deterministic bandwidth and predictable latency, cellular networks are stochastic systems governed by high entropy. Signal-to-Interference-plus-Noise Ratio (SINR) fluctuates on millisecond timescales due to multipath fading, tower congestion, and physical obstruction. In this volatile environment, the objective of a bonded video transport system is not merely to aggregate bandwidth, but to manage variance.
For the past decade, this domain has been dominated by a triad of commercial incumbents—LiveU, TVU Networks, and Dejero—who have built robust proprietary moats around their transport protocols. These systems do not simply treat modems as generic network interfaces; they integrate deep physical layer (Layer 1) feedback into transport layer (Layer 4) scheduling decisions, allowing them to predict packet loss before it occurs.
However, the period between 2024 and 2026 marks a structural inflection point. The maturation of open standards such as Secure Reliable Transport (SRT) and Reliable Internet Stream Transport (RIST), combined with the emergence of the IETF's Media over QUIC (MoQ) working group, has provided the open-source community with primitives that—for the first time—rival proprietary capabilities. Simultaneously, the ubiquity of 5G Standalone (SA) networks and the accessibility of Linux-based modem management tools (ModemManager, libqmi) enable a level of hardware control previously reserved for vertical integrators.
This report provides an exhaustive technical analysis of the existing commercial landscape, dissects the capabilities and limitations of current open standards, and proposes a comprehensive architectural blueprint for a "winning" open-source solution. It argues that to displace incumbents, an open-source project must move beyond simple link aggregation and implement a cybernetic control loop that unifies modem signal intelligence, predictive packet scheduling, and cloud-native orchestration.
2. The Incumbent Landscape: Deconstructing Proprietary Engineering Moats
To engineer a superior alternative, one must first deconstruct the mechanisms by which commercial vendors achieve "five nines" (99.999%) reliability on "best effort" networks. The analysis reveals that their success lies in the tight coupling of Forward Error Correction (FEC), Automatic Repeat reQuest (ARQ), and proprietary congestion control algorithms.
2.1 LiveU and the LRT™ (LiveU Reliable Transport) Ecosystem
LiveU retains the largest market share in the bonded cellular space, a position underpinned by its LiveU Reliable Transport (LRT™) protocol. LRT™ is not a single technique but a protocol stack designed specifically to hide the underlying complexity of LTE/5G networks from the video encoder.
2.1.1 The LRT™ Protocol Architecture
LRT™ operates on the principle that cellular links are inherently non-deterministic. Its architecture integrates four critical resilience mechanisms that function simultaneously rather than sequentially:
Packet Ordering and Reassembly: In a bonded environment, packets striped across different modems arrive at the receiver with significant timing skew (jitter). A 5G millimeter-wave link might deliver packets in 20ms, while a congested 4G LTE band 71 link might take 150ms. LRT™ employs a high-performance reordering buffer at the receiver (LiveU LU2000/4000 or Cloud Server).1 Unlike basic TCP reordering, which can stall the window, the LRT™ resequencing engine is often hardware-accelerated (using FPGAs or optimized software instructions) to handle high-throughput 4K/8K streams without inducing CPU-bound latency bottlenecks.
Integrated Control Data: A distinct architectural advantage of LRT™ is the interleaving of control data with media payloads. The protocol constantly exchanges metadata regarding modem health, Round Trip Time (RTT), and packet loss rates. This tight coupling allows the protocol to react instantaneously to link degradation. If a specific modem’s latency profile spikes, LRT™ throttles that link's queue depth immediately, preventing the "bufferbloat" phenomenon that plagues generic bonding solutions.1
Dynamic Forward Error Correction (FEC): LRT™ employs an adaptive FEC mechanism. Rather than applying a static overhead (e.g., 20% redundancy), the protocol dynamically adjusts the FEC ratio per link based on the observed Bit Error Rate (BER). This ensures that bandwidth is not wasted on clean links while providing robust protection on noisy ones.1
2.1.2 5G Network Slicing and Hardware Integration
By 2025, LiveU had heavily integrated 5G network slicing capabilities into its field units (LU800/900). These devices interface directly with carrier APIs to request specific Quality of Service (QoS) slices, prioritizing video packets over background data. This "Layer 1 to Layer 7" vertical integration—where the application layer is aware of and can influence the physical layer allocation—is a capability currently absent in standard open-source tools, which typically view the modem as a black-box Ethernet interface.2
2.2 TVU Networks: Inverse StatMux Plus (IS+) and ISX
TVU Networks differentiates itself through its "Inverse StatMux" technology, a patented approach to bandwidth aggregation that prioritizes latency minimization.
2.2.1 Inverse StatMux Plus (IS+)
Traditional statistical multiplexing (StatMux) allocates bandwidth to video channels based on the complexity of the content. TVU inverts this logic: it allocates video packets to network links based on the real-time capacity of the link.
Predictive Adaptation: The IS+ algorithm moves beyond reactive congestion control. By monitoring the first and second derivatives of RTT (velocity and acceleration of latency), the system infers impending congestion before it results in packet loss. If a modem's RTT variance (jitter) increases, IS+ proactively reduces the load on that link. This predictive capability allows TVU to maintain stability in high-mobility scenarios where signal characteristics change rapidly.3
Granular Packet Splitting: TVU’s transmission engine splits the video stream into micro-chunks. Unlike session-based load balancing, where a single flow adheres to one link, IS+ stripes a single video frame across multiple links. This ensures that even if one link fails catastrophically, the FEC overhead distributed across the remaining links allows the frame to be reconstructed without a costly retransmission request (ARQ), which is critical for their sub-second latency performance.6
2.2.2 ISX: The 0.3-Second Benchmark
In the 2024-2026 timeframe, TVU introduced ISX, an evolution of their protocol claiming a 0.3-second glass-to-glass latency even in congested environments. This performance is achieved by moving the bonding logic closer to the kernel space and utilizing deep learning models trained on vast datasets of cellular behavior. ISX can identify specific interference patterns—such as the "stadium effect," where thousands of devices compete for uplink resources—and switch modulation strategies or carrier bands instantly.5
2.3 Dejero: Smart Blending Technology
Dejero’s "Smart Blending" technology is arguably the most sophisticated regarding heterogeneous network mixing, excelling in environments that combine LEO satellite (Starlink), Wi-Fi, and Cellular.
2.3.1 Packet-Level Routing vs. Session Aggregation
Dejero differentiates itself by strictly avoiding "flow stickiness." In standard networking protocols like Multipath TCP (MPTCP), a flow often sticks to a path until it degrades significantly. Dejero’s GateWay and EnGo devices make routing decisions for every single packet.9
Asymmetric Link Handling: A core innovation in Smart Blending is the handling of highly asymmetric links. For instance, a Starlink connection may offer high download speeds but variable upload, while a 5G connection might offer high upload throughput. Smart Blending creates a virtualized pipe that masks these asymmetries, presenting a uniform, high-performance interface to the application layer.
Bufferbloat Management: Dejero actively measures the "standing queue" in the cellular modem. If the modem’s internal buffer fills up—creating latency without causing packet loss—Dejero’s algorithm detects the delay spike and artificially throttles the send rate to drain the modem buffer, thereby maintaining interactivity.10
3. The State of Open Standards: SRT and RIST in 2026
The open-source community relies primarily on SRT and RIST. While powerful, these protocols have historically lacked the "vertical integration" of commercial bonded solutions. However, developments in 2024-2026 have significantly narrowed this gap.
3.1 Secure Reliable Transport (SRT)
Developed by Haivision and open-sourced, SRT has become the de facto standard for point-to-point IP video transport. Its adoption is driven by its inclusion in major tools like FFmpeg, OBS Studio, and vMix.
3.1.1 Architecture and The "Too Late" Drop Mechanism
SRT is built on the UDT (UDP-based Data Transfer) protocol. Its defining feature is latency-bounded reliability. Every packet is stamped with a precise generation time. The receiver buffer is configured with a fixed latency (e.g., 200ms). If a packet is lost, the receiver requests a retransmission (ARQ). However, if the retransmitted packet would arrive after its scheduled playout time, SRT drops the packet to preserve the timing of the stream.
Implication for Bonding: In a bonded cellular environment, this mechanism can be aggressive. If one modem lags significantly, SRT might drop packets that could theoretically have been recovered over a faster link, simply because its scheduler is not fully "link-aware" in the way LiveU’s LRT is. The scheduler must be intimately aware of the RTT differences to make intelligent retransmission decisions.11
3.1.2 Bonding Modes: Socket Groups
As of libsrt 1.5.x and 1.6.x (2025), SRT bonding is implemented via "Socket Groups," offering three distinct modes 14:
Bonding Mode
Mechanism
Pros
Cons
Broadcast
Sends identical data payloads over all available links simultaneously.
Maximum reliability; essentially SMPTE 2022-7 with ARQ. Zero failover time.
Extremely inefficient bandwidth usage (300% overhead for 3 links). Unsuitable for cellular data caps.
Main/Backup
Designates one link as primary; switches to backup only upon failure detection.
Efficient bandwidth usage. Simple to configure.
"Cold" failover often results in visible artifacts or macro-blocking during the switch.
Balancing
Splits the stream payload across available links.
Aggregates bandwidth (e.g., 5Mbps + 5Mbps = 10Mbps). Ideal for high-quality video.
The open-source implementation (SRT_GTYPE_BALANCE) is often criticized for simplistic round-robin scheduling. It lacks the deep signal awareness to handle variable cellular RTTs effectively.

The "Balancing" mode is the holy grail for an open-source LiveU competitor. However, as of 2026, the default implementation often struggles with the "straggler problem"—if one link in the group stalls, the sender buffer fills up, potentially stalling the entire stream because the logic for dropping "too late" packets is global rather than per-link.12
3.2 Reliable Internet Stream Transport (RIST)
RIST is the broadcaster-centric alternative to SRT, developed by the Video Services Forum (VSF). While it has lower adoption in the "prosumer" space, it is technically superior for complex bonding scenarios.
3.2.1 Advanced Profile (TR-06-03) and Tunneling
RIST’s Advanced Profile supports full-stream tunneling, allowing it to carry any IP traffic (not just video). This makes it a potential replacement for VPNs in a bonding router, capable of carrying PTZ control protocols, tally lights, and intercom audio alongside the video.
Multi-Link Load Sharing: RIST supports true load sharing where a single stream is striped across multiple links. Crucially, RIST allows for a single buffer for all paths, whereas SRT (in some implementations) maintains per-link buffers. This shared buffer architecture allows RIST to handle timing skew between disparate links (e.g., Starlink vs. LTE) more gracefully.17
3.2.2 Source Adaptation (TR-06-04 Part 1)
Released and refined between 2022 and 2024, the TR-06-04 Part 1 specification is a critical development. It defines a standardized protocol for the stream receiver to provide detailed feedback (Link Quality Reports) to the source.
The "Missing Link": This feature enables the "cybernetic loop" found in commercial systems. It allows the encoder to dynamically adjust its bitrate based on the aggregate capacity of the bonded tunnel. For example, if the bonded capacity drops from 10Mbps to 6Mbps, the RIST receiver notifies the source, which then commands the encoder to drop to a lower profile. However, few open-source encoders (like OBS) have fully implemented this feedback loop, leaving a gap in the ecosystem.19
3.2.3 RIST vs. SRT Feature Comparison (2026)
Feature
SRT (libsrt 1.6+)
RIST (Advanced Profile)
Underlying Protocol
UDT over UDP
RTP over UDP
Header Overhead
Low (custom header)
Medium (RTP headers can add 5-10%)
Bonding Architecture
Socket Groups (Broadcast/Backup/Balance)
Native Multi-Link (SMPTE 2022-7 & Load Share)
Retransmission Logic
Selective ARQ (Timestamp-based)
NACK Bitmask (Range-based, efficient for bursts)
Encryption
AES-128/256
DTLS (Certificate/PSK) & AES-256
Multicast Support
Ingest only; playout requires relay
Native Multicast & Stream IP Preservation
Source Adaptation
Custom implementation required
Standardized (TR-06-04 Part 1)

4. The Zixi Protocol: The Gold Standard of Reliability
While LiveU dominates the "First Mile" (camera to cloud), Zixi dominates the "Mid Mile" (cloud to cloud/station). Zixi is a software-defined solution that offers insights into what a premium transport layer looks like.
4.1 Proprietary Resilience: "DNA Alignment"
Zixi’s patent-pending "DNA sequence alignment" algorithm represents a major leap in failover technology. Traditional SMPTE 2022-7 failover requires two RTP streams to be bit-exact clones (identical sequence numbers). In the real world, streams from different encoders or paths often differ slightly.
Pattern Matching: Zixi’s algorithm aligns streams that are not identical at the packet level. It analyzes the payload content (the "DNA") to align disparate streams and reconstruct a seamless output. This allows for hitless failover between completely independent contribution paths—for example, a primary SRT feed and a backup RTMP feed—a capability that currently has no open-source equivalent.22
4.2 Dynamic Latency and Machine Learning
Zixi’s protocol continually adjusts the receiver buffer size (Dynamic Latency). In 2026, this has evolved to use machine learning models that distinguish between transient jitter (which requires a temporary buffer increase) and structural network degradation (which requires a bitrate drop). This dynamic sizing is critical for maintaining the lowest possible latency without risking frame drops, effectively keeping the buffer "right-sized" at all times.24
5. The QUIC Revolution: The Future of Media Transport
While SRT and RIST fight the battles of today, the war for 2026 and beyond is shifting toward QUIC. The IETF's standardization of Media over QUIC (MoQ) represents a paradigm shift that solves many of the inherent limitations of TCP and UDP bonding.
5.1 Media over QUIC (MoQ)
Standardized by the IETF (with active drafts in 2026), MoQ is designed to replace both HLS/DASH (for scale) and WebRTC (for latency). It utilizes QUIC streams and datagrams to transport media objects.
5.1.1 Elimination of Head-of-Line (HoL) Blocking
In TCP (and legacy RTMP), one lost packet stalls the entire delivery queue until it is retransmitted. In QUIC, streams are independent. If an audio packet is lost on Stream A, video frames on Stream B continue to be processed.
Relevance to Bonding: This is vital for cellular bonding where one modem might drop packets while another is healthy. MoQ allows the application to continue rendering valid data from healthy paths without waiting for the "straggler" packets from the degraded path, fundamentally solving the "stalling" issue seen in SRT balancing.11
5.1.2 The Relay Architecture
MoQ introduces the concept of "Relays" rather than simple servers. A Relay caches and forwards named objects. For an open-source bonding solution, a MoQ Relay acting as the cloud receiver offers native "fan-out" capabilities. An edge node can send one bonded stream to the cloud, and the Relay fans it out to YouTube, Twitch, and a multiviewer with practically zero added latency, leveraging the "publish/subscribe" model.11
5.2 MP-QUIC (Multipath QUIC)
This is the most promising technology for building a LiveU competitor. MP-QUIC extends QUIC to support multiple active network paths simultaneously.
5.2.1 User-Space Agility
Unlike MPTCP, which requires kernel patching (historically difficult to deploy and manage), QUIC lives in user space (e.g., quic-go, mvfst, aioquic). This means a bonding application can be shipped as a single binary without modifying the host OS. This lowers the barrier to entry for users and simplifies updates.27
5.2.2 Next-Generation Schedulers
The effectiveness of MP-QUIC depends entirely on the packet scheduler. Recent research (2024-2025) has produced schedulers far superior to round-robin:
MinRTT: Sends packets on the link with the lowest latency. Good for stability but wastes bandwidth on slower links.28
BLEST (Blocking Estimation): Estimates if sending a packet on a slow path will cause head-of-line blocking at the receiver. If the delay is predicted to be too high, it skips the slow path even if it has bandwidth available. This is ideal for heterogeneous cellular links.28
ECF and Peekaboo: Learning-based schedulers that adapt to channel conditions using reinforcement learning. Implementing these in an open-source tool would provide "Smart Blending" capabilities similar to Dejero.28
5.2.3 BBRv3 Congestion Control
In 2025-2026, Google’s BBRv3 (Bottleneck Bandwidth and Round-trip propagation time) congestion control algorithm has become the standard for high-performance QUIC. Unlike loss-based algorithms (CUBIC) which fill buffers until packets drop, BBRv3 models the network pipe to find the maximum bandwidth with the minimum RTT. This explicit congestion model fights bufferbloat, ensuring that cellular modems are utilized efficiently without inducing the latency spikes that kill live video.30
6. Gap Analysis: Why Open Source Hasn't "Won" Yet
Despite the availability of powerful protocols like SRT, RIST, and QUIC, no open-source project has successfully displaced LiveU for mission-critical broadcast. The failure is not in the transport protocol, but in the system integration and modem intelligence.
6.1 The Modem Management Gap: The "Black Box" Problem
Commercial units communicate with cellular modems at the chipset level (Qualcomm/Sierra Wireless/Telit). They do not just open a socket; they act as a telecom supervisor.
The Gap: Most open-source scripts (e.g., using ip route or mwan3) treat LTE modems as generic Ethernet interfaces. They only know a link is bad after a packet is lost.
The Requirement: A "winning" solution must implement a daemon that queries the modem via QMI (Qualcomm MSM Interface) or MBIM (Mobile Broadband Interface Model) to monitor:
RSRP (Reference Signal Received Power): The signal strength.
RSRQ (Reference Signal Received Quality): The signal quality (noise/interference).
SINR (Signal-to-Interference-plus-Noise Ratio): The truest measure of potential throughput.
CQI (Channel Quality Indicator): Feedback from the tower on modulation coding schemes. LiveU knows a link is about to go bad because the SINR dropped, and preemptively routes packets away. Open source needs this "look-ahead" capability.33
6.2 The "Dumb" Scheduler Problem
SRT's native bonding scheduler is often "reactive." It measures RTT via handshake probes. In cellular networks, RTT is volatile due to bufferbloat. A "dumb" scheduler sees a low RTT on an idle link, dumps a burst of video packets, fills the modem's buffer, causes latency to spike to 2 seconds, and then the protocol panics and drops the connection. Requirement: An "Active Queue Management" (AQM) approach at the sender side, modeled after BBRv3, which explicitly models the bottleneck bandwidth to avoid overfilling the modem buffer.30
6.3 The Orchestration Gap (Cloud Control)
Zixi has ZEN Master. LiveU has LiveU Central. These are not just video players; they are fleet management tools. They allow engineers to:
Remotely configure bitrate and latency.
Geo-locate units on a map.
Manage cloud receiver instances (autoscaling). Current open-source alternatives (like managing srt-live-server via command line) scale poorly. A winning solution needs a web-based control plane, potentially built on Kubernetes for autoscaling the ingest layer.37
7. Strategic Roadmap: Building the "OpenBond" Solution
To win, an open-source project must move beyond being a library and become a platform. Below is a proposed architecture for a system (let's call it "OpenBond") that synthesizes the best of 2026 technology.
7.1 Architecture Overview
The system consists of three distinct but interconnected components:
The Edge Node (Sender): A Linux-based ruggedized computer (e.g., NVIDIA Jetson, Intel NUC) equipped with multiple LTE/5G modems.
The Cloud Gateway (Receiver): A scalable bonding aggregator, re-assembler, and re-streamer.
The Control Plane: A centralized web dashboard for telemetry, configuration, and orchestration.
7.2 Component 1: The "Signal-Aware" Edge Node
Instead of relying on the OS's default routing table, the Edge Node runs a custom user-space networking stack that bypasses standard kernel limitations.
7.2.1 The Link Supervisor Daemon
This service, likely written in Rust or Go for performance and safety, interacts directly with ModemManager or the /dev/cdc-wdm0 QMI device.
Polling Cycle: It polls RSRP, RSRQ, SINR, and CQI every 100ms.
Scoring Algorithm: It calculates a real-time "Link Health Score" (0-100) for each modem.
Concept Formula:

Feedback Loop: This score is fed directly into the Transport Scheduler via shared memory or a lightweight RPC, allowing the scheduler to weight packet distribution based on physics, not just ping.35
7.2.2 The Transport Layer: MP-QUIC with "BBR-Bond"
We utilize MP-QUIC (using a library like quic-go, msquic, or aioquic) as the bonding tunnel. Inside this reliable QUIC tunnel, we carry the video stream (MPEG-TS or raw frames).
Congestion Control: Implement BBRv3 per path. BBRv3 is non-negotiable because it seeks bandwidth and minimum RTT, explicitly fighting the bufferbloat common in LTE/5G uplinks. It ensures that the modem queues remain empty, preserving the low latency required for live interaction.31
Predictive Scheduler: Implement a scheduler inspired by BLEST or Peekaboo. It uses the "Link Health Score" from the Supervisor Daemon. If the SINR drops on Modem A (indicating a car drove behind a building), the scheduler reduces the weight of Modem A before packet loss occurs, diverting traffic to Modem B.28
7.3 Component 2: The Cloud Aggregator
The receiver must be more than a simple SRT listener; it must be a media processing hub.
Reassembly Engine: Uses a jitter buffer that dynamically resizes based on the aggregate jitter of all paths (similar to Zixi's dynamic latency).
MoQ Relay Integration: The output of the bonding tunnel is fed into a Media over QUIC (MoQ) Relay.
Value Add: This allows the ingest node to serve the video to 1, 10, or 10,000 viewers (e.g., production staff, multiviewers) via WebTransport directly in the browser. This achieves sub-second latency for monitoring without the computational cost of transcoding to HLS/DASH.26
Protocol Bridge: The Aggregator outputs standard SRT (for hardware decoders), RTMP (for social platforms), and NDI (for cloud production tools like vMix or Grass Valley AMPP).
7.4 Component 3: The Orchestration Layer
Telemetry Stream: The Edge Node sends a telemetry stream (bitrate, battery voltage, CPU temp, modem stats) via a lightweight QUIC stream to the control plane.
Remote Configuration: The control plane can push configuration changes (e.g., "Switch to Low Latency Mode," "Lock Modem 1 to Band 71") which updates the Edge Node's Scheduler parameters in real-time.
Kubernetes Autoscaling: The Cloud Gateway is deployed on Kubernetes. If 50 field units come online simultaneously for a large event, the K8s Horizontal Pod Autoscaler (HPA) detects the CPU/Network load and spins up more Aggregator pods automatically.37
8. Detailed Technical Recommendations for Implementation
8.1 Addressing the "Hardware Gap"
Commercial units use custom antenna arrays to prevent RF interference between modems. An open-source hardware reference design must provide guidelines to mitigate this:
Antenna Isolation: Guidelines for spatial diversity (e.g., keeping MIMO antennas >1/2 wavelength apart) to prevent near-field coupling.
Frequency Band Locking: The Link Supervisor should include logic to "spread" modems across bands. For example, instruct Modem A to lock to Band 71 (600MHz) for coverage and Modem B to lock to Band 41 (2.5GHz) for capacity. This prevents multiple modems from competing for the same resource blocks on the same tower sector—a standard feature in LiveU but manual in most open setups.36
8.2 The "Killer Feature": Closed-Loop Source Adaptation
The system must implement a closed feedback loop between the Cloud Aggregator and the Video Encoder (e.g., OBS/FFmpeg).
Detection: The Cloud Aggregator detects that the aggregate bandwidth of the bonded tunnel is dropping below the target bitrate.
Notification: It sends a "Link Quality Report" (following the RIST TR-06-04 model) back to the Edge Node.
Action: The Edge Node interprets this report and sends a command to the Encoder (via API, e.g., OBS WebSocket) to lower the video bitrate on the fly. Result: The video quality decreases momentarily, but the stream never freezes or stutters. This "graceful degradation" is the hallmark of professional bonding.19
8.3 Security Considerations
DTLS 1.3: Mandatory encryption for both the control and data plane to prevent stream hijacking.
Token Authentication: Implementation of the "Common Access Tokens" scheme proposed in IETF MoQ drafts. This allows for simple but secure granting of publish/subscribe rights without managing complex PKI infrastructures.11
9. Conclusion: The Path to Victory
To displace LiveU, TVU, and Dejero, an open-source solution cannot simply be "SRT with multiple IP addresses." It must emulate the cybernetic nature of commercial systems—where the physical layer (modems), transport layer (bonding), and application layer (video encoding) operate as a single, tightly coupled control loop.
The Winning Formula for 2026:
Adopt MP-QUIC as the transport core for its user-space flexibility, pluggable schedulers, and BBRv3 congestion control.
Build a "Modem Supervisor" daemon to inject real-time RF intelligence (SINR, CQI) into the packet scheduler, enabling predictive routing.
Implement MoQ for scalable, low-latency cloud distribution and monitoring via WebTransport.
Create a Web-Based Control Plane to rival ZEN Master, offering fleet management and orchestration.
By focusing on these four pillars, an open-source project can offer 95% of the performance of a $25,000 commercial unit for the cost of off-the-shelf hardware, fundamentally disrupting the broadcast economics of the next decade.
Citations Table

Category
Source IDs
SRT & RIST
12
Zixi Protocol
22
Commercial Bonding (LiveU/TVU/Dejero)
1
QUIC, MP-QUIC & MoQ
11
Cellular Physics & Modem Mgmt
33
Cloud & Orchestration
37
Machine Learning/Schedulers
13

Works cited
LiveU Reliable Transport (LRT) Video Transmission Protocols, accessed February 16, 2026, https://www.liveu.tv/solutions/lrt
5G IP-Bonding Solutions: The New World Of Video Broadcasting - LiveU, accessed February 16, 2026, https://www.liveu.tv/resources/blog/next-generation-5g-ip-bonding-and-the-new-world-of-production
Fremont Fire | Real-time Drone Video | UAV Program | TVU One, accessed February 16, 2026, https://www.tvunetworks.com/story/tvu-fremont-fire-drone-live-video/
TVU Networks | EquityNet, accessed February 16, 2026, https://www.equitynet.com/c/tvu-networks
Deep dive into cellular bonding protocols: why Inverse StatMux beats traditional virtual pipe aggregation : r/BroadcastingTechology - Reddit, accessed February 16, 2026, https://www.reddit.com/r/BroadcastingTechology/comments/1p73q1d/deep_dive_into_cellular_bonding_protocols_why/
UL - 8K - Backhaul Whitepaper Clean Version | PDF | Data Compression - Scribd, accessed February 16, 2026, https://www.scribd.com/document/662937817/UL-8K-backhaul-whitepaper-clean-version-1
Media Organizations are Switching to Cellular Bonded Solutions, accessed February 16, 2026, https://www.tvunetworks.com/guides/why-media-organizations-are-switching-to-cellular-bonding-solutions-and-you-should-too/
The Unbundling of the Broadcast Truck: A Technical Analysis of, accessed February 16, 2026, https://dev.to/jason_jacob_dcfc2408b7557/the-unbundling-of-the-broadcast-truck-a-technical-analysis-of-remi-cloud-and-ai-in-modern-sports-4oid
Smart Blending Technology - Dejero, accessed February 16, 2026, https://www.dejero.com/smart-blending-technology/
Smart Blending Technology - Dejero, accessed February 16, 2026, https://www.dejero.com/wp-content/uploads/2025/04/Dejero-Smart-Blending-Technology.pdf
What's the deal with Media Over QUIC? - IETF, accessed February 16, 2026, https://www.ietf.org/blog/moq-overview/
SRT Protocol TechnicalOverview DRAFT 2018-10-17 PDF - Scribd, accessed February 16, 2026, https://www.scribd.com/document/484512415/SRT-Protocol-TechnicalOverview-DRAFT-2018-10-17-pdf
A Decentralized Multi-Venue Real-Time Video Broadcasting System ..., accessed February 16, 2026, https://www.mdpi.com/2076-3417/15/14/8043
srt/docs/features/socket-groups.md at master · Haivision/srt · GitHub, accessed February 16, 2026, https://github.com/Haivision/srt/blob/master/docs/features/socket-groups.md
SRT Connection Bonding: Quick Start - GitHub, accessed February 16, 2026, https://github.com/Haivision/srt/blob/master/docs/features/bonding-quick-start.md
srt/docs/features/bonding-intro.md at master · Haivision/srt - GitHub, accessed February 16, 2026, https://github.com/Haivision/srt/blob/master/docs/features/bonding-intro.md
2025: RIST vs SRT Comparison — RIST Forum, accessed February 16, 2026, https://www.rist.tv/articles-and-deep-dives/2025-rist-vs-srt-comparison
Reliability Doesn't Happen by Chance - SipRadius, accessed February 16, 2026, https://sipradius.com/news-events/reliability-doesnt-happen-by-chance/
RIST Releases Specification for Source Adaptation — RIST Forum, accessed February 16, 2026, https://www.rist.tv/news/2022/11/8/rist-releases-specification-for-source-adaption
The Latest Updates to SRT and RIST - RIST Forum, accessed February 16, 2026, https://www.rist.tv/articles-and-deep-dives/2023/1/9/the-latest-updates-to-srt-and-rist
Preamble to Video Services Forum (VSF) Technical Recommendation TR-06-4 Part 1, accessed February 16, 2026, https://static.vsf.tv/download/technical_recommendations/VSF_TR-06-4-Part-1_2022-11-01.pdf
Zixi Announces Robust Hitless Failover Between All Types of IP ..., accessed February 16, 2026, https://zixi.com/news/zixi-announces-robust-hitless-failover-between-all-types-of-ip-streams/
Ultra-Low Latency Delivery - Zixi, accessed February 16, 2026, https://zixi.com/ultra-low-latency-delivery/
A zixi ApproAch: - Dynamic Latency Management in Video Streaming Networks, accessed February 16, 2026, https://zixi.wpenginepowered.com/wp-content/uploads/2024/01/Whitepaper_Zixi-Dynamic-Latency.pdf
Latency Considerations - Zixi Current Version, accessed February 16, 2026, https://docs.zixi.com/zixi-broadcaster-zec-current-version/latency-considerations
MoQ: Refactoring the Internet's real-time media stack - The Cloudflare Blog, accessed February 16, 2026, https://blog.cloudflare.com/moq/
Increasing Throughput & Redundancy with Multipath QUIC - Chair of Network Architectures and Services, accessed February 16, 2026, https://www.net.in.tum.de/fileadmin/TUM/NET/NET-2024-09-1/NET-2024-09-1_03.pdf
Evaluating Mpquic Schedulers in Dynamic Wireless Networks with 2d and 3d Mobility, accessed February 16, 2026, https://papers.ssrn.com/sol3/papers.cfm?abstract_id=4654240
Cost optimized multipath scheduling in 5G for Video-on-Demand, accessed February 16, 2026, https://www.researchgate.net/publication/351406389_Cost_optimized_multipath_scheduling_in_5G_for_Video-on-Demand_traffic
BBR Congestion Control - IETF, accessed February 16, 2026, https://www.ietf.org/archive/id/draft-ietf-ccwg-bbr-00.html
Current State of BBR Congestion Control - TUM, accessed February 16, 2026, https://www.net.in.tum.de/fileadmin/TUM/NET/NET-2025-05-1/NET-2025-05-1_17.pdf
Insights into BBRv3's Performance and Behavior by Experimental Evaluation, accessed February 16, 2026, https://publikationen.bibliothek.kit.edu/1000182022/168677048
ModemManager: Signal-/cell information report functionality - Stack Overflow, accessed February 16, 2026, https://stackoverflow.com/questions/58643673/modemmanager-signal-cell-information-report-functionality
How to get up to date signal strength of Cellular Modem through ConnectCore6 - Linux, accessed February 16, 2026, https://forums.digi.com/t/how-to-get-up-to-date-signal-strength-of-cellular-modem-through-connectcore6/19825
SINR, RSRP, RSSI AND RSRQ MEASUREMENTS IN LONG TERM EVOLUTION NETWORKS - OPUS at UTS, accessed February 16, 2026, https://opus.lib.uts.edu.au/rest/bitstreams/1e4a4206-eafe-4ee7-a9d3-13195aeef2bf/retrieve
RSRP and RSRQ - Teltonika Networks Wiki, accessed February 16, 2026, https://wiki.teltonika-networks.com/view/RSRP_and_RSRQ
Kubernetes Architecture : let's design for a video streaming app at 1 million users - Dev.to, accessed February 16, 2026, https://dev.to/kaustubhyerkade/kubernetes-architecture-for-a-video-streaming-app-at-1-million-users-2168
How to Auto Scale Kubernetes for Streaming Server? Easy and Quick Guide, accessed February 16, 2026, https://antmedia.io/auto-scaling-streaming-server-with-kubernetes/
ZEN Master for Control, Monitoring & Orchestration of Live Video ..., accessed February 16, 2026, https://zixi.com/zen-master-control-plane/
Launching a Live Streaming Website on Kubernetes: A Step-by-Step Guide - Medium, accessed February 16, 2026, https://medium.com/@shamsfiroz/launching-a-live-streaming-website-on-kubernetes-a-step-by-step-guide-2be3a893d4b4
[OpenWrt Wiki] How to use an LTE modem in QMI/MBIM mode for WWAN connection, accessed February 16, 2026, https://openwrt.org/docs/guide-user/network/wan/wwan/ltedongle
Modem Commands for MBIM and QMI Composition - The Wireless Haven, accessed February 16, 2026, https://thewirelesshaven.com/modem-commands-for-mbim-and-qmi-composition/
RIST and SRT overview: what to choose and why - IABM, accessed February 16, 2026, https://theiabm.org/rist-and-srt-overview-what-to-choose-and-why/
Cloud ingest of live video - Net Insight, accessed February 16, 2026, https://netinsight.net/wp-content/uploads/2022/10/White-paper-Cloud-ingest-of-live-video-%E2%80%93-An-open-approach-to-RIST-SRT-and-retransmission-protocols.pdf
Understanding and Managing Latency in Live Streaming - Zixi, accessed February 16, 2026, https://zixi.com/demystifying-video-streaming-latency-a-comprehensive-guide/
ULTRA-LOW LATENCY DELIVERY OVER IP, accessed February 16, 2026, https://zixi.wpenginepowered.com/wp-content/uploads/2020/06/Zixi-Ultra-Low-Latency-Delivery.pdf
US9154247B2 - Live uplink transmissions and broadcasting management system and method - Google Patents, accessed February 16, 2026, https://patents.google.com/patent/US9154247B2/en
IP Bonding Explained: Why It Matters - LiveU, accessed February 16, 2026, https://www.liveu.tv/resources/blog/ip-bonding-explained-why-it-matters
On the Potential of MP-QUIC as Transport Layer Aggregator for Multiple Cellular Networks, accessed February 16, 2026, https://www.mdpi.com/2079-9292/11/9/1492
draft-ietf-quic-multipath-19 - Managing multiple paths for a QUIC connection, accessed February 16, 2026, https://datatracker.ietf.org/doc/draft-ietf-quic-multipath/
QUIC & MPQUIC @ PANLAB - UVic Online Academic Community, accessed February 16, 2026, https://onlineacademiccommunity.uvic.ca/quic/
