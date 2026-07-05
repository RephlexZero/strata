# Index

Map of this workspace. **One row per `wiki/` page**: path, summary, tags.
Scan before opening any full page. Keep summaries to one line.

## Documentation (user-facing)

| Page | Summary | Tags |
|------|---------|------|
| [wiki/Home.md](wiki/Home.md) | Overview, at-a-glance table, architecture diagram, doc index | overview, meta |
| [wiki/Getting-Started.md](wiki/Getting-Started.md) | Install, build, quick start, dev container, cross-compilation, releasing | setup, build |
| [wiki/Configuration-Reference.md](wiki/Configuration-Reference.md) | Complete TOML config — links, scheduler, congestion, FEC, lifecycle, receiver | config, reference |
| [wiki/GStreamer-Elements.md](wiki/GStreamer-Elements.md) | stratasink / stratasrc properties, pad templates, pipeline examples | gstreamer, api |
| [wiki/Strata-Node.md](wiki/Strata-Node.md) | strata-pipeline CLI — sender/receiver modes, source hot-swap, RTMP/HLS relay | cli, usage |
| [wiki/Cellular-Modem-Setup.md](wiki/Cellular-Modem-Setup.md) | USB modem config, ModemManager, policy routing, band management | hardware, setup |
| [wiki/Telemetry.md](wiki/Telemetry.md) | Stats schema, JSON relay, Prometheus metrics endpoint | telemetry, ops |
| [wiki/Integration-Node.md](wiki/Integration-Node.md) | Integration node setup and wiring | integration |

## Architecture & design

| Page | Summary | Tags |
|------|---------|------|
| [wiki/Architecture.md](wiki/Architecture.md) | Transport protocol, bonding engine, scheduling algorithms, FEC/ARQ, congestion control | architecture, internals |
| [wiki/Adaptation-Delay-Pressure.md](wiki/Adaptation-Delay-Pressure.md) | Why the bitrate adapter measures bufferbloat via AQM/receiver delay, never raw paced-queue packet count | adaptation, congestion, invariant |
| [wiki/Adaptation-FEC-Sizing.md](wiki/Adaptation-FEC-Sizing.md) | Why FEC parity is sized to per-link channel loss, never the post-FEC residual (the microburst death spiral) | adaptation, fec, invariant |
| [wiki/Adaptation-Encoder-Cut-Signals.md](wiki/Adaptation-Encoder-Cut-Signals.md) | What may cut the encoder bitrate (capacity pressure, goodput shortfall, AQM, per-link melt) — and why the post-FEC residual may not | adaptation, congestion, invariant |
| [wiki/Strata-Platform.md](wiki/Strata-Platform.md) | Control plane, dashboard, agent — full fleet management architecture (strata-portal retired 2026-07-01) | platform, fleet |
| [wiki/Adaptation-EWMA-Conventions.md](wiki/Adaptation-EWMA-Conventions.md) | The dozen-plus independently-tuned EWMAs in the bonding/transport stack, and the rise-fast/fall-slow vs rise-slow/fall-fast polarity rule that governs them | adaptation, congestion, ewma, invariant |
| [wiki/Control-Loop-Map.md](wiki/Control-Loop-Map.md) | The consolidated map of Strata's control loops, the broadcast-profile principle, and the historical whipsaw incidents behind it | architecture, adaptation, invariant |
| [wiki/Observability-Semantics.md](wiki/Observability-Semantics.md) | What each loss/latency metric actually measures — and the three ways the prominent ones lied during field debugging | telemetry, gotcha, invariant |
| [wiki/MPEG-TS-Mux-Overhead.md](wiki/MPEG-TS-Mux-Overhead.md) | mpegtsmux pat/pmt-interval are 90 kHz ticks, not packet counts — =1 tripled wire bandwidth and drove the AQM self-loss saga; use 9000 (100 ms) | gstreamer, mux, bandwidth, gotcha |
| [wiki/HLS-Egress-Discontinuity-Tagging.md](wiki/HLS-Egress-Discontinuity-Tagging.md) | hlssink3 needs video/audio request pads (not a pre-muxed TS) and never auto-tags DISCONT — discontinuity tagging is reconstructed in hls_upload.rs from hls-segment-added messages | gstreamer, hls, receiver, gotcha |

## Operations

| Page | Summary | Tags |
|------|---------|------|
| [wiki/Deployment.md](wiki/Deployment.md) | Manual (non-platform) production setup, privileges, performance budgets, troubleshooting | deployment, ops |
| [wiki/Platform-Operations.md](wiki/Platform-Operations.md) | Operator manual for the platform path — control plane setup, device enrollment, receivers, stream health, troubleshooting | platform, ops |
| [wiki/Daemon-Configuration.md](wiki/Daemon-Configuration.md) | Which config knob lives where per daemon (flags/env/TOML) and the /etc/strata/<role>.env pattern | config, reference, platform |
| [wiki/Testing.md](wiki/Testing.md) | Test matrix, simulation framework, CI workflows, regression thresholds | testing, ci |

<!--
Add a row when you create a wiki page; remove it when you retire one.
The summary here should be enough for an agent to decide relevance WITHOUT opening the page.
-->
