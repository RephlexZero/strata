# Receiver Workers & Stream Forwarding

> **Status:** Draft spec for future implementation.

---

## 1. Overview

When a client starts a broadcast, the control plane spawns a **receiver worker**
process on the VPS. This process runs a GStreamer pipeline that receives the
bonded RIST stream, reassembles it, and forwards it to the configured streaming
platform.

```
Sender (field)                    VPS                           Platform
                                  ┌─────────────────────────┐
  wwan0 ──RIST UDP:15000──────►   │  receiver worker        │
  wwan1 ──RIST UDP:15002──────►   │                         │
                                  │  rsristbondsrc          │
                                  │    ↓                    │
                                  │  tsdemux → h264parse    │
                                  │    ↓                    │
                                  │  flvmux → rtmpsink  ───┼──► YouTube RTMP
                                  │                         │
                                  └─────────────────────────┘
```

---

## 2. Worker Lifecycle

### Spawn

```
1. Control plane receives `stream.start` request from dashboard
2. Control plane allocates port pair (e.g. 15000, 15002) from pool
3. Control plane generates RIST PSK for this stream
4. Control plane spawns: strata-receiver \
     --stream-id str_xyz789 \
     --ports 15000,15002 \
     --psk "random-key-here" \
     --forward-to "rtmp://a.rtmp.youtube.com/live2/xxxx-xxxx-xxxx" \
     --control-socket /tmp/strata-receiver-str_xyz789.sock
5. Waits for worker to report "ready" on control socket
6. Sends destinations + PSK to sender agent via WSS
7. Sender starts transmitting → worker receives and forwards
```

### Monitor

The control plane monitors each worker via:
- **Process health**: `waitpid` with WNOHANG — detect crashes
- **Unix socket**: Worker reports stats (bitrate in, bitrate out, buffer health)
- **Timeout**: If no data received for 30s, consider the stream stalled

### Teardown

```
1. Dashboard sends stream.stop → control plane
2. Control plane sends "shutdown" command to worker via unix socket
3. Worker sends EOS to pipeline, flushes, exits cleanly
4. Control plane reclaims ports, cleans up temp files
5. Notifies sender agent to stop transmitting
```

---

## 3. Forwarding Pipelines

### YouTube / Twitch (RTMP)

```
rsristbondsrc → tsdemux → h264parse → flvmux streamable=true → rtmpsink
```

No transcoding — passthrough of the H.264 stream. CPU cost: ~0.1 cores.

### SRT Relay

```
rsristbondsrc → srtsink uri=srt://...
```

Direct passthrough of the MPEG-TS. Even cheaper.

### HLS Output (for custom CDN)

```
rsristbondsrc → tsdemux → h264parse → hlssink2 target-duration=4
```

Generates segments on disk; a web server (nginx) serves them.

### Record to File

```
rsristbondsrc → tee → queue → filesink  (record)
                    → queue → flvmux → rtmpsink  (forward)
```

Concurrent recording and forwarding.

### Future: ABR Transcoding

```
rsristbondsrc → tsdemux → avdec_h264 → [multiple x264enc at different bitrates]
                                       → hlssink / DASH
```

This is CPU-intensive (~1–2 cores per stream) and should only be offered as an
opt-in premium feature. Consider offloading to dedicated transcode nodes.

---

## 4. Worker Implementation

The receiver worker is a standalone binary (`strata-receiver`) that:
1. Accepts CLI arguments for stream config
2. Builds and runs the GStreamer pipeline
3. Listens on a Unix domain socket for control commands
4. Reports stats back to the control plane

```rust
// Simplified structure
struct ReceiverWorker {
    stream_id: String,
    pipeline: gst::Pipeline,
    control_socket: UnixListener,
    stats_interval: Duration,
}

impl ReceiverWorker {
    fn run(&mut self) {
        // 1. Build pipeline from config
        // 2. Start pipeline
        // 3. Enter event loop:
        //    - Poll GStreamer bus for errors/EOS
        //    - Poll control socket for commands (stop, config update)
        //    - Emit stats on interval
    }
}
```

### Control Socket Protocol

Simple line-delimited JSON over Unix socket:

```json
// Control plane → Worker
{"cmd": "stop"}
{"cmd": "stats"}
{"cmd": "update_forward", "url": "rtmp://new-url/..."}

// Worker → Control plane
{"event": "ready", "ports": [15000, 15002]}
{"event": "stats", "bitrate_in_kbps": 5200, "bitrate_out_kbps": 5100, "buffer_ms": 120}
{"event": "error", "message": "RTMP connection refused"}
{"event": "stopped", "reason": "eos"}
```

---

## 5. Resource Management

### Port Pool

```rust
struct PortPool {
    range: RangeInclusive<u16>,  // e.g. 15000..=16000
    allocated: HashSet<u16>,
}

impl PortPool {
    fn allocate(&mut self, count: usize) -> Option<Vec<u16>> {
        // Find `count` consecutive free ports
        // Mark as allocated
        // Return port list
    }

    fn release(&mut self, ports: &[u16]) {
        // Return ports to pool
    }
}
```

Each stream needs N ports (one per sender link). Typical: 2–3 ports per stream.
With a 1000-port range, that's ~333–500 concurrent streams.

### Process Table

The control plane maintains a process table mapping stream IDs to worker PIDs:

```rust
struct ProcessTable {
    workers: HashMap<String, WorkerHandle>,
}

struct WorkerHandle {
    stream_id: String,
    pid: u32,
    ports: Vec<u16>,
    started_at: Instant,
    control_socket: PathBuf,
    user_id: String,
}
```

### Resource Limits

Per-worker limits enforced via cgroups (or systemd resource control):

| Resource | Limit | Rationale |
|---|---|---|
| CPU | 0.5 cores (passthrough) / 2 cores (transcode) | Prevent one stream from starving others |
| Memory | 256 MB | Pipeline + buffers |
| Network (egress) | 20 Mbps | Prevent a misconfigured stream from saturating uplink |

---

## 6. Health Checks

The control plane runs a health check loop every 5 seconds:

```
For each active worker:
  1. Is the process still alive? (kill -0 pid)
  2. Has it reported stats in the last 15 seconds?
  3. Is its input bitrate > 0? (sender actually transmitting?)
  4. Is its output connection healthy? (RTMP connected?)

If a worker is unhealthy:
  - Alert the dashboard
  - After 60s of no input: auto-kill and reclaim resources
  - Report to sender agent so it can stop transmitting
```

---

## 7. Multi-Host Scaling (Future)

When a single VPS is not enough:

```
                    ┌──────────────────┐
                    │   Load Balancer   │  (control plane only — HTTPS)
                    └────────┬─────────┘
                             │
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
        ┌──────────┐  ┌──────────┐  ┌──────────┐
        │  VPS-1   │  │  VPS-2   │  │  VPS-3   │
        │ control  │  │ workers  │  │ workers  │
        │ + workers│  │ only     │  │ only     │
        └──────────┘  └──────────┘  └──────────┘
```

- Control plane runs on VPS-1 (or dedicated)
- Workers can run on any VPS — control plane assigns streams to the least-loaded host
- RIST UDP traffic goes directly to the worker host (DNS or IP returned to sender)
- Shared state (user DB, stream registry) in PostgreSQL on VPS-1 or managed DB

This is a **Phase 6** concern. Don't build it until a single host is saturated.
