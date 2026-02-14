# Control Protocol — Sender Agent ↔ Control Plane

> **Status:** Draft spec for future implementation.

---

## 1. Transport

All control communication uses **WebSocket over TLS** (WSS), initiated by the
sender agent as an outbound connection. This solves the carrier-grade NAT problem:
field devices behind cellular modems cannot accept inbound connections, but they
can always connect out.

```
Sender Agent ──WSS──► Control Plane (wss://platform.example.com/agent/ws)
```

The video data plane (RIST/UDP) is separate and does NOT flow through the WebSocket.

---

## 2. Authentication Flow

```
1. Sender agent boots, reads its enrollment token from /etc/strata/agent.conf
2. Agent connects to wss://platform.example.com/agent/ws
3. Agent sends `auth` message with enrollment token
4. Control plane validates token, returns a session JWT + sender_id
5. All subsequent messages carry the JWT in the envelope
6. If JWT expires, agent re-authenticates automatically
```

First-time enrollment:
```
1. Admin creates a new sender in the dashboard → gets a one-time enrollment token
2. Admin writes token to /etc/strata/agent.conf on the Orange Pi (or scans QR)
3. Agent connects and enrolls — control plane issues a persistent device key
4. Enrollment token is consumed and cannot be reused
```

---

## 3. Message Envelope

All messages are JSON over WebSocket text frames.

```json
{
  "id": "msg-uuid-1234",
  "type": "device.status",
  "ts": "2026-02-14T10:30:00Z",
  "payload": { ... }
}
```

| Field | Type | Description |
|---|---|---|
| `id` | string | Unique message ID (UUIDv7 — time-ordered) |
| `type` | string | Message type (dotted namespace) |
| `ts` | string | ISO 8601 timestamp |
| `payload` | object | Type-specific data |

Request-response pairs use the same `id`: the response echoes the request's `id`
with a `.response` suffix on the type.

---

## 4. Message Types

### 4.1 Agent → Control Plane

#### `auth.login`

Sent immediately after WebSocket connection.

```json
{
  "type": "auth.login",
  "payload": {
    "enrollment_token": "enr_abc123...",
    "agent_version": "0.5.0",
    "hostname": "orangepi-field-1",
    "arch": "aarch64"
  }
}
```

#### `device.status`

Periodic heartbeat (every 10s) or on-change. Reports hardware state.

```json
{
  "type": "device.status",
  "payload": {
    "network_interfaces": [
      {
        "name": "wwan0",
        "type": "cellular",
        "state": "connected",
        "ip": "10.45.0.2",
        "carrier": "T-Mobile",
        "signal_dbm": -72,
        "technology": "LTE"
      },
      {
        "name": "wwan1",
        "type": "cellular",
        "state": "connected",
        "ip": "10.46.0.3",
        "carrier": "Vodafone",
        "signal_dbm": -68,
        "technology": "5G-NSA"
      },
      {
        "name": "eth0",
        "type": "ethernet",
        "state": "disconnected"
      }
    ],
    "media_inputs": [
      {
        "device": "/dev/video0",
        "type": "v4l2",
        "label": "HDMI Capture - USB Video",
        "capabilities": ["1920x1080@30", "1280x720@60"],
        "status": "available"
      },
      {
        "device": "/dev/video2",
        "type": "v4l2",
        "label": "USB Webcam",
        "capabilities": ["1280x720@30", "640x480@30"],
        "status": "available"
      }
    ],
    "saved_media": [
      {
        "path": "/home/strata/media/test-pattern.mp4",
        "name": "test-pattern.mp4",
        "size_mb": 42,
        "duration_s": 120
      }
    ],
    "stream_state": "idle",
    "cpu_percent": 12.5,
    "mem_used_mb": 180,
    "uptime_s": 86400
  }
}
```

#### `stream.stats`

Sent every 1s while streaming. Relays the bonding telemetry.

```json
{
  "type": "stream.stats",
  "payload": {
    "stream_id": "str_xyz789",
    "uptime_s": 342,
    "encoder_bitrate_kbps": 4800,
    "links": [
      {
        "id": 0,
        "interface": "wwan0",
        "state": "Live",
        "rtt_ms": 45.2,
        "loss_rate": 0.001,
        "capacity_bps": 8500000,
        "sent_bytes": 194000000,
        "signal_dbm": -72
      },
      {
        "id": 1,
        "interface": "wwan1",
        "state": "Live",
        "rtt_ms": 38.7,
        "loss_rate": 0.0005,
        "capacity_bps": 12000000,
        "sent_bytes": 248000000,
        "signal_dbm": -68
      }
    ]
  }
}
```

#### `stream.ended`

Sent when a stream stops (user-initiated or error).

```json
{
  "type": "stream.ended",
  "payload": {
    "stream_id": "str_xyz789",
    "reason": "user_stop",
    "duration_s": 3600,
    "total_bytes": 2400000000
  }
}
```

