# Strata

**Bonded video transport over RIST for GStreamer, written in Rust.**

Strata aggregates bandwidth across multiple unreliable network interfaces — cellular modems, WiFi, Ethernet, satellite — into a single resilient video stream. It is built as a pair of GStreamer elements (`rsristbondsink` / `rsristbondsrc`) backed by an intelligent scheduling core that handles load balancing, adaptive redundancy, congestion control, and seamless failover.

Designed for field deployment on constrained hardware (e.g. Orange Pi with USB cellular modems) where link conditions are unpredictable and every bit of available bandwidth matters.

---

## Documentation

Full documentation is available in the **[Wiki](https://github.com/RephlexZero/strata/wiki)**.

| Page | Description |
|---|---|
| [Architecture](https://github.com/RephlexZero/strata/wiki/Architecture) | System design, component overview, and key design decisions |
| [Getting Started](https://github.com/RephlexZero/strata/wiki/Getting-Started) | Prerequisites, building, and quick start guide |
| [Configuration Reference](https://github.com/RephlexZero/strata/wiki/Configuration-Reference) | Full TOML config for links, scheduler, receiver, and lifecycle |
| [GStreamer Elements](https://github.com/RephlexZero/strata/wiki/GStreamer-Elements) | `rsristbondsink` and `rsristbondsrc` property and pad reference |
| [Integration Node](https://github.com/RephlexZero/strata/wiki/Integration-Node) | CLI binary for sender/receiver without writing pipeline code |
| [Telemetry](https://github.com/RephlexZero/strata/wiki/Telemetry) | Stats message schema and JSON relay |
| [Testing](https://github.com/RephlexZero/strata/wiki/Testing) | Test suites, how to run, and privilege requirements |
| [Deployment](https://github.com/RephlexZero/strata/wiki/Deployment) | Privileges, performance budgets, and operational guidance |

---

## Quick Start

**Sender** — stream over two bonded links:

```bash
gst-launch-1.0 \
  videotestsrc is-live=true ! \
  video/x-raw,width=1920,height=1080,framerate=30/1 ! \
  x264enc tune=zerolatency bitrate=3000 ! \
  mpegtsmux ! \
  rsristbondsink name=sink \
    sink.link_0::uri="rist://192.168.1.100:5000" \
    sink.link_1::uri="rist://10.0.0.100:5000"
```

**Receiver** — listen and display:

```bash
gst-launch-1.0 \
  rsristbondsrc links="rist://@0.0.0.0:5000" latency=100 ! \
  tsdemux ! h264parse ! avdec_h264 ! autovideosink
```

See the [Getting Started](https://github.com/RephlexZero/strata/wiki/Getting-Started) guide for prerequisites, build instructions, and TOML config examples.

---

## Project Structure

```
crates/
  gst-rist-bonding/     GStreamer plugin + integration_node binary
  rist-bonding-core/     Core bonding logic (no GStreamer dependency)
  librist-sys/           FFI bindings to librist
  rist-network-sim/      Linux netns + tc-netem test infrastructure
vendor/
  librist/               librist source (git submodule)
docs/                    Operational documentation
```

---

## License

LGPL. See [vendor/librist/COPYING](vendor/librist/COPYING) for librist's license terms.
