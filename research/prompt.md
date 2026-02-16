

 Let me strip the files back to empty stubs first, then craft the research prompts. 

Replacing 99 lines with 21 lines in [](file:///workspaces/rist-bonding/crates/strata-transport/src/lib.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/rtp.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/rtcp.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/socket.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/sender.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/receiver.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/arq.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/stats.rs)


```

```

```

```

```

```

```

```

```

```

```

```

```

```

```

```
Now let me also strip out all the code that was below the first doc comment in each file:   

Read [](file:///workspaces/rist-bonding/crates/strata-transport/src/rtp.rs)

Read [](file:///workspaces/rist-bonding/crates/strata-transport/src/rtcp.rs)

Read [](file:///workspaces/rist-bonding/crates/strata-transport/src/arq.rs)

These files still have the full implementations. Let me wipe them to just the doc comment: 

Replacing 1183 lines with 14 lines in [](file:///workspaces/rist-bonding/crates/strata-transport/src/rtp.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/rtcp.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/arq.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/sender.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/receiver.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/socket.rs), [](file:///workspaces/rist-bonding/crates/strata-transport/src/stats.rs)


```

```

```

```

```

```

```

```

```

```

```

```

```

```
Good, files are now clean stubs. Now let me also quickly update the workspace Cargo.toml and the renamed crate Cargo.toml files so the workspace is consistent, then I'll commit and get to the research prompts. 

Replacing 16 lines with 17 lines in [](file:///workspaces/rist-bonding/Cargo.toml), [](file:///workspaces/rist-bonding/crates/strata-bonding/Cargo.toml), [](file:///workspaces/rist-bonding/crates/strata-gst/Cargo.toml), [](file:///workspaces/rist-bonding/crates/strata-sim/Cargo.toml)


```

```

```

```

```

```

```

```
Now I need to update the Rust source files that reference the old crate names in `use` statements and `extern crate`: 

Searched for regex `rist_bonding_core|rist_network_sim|gst_rist_bonding` (`**/*.rs`), 18 results

Ran terminal command:  find crates/strata-gst -name '*.rs' -exec sed -i 's/rist_bonding_core/strata_bonding/g; s/rist_network_sim/strata_sim/g' {} + && sed -i 's/rist_bonding_core/strata_bonding/g' crates/strata-bonding/src/lib.rs

Now let me commit and move on to the research prompts:  

Ran terminal command:  cd /workspaces/rist-bonding && git add -A && git commit -m "refactor: restructure workspace for strata-transport

Renames:
  - rist-bonding-core → strata-bonding
  - gst-rist-bonding → strata-gst  
  - rist-network-sim → strata-sim

New crate:
  - strata-transport (empty stubs, pending design)

All Rust imports and Cargo.toml paths updated.
librist-sys retained as reference during transition."

Clean build, clean commit. Now for the research prompts.

---

Here are **5 deep research prompts** — each targeting a different domain. Give them to separate AI sessions for maximum depth.

---

## Prompt 1: Beyond-RIST Transport Protocol Design

> **I'm building a next-generation reliable media transport protocol in pure Rust, intended to replace RIST (VSF TR-06-2) for live video over bonded cellular links. I want it to be strictly better than RIST, not just a reimplementation. Research the following and provide concrete protocol design recommendations with tradeoffs:**
>
> 1. **Hybrid ARQ + FEC**: RIST uses pure NACK-based ARQ. Research the state of the art in combining selective ARQ with adaptive forward error correction (e.g., RaptorQ / RFC 6330, Reed-Solomon, fountain codes). Specifically: how should FEC rate adapt in real-time based on measured loss and RTT? What are the latency tradeoffs? Look at what SRT (Secure Reliable Transport), Zixi, QUIC, and WebTransport do. Find any 2023-2026 papers on "hybrid ARQ-FEC for real-time media."
>
> 2. **Congestion control for bonded cellular**: Research BBR v2, COPA, Sprout, Verus, ABC, and any cellular-specific congestion control algorithms. The links are 4G/5G modems with highly variable capacity, deep queues, and sudden handoffs. What works best for maintaining low latency while maximizing throughput on cellular? Are there approaches that use RTT gradient or one-way delay variation rather than loss-based signals?
>
> 3. **Media-aware packet prioritization**: H.264/H.265 have I-frames, P-frames, B-frames with vastly different importance. Research how to make the transport layer aware of NAL unit types so it can: (a) prioritize retransmission of keyframes, (b) selectively drop B-frames under congestion, (c) signal the encoder to adjust quality. Look at what QUIC DATAGRAM, RoQ (RTP over QUIC), and MoQ (Media over QUIC) are doing. Any 2024-2026 IETF drafts on media-aware transport.
>
> 4. **Sequence numbering beyond 16-bit**: RIST inherits RTP's 16-bit sequence space which wraps at 65536. At high bitrates this wraps in seconds. Research extended sequence number schemes, how QUIC handles it (62-bit variable-length), and whether we should design our own framing or use QUIC frames as a substrate.
>
> 5. **Zero-copy and io_uring**: Research the state of the art for high-performance UDP in Rust — io_uring with `recvmsg_multishot`, `sendmsg_zerocopy`, GRO/GSO for batched sends, `AF_XDP` for kernel bypass. What Rust crates exist (tokio-uring, glommio, monoio)? What's production-ready in 2026?
>
> For each topic, provide: the best approach for our use case, the key paper/RFC/drafts to read, what existing open-source code to study, and a concrete recommendation.

---

## Prompt 2: Cutting-Edge Multi-Link Bonding & Scheduling

> **I'm building a packet scheduler that bonds 2-6 cellular links (4G/5G USB modems) into a single aggregated stream for live video. I already have a DWRR scheduler with quality-adjusted capacity and trend prediction. I want to go further. Research the following:**
>
> 1. **MPTCP Scheduler Research (2022-2026)**: What scheduler algorithms have been proposed for MPTCP that outperform minRTT and round-robin? Look at BLEST, ECF, DEMS, ReMP, and any ML-based schedulers. Which ideas transfer to UDP-level bonding? Focus on heterogeneous paths (one fast WiFi, two variable cellular).
>
> 2. **Reinforcement learning for link selection**: Research whether RL (PPO, SAC, or simpler bandit algorithms like Thompson Sampling or UCB) can outperform hand-tuned heuristics for per-packet link selection. What state features would we feed? (RTT, loss rate, queue depth, throughput trend, time-of-day, signal strength). Has anyone applied RL to multi-path scheduling for real-time media? Find papers.
>
> 3. **Predictive link quality**: Cellular links have patterns — handoffs cause 50-500ms blackouts, towers have capacity cycles, signal fading is somewhat predictable. Research techniques for short-term (<1s) and medium-term (1-30s) link quality prediction. Can we use the modem's signal metrics (RSRP, RSRQ, SINR, CQI) as leading indicators? Any papers on "cellular link quality prediction for real-time applications"?
>
> 4. **Coded bonding / network coding**: Instead of sending packet A on link 1 and packet B on link 2, could we send coded combinations? Research Random Linear Network Coding (RLNC) for multi-path, specifically Steinwurf's Kodo library concepts, and how this interacts with ARQ. Is the CPU cost acceptable on embedded ARM devices?
>
> 5. **Graceful degradation strategies**: When total bonded capacity drops below the video bitrate, what's the optimal strategy? Research "quality ladder" approaches (multiple encoder bitrates ready to switch), SVC (Scalable Video Coding/AV1-SVC), and ROI-based quality (keep faces sharp, blur background). What can be done at the transport layer vs requiring encoder cooperation?
>
> Provide concrete algorithm pseudocode where possible, not just theory.

---

## Prompt 3: Docker-Based Cellular Network Simulation

> **I need to build a comprehensive network simulation environment that runs entirely inside Docker containers, for testing a UDP-based bonded media transport protocol. The simulation must realistically model 4G/5G cellular links. Research the following:**
>
> 1. **tc netem vs ns-3 vs other simulators**: Compare Linux `tc netem` (which we currently use), ns-3, CORE (Common Open Research Emulator), Mininet, and Kathará for cellular network simulation fidelity. Which can model: variable bandwidth (not just fixed rate limiting), correlated loss (Gilbert-Elliott model), reordering, jitter with heavy-tailed distributions, and handoff events? What runs well inside Docker with no X11?
>
> 2. **Realistic cellular link models**: Research what real 4G/5G links look like in terms of: bandwidth variation patterns over time (walking, driving, stationary), loss burst characteristics, RTT distributions, jitter patterns, and handoff behavior. Find datasets or published models (3GPP channel models, NYU Wireless traces, MONROE platform data). How do we encode these as reproducible scenarios?
>
> 3. **Docker networking for multi-link simulation**: We need 2-6 independent "links" between a sender container and receiver container, each with independent impairment. Research the best Docker networking approach: macvlan, ipvlan, bridge networks, or raw veth pairs with network namespaces. How do we ensure traffic for each link stays isolated? Can we use `tc` inside Docker (CAP_NET_ADMIN)? What about nested Docker-in-Docker?
>
> 4. **Dynamic impairment control**: We want to change link conditions during a test run (simulate driving into a tunnel, handoff, congestion event). Research approaches for scripted time-varying impairment: `tc` live modification, eBPF-based packet manipulation, Pumba, Toxiproxy, or custom solutions. What gives us millisecond-granularity control?
>
> 5. **Measurement and validation**: How do we measure whether our simulation is realistic? Research tools for capturing per-link statistics: packet timing, one-way delay measurement (requires clock sync — PTP/chrony in containers?), throughput curves, loss burst lengths. What visualization tools work for comparing simulated vs real cellular traces?
>
> 6. **Published trace replay**: Can we replay real-world cellular bandwidth traces (like Mahimahi traces, FCC MBA data, Pantheon traces)? Research trace-driven simulation: tools, formats, and how to apply real bandwidth traces as `tc` rate limiting in real-time.
>
> Provide a concrete Docker Compose architecture and example commands for each approach.

---

## Prompt 4: Rust Systems Programming — High-Performance Networking

> **I'm building a high-performance UDP transport protocol in Rust that will handle 3-6 simultaneous links, each running at 2-10 Mbps, with sub-millisecond timing requirements. Research the best Rust patterns and crates for this:**
>
> 1. **Async runtime choice for networking**: Compare tokio, async-std, smol, glommio, and monoio for our use case. We need: UDP socket polling, timers (100ms RTCP interval, RTT-based NACK scheduling), and parallel processing of multiple links. Is async even the right model, or should we use dedicated threads with crossbeam channels (which we currently do)?
>
> 2. **Zero-copy buffer management**: Research `bytes::Bytes` vs custom slab allocators vs arena allocation for packet buffers. We're doing ~3000 packets/sec across all links. Each packet needs to live in a retransmission buffer for up to 1 second. What's the most cache-friendly, allocation-efficient approach? Look at what ngtcp2, quinn, and s2n-quic do.
>
> 3. **Lock-free data structures for packet scheduling**: Our DWRR scheduler is called on every packet (~3000/sec). Research lock-free queues (crossbeam, rtrb), concurrent hash maps (dashmap, flurry), and whether we should use a single-threaded design with message passing instead. What about using `io_uring` submission queues as the scheduling primitive?
>
> 4. **Timing precision**: We need microsecond-precision timestamps for RTT calculation and jitter measurement. Research `Instant::now()` precision on Linux, whether `clock_gettime(CLOCK_MONOTONIC_RAW)` is better, and `TSC`-based timing. Does Rust's `std::time::Instant` have enough resolution? What about `quanta` crate?
>
> 5. **RaptorQ / fountain codes in Rust**: Survey the Rust ecosystem for FEC: `raptorq` crate, `reed-solomon-erasure`, `leopard-rs` (FFT-based RS). Benchmark data on encoding/decoding throughput at our packet rates. Is CPU overhead acceptable on ARM (Raspberry Pi 5, Jetson)?
>
> 6. **Testing patterns for network protocols**: Research property-based testing (proptest, quickcheck), simulation testing (like FoundationDB's deterministic simulation), and fuzz testing (cargo-fuzz, AFL) applied to network protocols in Rust. How do we test sequence wrap, reordering, and loss recovery exhaustively?
>
> Provide specific crate names, version numbers, and benchmark data where available.

---

## Prompt 5: Competitive Analysis — SRT, Zixi, LiveU, TVU, Haivision

> **I'm building an open-source alternative to commercial bonded cellular video transport systems. Research these competitors and the broader landscape to identify what we should learn from and what we can do better:**
>
> 1. **SRT (Secure Reliable Transport)**: Deep dive into SRT's architecture — its "too late to send" mechanism, caller/listener/rendezvous modes, encryption (AES-CTR), and how its ARQ differs from RIST's. What are SRT's known weaknesses? Why did the industry also create RIST if SRT existed? What does SRT's bonding story look like (SRT Alliance, Haivision's Hub)?
>
> 2. **Zixi protocol**: What is known about Zixi's proprietary protocol? Patents, white papers, published benchmarks. How does their "hitless failover" work? What makes them claim sub-frame latency? Do they use FEC+ARQ hybrid?
>
> 3. **LiveU / TVU / Dejero**: These are the commercial bonded cellular solutions. Research their published architectures: how many modems they support, whether bonding is done at IP layer or transport layer, how they handle encoder rate adaptation, what their failover time is. Any published patents or conference papers from these companies?
>
> 4. **QUIC-based media transport**: Research RTP-over-QUIC (RoQ, draft-ietf-avtcore-rtp-over-quic), Media over QUIC (MoQ, draft-ietf-moq-transport), and WebTransport. Could we build on QUIC instead of raw UDP? What are the latency implications? Does QUIC's built-in congestion control help or hurt for bonded cellular? Any multi-path QUIC (MP-QUIC) developments?
>
> 5. **What would make an open-source solution win?**: Research what features are missing from RIST and SRT that commercial solutions provide. Look at user forums, GitHub issues, and industry complaints. What gaps exist that we could fill? Consider: bonding, mobile encoding, cloud receiver autoscaling, monitoring/alerting, cellular modem management (AT commands, signal monitoring).
>
> I want a clear picture of the competitive landscape and specific technical innovations we should adopt or improve upon. Focus on 2024-2026 developments.

Made changes.