### 4.2 Control Plane → Agent

#### `auth.login.response`

```json
{
  "type": "auth.login.response",
  "payload": {
    "success": true,
    "sender_id": "snd_abc123",
    "session_token": "eyJhbGciOiJFZDI1NTE5...",
    "config": { ... }
  }
}
```

#### `stream.start`

Instructs the agent to start a broadcast.

```json
{
  "type": "stream.start",
  "payload": {
    "stream_id": "str_xyz789",
    "source": {
      "mode": "v4l2",
      "device": "/dev/video0",
      "resolution": "1920x1080",
      "framerate": 30
    },
    "encoder": {
      "bitrate_kbps": 5000,
      "tune": "zerolatency",
      "keyint_max": 60
    },
    "destinations": [
      "rist://platform.example.com:15000",
      "rist://platform.example.com:15002"
    ],
    "bonding_config": {
      "version": 1,
      "links": [
        { "id": 0, "uri": "rist://platform.example.com:15000", "interface": "wwan0" },
        { "id": 1, "uri": "rist://platform.example.com:15002", "interface": "wwan1" }
      ],
      "scheduler": {
        "storm_enabled": true,
        "discard_deadline_ms": 200,
        "redundancy_enabled": true,
        "critical_broadcast": true
      }
    }
  }
}
```

The `bonding_config` is a complete TOML-equivalent JSON object that the agent
serialises to TOML and passes to the GStreamer pipeline.

#### `stream.stop`

```json
{
  "type": "stream.stop",
  "payload": {
    "stream_id": "str_xyz789",
    "reason": "user_request"
  }
}
```

#### `config.update`

Push a new bonding config to the agent without stopping the stream (hot-reload
for supported fields like scheduler weights, failover thresholds).

```json
{
  "type": "config.update",
  "payload": {
    "scheduler": {
      "discard_deadline_ms": 300,
      "redundancy_spare_ratio": 0.4
    }
  }
}
```

#### `agent.update`

Instruct the agent to self-update.

```json
{
  "type": "agent.update",
  "payload": {
    "binary_url": "https://releases.example.com/strata-agent-0.6.0-aarch64",
    "sha256": "abc123...",
    "restart_after": true
  }
}
```

---

## 5. REST API (Dashboard ↔ Control Plane)

The web dashboard communicates with the control plane over a standard REST API.

### Authentication

```
POST /api/auth/login
  Body: { "email": "...", "password": "..." }
  Response: { "token": "eyJ...", "user": { ... } }

POST /api/auth/refresh
  Headers: Authorization: Bearer <token>
```

### Senders

```
GET    /api/senders                    # List all senders for this user
GET    /api/senders/:id                # Get sender details + live status
POST   /api/senders                    # Create new sender → returns enrollment token
DELETE /api/senders/:id                # Decommission a sender

GET    /api/senders/:id/status         # Live hardware status (interfaces, inputs)
GET    /api/senders/:id/stats          # Live stream stats (if streaming)
```

### Streams

```
POST   /api/senders/:id/stream/start   # Start a broadcast
  Body: { source, encoder, platform_dest }

POST   /api/senders/:id/stream/stop    # Stop a broadcast

GET    /api/streams                     # List all active streams
GET    /api/streams/:id                 # Get stream details + stats
```

### Platform Destinations

```
GET    /api/destinations                # List configured streaming destinations
POST   /api/destinations               # Add a destination (YouTube, Twitch key, SRT endpoint)
PUT    /api/destinations/:id            # Update
DELETE /api/destinations/:id            # Remove
```

### WebSocket (Live Updates)

```
WS /api/ws
  # Subscribes to real-time events: sender status changes, stream stats, alerts
  # Client sends: { "subscribe": ["sender:snd_abc123", "stream:str_xyz789"] }
  # Server pushes: status updates, stats, alerts
```

---

## 6. Port Allocation

The control plane dynamically allocates UDP port pairs for each new stream:

```
Stream starts → control plane picks ports 15000, 15002 from pool
             → tells receiver worker to bind on those ports
             → tells sender agent to send to those ports
Stream ends   → ports returned to pool
```

Port range: configurable, e.g. 15000–16000 (500 concurrent streams).

---

## 7. Error Handling

| Scenario | Behaviour |
|---|---|
| Agent loses WebSocket | Auto-reconnect with exponential backoff (1s → 2s → 4s → 30s max) |
| Stream fails on sender | Agent sends `stream.ended` with `reason: "error"`, control plane cleans up receiver |
| Receiver worker crashes | Control plane detects (process exit), notifies dashboard, agent stops sending |
| Agent offline | Dashboard shows "offline" with last-seen timestamp |
| Control plane restart | Agents reconnect, re-auth, report current state; active receivers survive (they're separate processes) |
