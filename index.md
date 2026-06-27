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
| [wiki/Strata-Platform.md](wiki/Strata-Platform.md) | Control plane, dashboard, agent, portal — full fleet management architecture | platform, fleet |

## Operations

| Page | Summary | Tags |
|------|---------|------|
| [wiki/Deployment.md](wiki/Deployment.md) | Production setup, privileges, performance budgets, troubleshooting guide | deployment, ops |
| [wiki/Testing.md](wiki/Testing.md) | Test matrix, simulation framework, CI workflows, regression thresholds | testing, ci |

<!--
Add a row when you create a wiki page; remove it when you retire one.
The summary here should be enough for an agent to decide relevance WITHOUT opening the page.
-->
