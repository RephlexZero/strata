SRT (Secure Reliable Transport)
Architecture: SRT (open-sourced by Haivision) is a UDP-based transport with built‑in reliability and
security. It uses a sliding-window ARQ: each data packet carries a sequence number and is ACKed or
NACKed. Unlike RIST (which uses only NACKs), SRT uses both ACK and NACK frames for retransmission
control . Lost packets are detected via NACKs and then resent, but SRT also employs a
“too‑late‑to‑send” drop: if the receiver’s playout window has advanced past an unsent packet, the
sender will drop it from the buffer (the TLPKTDROP feature) . SRT supports three handshake modes:
Caller (active opener), Listener (passive), and Rendezvous (for NAT traversal). In Rendezvous mode,
both ends bind/listen on the same port so they can connect through symmetric NATs . (See
figure below: in Caller mode one side initiates, Listener waits; Rendezvous lets two NATed peers connect
without pre-established roles.)
Fig. 1: SRT’s handshake modes – Caller (active) versus Listener (passive) endpoints, and Rendezvous for NAT
traversal .
Encryption: SRT includes built-in AES encryption. It uses AES in counter mode (AES‑CTR) with a shortterm session key for the media stream . (Haivision’s spec explains that packet sequence numbers
indicate which AES key to use and that AES‑CTR is applied to the payload .) Because QUIC (and
therefore RTP-over-QUIC) is secure by default, SRT’s SRTP layer can be seen as redundant in those
contexts .
Known Weaknesses: SRT performs very well at low-to-moderate loss but can become inefficient under
heavy loss. As one analysis notes, beyond ~10–15% loss SRT’s ARQ can “flood” the network – it may
double the outgoing rate to recover packets, whereas RIST (with optimized NACK-only ARQ and optional
selective dropping) maintains efficiency . SRT also lacks a dynamic congestion-control loop in
Live Mode (it uses a simple pacing only) and cannot directly do multipoint or multicast transport. Early
SRT had no native bonding: until version 1.5 (still pending as of 2026) SRT only supported fallback (1+1
redundancy). The SRT Alliance (with hundreds of members) has promised true link bonding in upcoming
1
2
3 4
3 4
5
5
6
1 7
1
releases , but today SRT bonding is limited compared to commercial solutions. In practice, many
users find SRT misses higher-level features (like automatic encoder adaptation or cellular radio
management). This partly motivated the creation of RIST: an open-spec RTP-based alternative
designed by multiple vendors that adds features like multicast streaming, flexible profile use, and
lighter-weight ARQ .
Zixi (Proprietary Protocol)
Zixi’s protocol is closed-source and proprietary, but company literature highlights several key features. It
is a hybrid ARQ+FEC system: every stream uses both packet retransmission and adaptive forwarderror-correction coding to recover losses . The protocol is network-aware: it continuously monitors
path conditions and dynamically adjusts FEC overhead and packet scheduling. For example, Zixi claims
it can operate with as little as one frame of buffer (≈30ms) for low latency , dynamically expanding
the buffer only if packet loss grows. Zixi also employs built-in bonding: streams are sent redundantly
over multiple IP paths, and on the receive side the system reconstructs a single coherent output. Their
patented hitless failover is noteworthy – Zixi uses a “DNA sequence alignment” algorithm (patentpending) to align and merge packets from multiple streams without interrupting playback . In
essence, fragments from active and backup streams are merged in-flight so that a failed link “slides
away” seamlessly.
Zixi advertises sub-second latencies, enabled by its ARQ+FEC hybrid and dynamic buffering. While exact
benchmarks are undisclosed, marketing materials note “ultra-low latency” delivery and one-frame
buffering . (In practice, “sub-frame” may refer to this ~30ms frame-level buffer, which is lower
than conventional 1–2s OTT or HLS delays.) Zixi holds patents on its FEC, bonding and failover methods.
The company has published white papers and datasheets (e.g. “Ultra-Low Latency Delivery”) describing
features like rateless FEC and congestion avoidance, but no independent benchmarks exist. In short,
Zixi’s protocol uses all the tricks (adaptive FEC, ARQ, multistream bonding, congestion feedback, etc.)
along with proprietary enhancements (patented sequence alignment failover ) to maximize
reliability.
LiveU, TVU, and Dejero (Commercial Bonding Products)
These vendors offer complete cellular-bonding systems (hardware + service) with proprietary protocols.
Broadly they all use multiple 4G/5G modems + Wi-Fi/ethernet, split video packets across links, and
implement retransmissions/FEC at the stream level. Key points:
LiveU (LRT™) – LiveU developed and patented the concept of cellular bonding. Their “LiveU
Reliable Transport” (LRT™) protocol splits (bonds) IP traffic across all links and applies multiple
resilience layers simultaneously: packet-ordering, ACK/NACK resend, dynamic FEC, and even
adaptive bitrate encoding . LiveU gear ranges from portable units (Solo PRO kits with 2 or 4
cellular modems ) up to rackmount systems with 6–10 modems plus Ethernet/Wi-Fi. For
instance, the Solo PRO Connect “Quattro” kit provides up to 6 connections (4 cellular + Ethernet +
Wi-Fi) . LiveU’s protocol is optimized for fluctuating cellular links and is bi-directional. While
live details are proprietary, LRT has very low failover (sending redundant packets means a
broken link simply stops forwarding, with next packets coming via other modems). LiveU claims
“highly accurate end-to-end latency” and emphasizes interoperability across networks .
TVU Networks – TVU One boxes use TVU’s Inverse StatMux Plus (IS+) protocol. Like LiveU, TVU
aggregates multiple links (up to 6 embedded 5G/LTE modems in One; plus external Ethernet, Wi-Fi,
etc.). It advertises combining up to 12 IP connections (6 cellular + 4 Wi-Fi + wired/satellite) .
TVU IS+ is patented: it monitors each link independently and applies rateless FEC (RaptorQ)
8
9 1
10
11
10 12
11 10
10 12
•
13
14
15
13
•
16 17
2
rather than ARQ . This eliminates retransmission delay – TVU reports “as low as 0.3 seconds
glass-to-glass latency on cellular only” , far better than typical ~0.5–1s on competitors. The
ISX system dynamically adjusts FEC rate per link and rebalances packets in real-time (e.g. shifting
traffic away from a congested carrier before video quality drops ). TVU’s solution is aimed
at extremely low latency use-cases.
Dejero – Dejero’s products (EnGo mobile transmitter, Titan routers, etc.) use Smart Blending™.
This patented technology routes individual packets over the “best” available links in real time
, blending total bandwidth rather than simple backup. For example, the Dejero TITAN (for
vehicle/field use) has 3×5G modems (and 14 antennas) . The portable EnGo 3 has 4×3G/
4G/5G modems . Dejero gear includes hardware H.265 encoding and AES‑256 encryption
, and advertises “glass-to-glass latency as low as 0.5 seconds” over bonded cellular . In
practice, Dejero continuously “blends” all active links and encodes continuously, whereas many
older systems only send keyframes across all links or wait to switch. Dejero’s devices also offer
features like IFB (return talkback) and VPN data pass-through.
In summary, all three use multiple modems (typically 2–6) at the IP level (layer 3), not separate “per-link”
streams. They apply dynamic error correction (often RaptorQ FEC and/or ARQ) and adaptive bitrate.
Failover is effectively instant because every packet appears on at least one link; when a link drops, the
system just omits it, with no need to re-establish a new path. Note that each vendor also maintains
many patents on their specific algorithms (for instance, LiveU and Dejero emphasize their patented
bonding/scheduling methods, TVU on RaptorQ), though those internal details aren’t public.
QUIC-Based Media Transport (RoQ, MoQ, WebTransport)
Recent IETF work is creating QUIC-based media layers that could serve as alternatives to UDP/SRT. “RTP
over QUIC” (RoQ) is an Internet-Draft that defines how to encapsulate RTP/RTCP streams into QUIC
packets . RoQ leverages QUIC’s built-in TLS security and multiplexed streams. It can use QUIC’s
DATAGRAM extension so that media need not wait for retransmissions (avoiding head-of-line blocking)
. In practice, RoQ would let any RTP-based system run over QUIC (potentially with existing signaling)
while gaining NAT-friendliness and integrated ACKs. In parallel, the “Media over QUIC” (MoQ) draft
defines a publish/subscribe model: media producers push on a QUIC connection and many subscribers
receive via intermediate distribution nodes . MoQ (and related WebTransport on HTTP/3) aims for
large-scale low-latency distribution.
QUIC advantages: TLS 1.3 encryption (so no separate SRTP), flexible streams, fast connection setup (1-
RTT or 0-RTT), and built-in congestion control. On the downside, QUIC’s congestion control (like TCP
variants) may throttle aggressively on lossy cellular links, possibly limiting throughput compared to an
unconstrained UDP sender; one can choose algorithms (BBR, etc.) but it’s complex. Also, pure QUIC
introduces a small handshake delay and more CPU usage than raw UDP. Importantly for bonding, the
IETF has an active “MP‑QUIC” draft which would allow one QUIC session to use multiple network
paths simultaneously. If adopted, multipath-QUIC could natively unify multiple cellular interfaces under
one QUIC transport – potentially simplifying bonding. In sum, building on QUIC means trading
some raw performance for ease-of-use (encryption/NAT) and future internet compatibility.
18
19
20 21
•
22
23 24
25 25
26 25
27
28
29
30
30
3
Opportunities for an Open-Source Solution
Current OSS protocols (SRT, RIST, etc.) cover basic reliability but lack many high-level features that
broadcasters want. Key gaps to address include:
Robust Multi-path Bonding: Unlike LiveU/TVU/Dejero’s always-on bonding, SRT and RIST have
limited support. RIST offers “load sharing” RTP streams and SRT 1.5 has rudimentary multiplex
failover , but neither routinely splits live traffic over N links with dynamic switching. An open
system should support N>2 links, seamless path switching, per-link quality monitoring, and
perhaps predictive re-routing as TVU/Dejero do.
Adaptive Encoding Integration: Commercial systems often tie the transport to the encoder. For
example, LiveU’s LRT can signal encoders to reduce bitrate if losses rise . Open-source users
often must handle encoding separately (e.g. ffmpeg hardware encoders). A winning OSS solution
might include a feedback channel to adjust encoding parameters (resolution/bitrate) in response
to link changes, or integrate with adaptive codecs.
Cellular Modem Management: SRT/RISt treat modems as generic IP links; they don’t monitor
signal strength or manage SIMs. In contrast, devices like the Dejero TITAN actively read RSRP/
RSSI and can instruct the operator when to swap SIM cards, etc. An OSS alternative could include
a modem-management layer (using AT commands or Linux WWAN APIs) to monitor link health,
log carrier quality, and even implement automatic failover based on signal.
Monitoring and Alerting: Out of the box, RIST and SRT provide few user-facing metrics. In fact,
Sony recently released a Prometheus exporter for SRT – underscoring that monitoring was
needed. An open solution should export rich stats (loss rate, latency, throughput per link,
buffering) via standard APIs (Prometheus, MQTT, etc.) so operators can visualize performance
and get alerts. (This includes cloud metrics for autoscaling receivers if deployed in Kubernetes.)
Cloud-Scale Orchestration: Commercial vendors offer managed cloud ingest or auto-scaling
decoder farms. OSS tools could similarly enable automatic scaling of receive endpoints (e.g. spin
up new listeners on demand), and integrate with cloud CDNs or multicast. Also helpful would be
built-in support for rendezvous (many-to-many rendezvous mode like SRT) so that many
senders/receivers can connect without fixed IPs.
Reliability Features: Additional high-end features are often cited: redundant redundant path
“glue” (like separate TCP/IP-over-satellite fallback), built-in IFB/talkback, stream encryption keys
management (beyond AES-CTR), and simplified NAT traversal.
In summary, the open-source alternative should adopt the best of both worlds: use robust ARQ/FEC
bonding like the commercial systems (perhaps via an evolving multipath-QUIC or enhanced RIST), but
remain flexible and lightweight. It should build in missing pieces – especially multiplexed bonding
across many links, automatic bitrate adaptation, and modern observability – to outpace the older SRT/
RIST stacks. Doing so would meet live-contribution needs (e.g. racecar telemetry) with the reliability of a
bonded transmitter while leveraging open standards and low-cost edge devices.
Sources: Architecture and protocol details are drawn from official specs and technical overviews
, vendor literature and datasheets , and IETF drafts . The Sony
Prometheus exporter highlights monitoring needs . Together, these paint the current landscape
(2024–2026) of bonded cellular streaming.
•
8
•
13
•
•
31
•
•
2 5
3 1 8 13 14 19 10 24 27 29 30
31
4
RIST and SRT overview: what to choose and why | Elecard: Video Compression Guru
https://www.elecard.com/page/article_rist_vs_srt
Secure Reliable Transport (SRT) Protocol Technical Overview
https://ossrs.net/lts/zh-cn/assets/files/SRT_Protocol_TechnicalOverview_DRAFT_2018-10-17-
e74c1ba00b24e7149bf46826be46cc62.pdf
SRT Connection Modes
https://doc.haivision.com/SRT/1.5.3/Haivision/srt-connection-modes
draft-ietf-avtcore-rtp-over-quic-09
https://datatracker.ietf.org/doc/html/draft-ietf-avtcore-rtp-over-quic-09
Ultra-Low Latency Delivery - Zixi
https://zixi.com/ultra-low-latency-delivery/
Latency Considerations - Zixi Current Version
https://docs.zixi.com/zixi-broadcaster-zec-current-version/latency-considerations
Zixi Announces Robust Hitless Failover Between All Types of IP Streams - Zixi
https://zixi.com/news/zixi-announces-robust-hitless-failover-between-all-types-of-ip-streams/
LiveU Reliable Transport (LRT) Video Transmission Protocols
https://www.liveu.tv/solutions/lrt
Solo PRO Connect: modems, unlimited data plans, and LRT™
https://www.liveu.tv/products/create/liveu-solo/solo-pro-connect
TVU_One_V4_Data_Sheet 2025
https://www.tvunetworks.com/wp-content/uploads/2023/09/TVU_One_V4_Data_Sheet-2025.pdf
Transmission Protocol Architecture: The Crucial Factor in Cellular Bonding Performance |
by Tse Kevin | Dec, 2025 | Medium
https://medium.com/@kevintse756/transmission-protocol-architecture-the-crucial-factor-in-cellular-bondingperformance-85580dff6c1c
TITAN - Dejero
https://www.dejero.com/titan/
Dejero - EnGo 3 - Product Brief
https://www.dejero.com/wp-content/uploads/2025/08/Dejero-EnGo3-EN-Web.pdf
draft-ietf-moq-transport-16 - Media over QUIC Transport
https://datatracker.ietf.org/doc/draft-ietf-moq-transport/
draft-ietf-quic-multipath-19 - Managing multiple paths for a QUIC connection
https://datatracker.ietf.org/doc/draft-ietf-quic-multipath/
Sony™ Open Source Statistics Exporter for SRT Available Now
https://www.haivision.com/blog/broadcast-video/sony-statistics-exporter-for-srt-available/
1 7 8 9
2 5
3 4
6 27 28
10
11
12
13
14 15
16 19
17 18 20 21
22 23 24
25 26
29
30
31
5