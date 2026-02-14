# Security Model

> **Status:** Draft spec for future implementation.

---

## 1. Threat Model

The system has three trust boundaries:

```
┌─────────────────────┐          ┌──────────────────┐          ┌──────────────────┐
│   Field Sender      │──────────│   Public Internet │──────────│   Cloud VPS      │
│   (untrusted net)   │  RIST    │   (hostile)       │  RTMP    │   (trusted)      │
│                     │  UDP     │                   │  TCP     │                  │
└─────────────────────┘          └──────────────────┘          └──────────────────┘
        ▲                                                              ▲
        │ Physical access                                              │ SSH / API
        │ possible by                                                  │ access by
        │ field operator                                               │ admin
```

| Threat | Risk | Mitigation |
|---|---|---|
| Video interception in transit | High — cellular traffic traverses carrier infrastructure | RIST encryption (PSK or DTLS) |
| Control channel interception | High — credentials, stream keys | TLS (WSS) for all control traffic |
| Unauthorized stream start | Medium — abuse of sender hardware | JWT auth + device enrollment tokens |
| Sender impersonation | Medium — rogue device pretending to be enrolled | Device key pairs (Ed25519) |
| VPS compromise | High — access to all stream keys, credentials | Least-privilege, secrets management, no plaintext keys on disk |
| Denial of service on RIST ports | Medium — UDP amplification | Rate limiting, port randomisation, firewall rules |
| Stolen sender device | Medium — contains device key, local media | Encrypted storage, remote wipe capability, key revocation |
| Dashboard credential stuffing | Medium | Rate limiting, bcrypt/argon2 passwords, optional 2FA |

---

## 2. Video Encryption

### Option A: RIST Pre-Shared Key (PSK) — Recommended for v1

librist supports AES-128/256 encryption with a pre-shared key. The sender and
receiver share a passphrase; librist derives the AES key internally.

```toml
# In the link URI
uri = "rist://platform.example.com:15000?secret=my-strong-passphrase&aes-type=256"
```

**Pros:**
- Zero additional implementation — librist handles it natively
- Negligible performance impact (AES-NI on most CPUs)
- Simple key management — control plane generates per-stream PSK

**Cons:**
- Key distribution requires a secure channel (we have one — the WSS control channel)
- No perfect forward secrecy (static key for stream lifetime)

**Workflow:**
1. Control plane generates a random 32-byte PSK for each stream
2. PSK is sent to sender agent via encrypted WSS in `stream.start` message
3. PSK is passed to receiver worker at spawn time
4. Both sides embed PSK in RIST URI `?secret=...&aes-type=256`
5. PSK is never stored on disk — it lives only in memory for the stream duration

### Option B: DTLS (Future)

librist also supports DTLS 1.2 with certificate-based auth. This provides
perfect forward secrecy and mutual authentication, but requires a PKI
(certificate authority, per-device certs).

**When to adopt:** When the platform has >50 senders and key management becomes
a burden, or when customers require PFS compliance.

---

## 3. Control Channel Security

| Layer | Mechanism |
|---|---|
| Transport | TLS 1.3 (via WSS and HTTPS) |
| Authentication | JWT (Ed25519-signed, short-lived: 1 hour) |
| Device identity | Ed25519 key pair generated at enrollment, stored in `/etc/strata/device.key` |
| API auth | Bearer token (JWT) in Authorization header |
| Dashboard auth | Email + password (Argon2id hash) + optional TOTP 2FA |

### JWT Claims

```json
{
  "sub": "snd_abc123",
  "iss": "strata-control",
  "exp": 1739530200,
  "iat": 1739526600,
  "role": "sender",
  "owner": "usr_xyz789"
}
```

### Token Refresh

- Sender agents: auto-refresh via WSS before expiry
- Dashboard: refresh via `/api/auth/refresh` endpoint
- Refresh tokens: 30-day lifetime, rotate on use, revocable

---

## 4. Network Security

### Firewall Rules (VPS)

```bash
# Control plane (HTTPS + WSS)
ufw allow 443/tcp

# RIST receiver ports (dynamic range)
ufw allow 15000:16000/udp

# RTMP out to streaming platforms — no inbound rule needed (outbound TCP)
# SSH — restrict to admin IPs
ufw allow from <admin-ip> to any port 22

# Deny everything else
ufw default deny incoming
```

### DDoS Mitigation

- RIST ports only accept traffic from registered sender IPs (control plane updates iptables rules when senders connect)
- Rate-limit unauthenticated WebSocket connections
- Use `fail2ban` for SSH and API endpoint brute-force protection
- Consider Cloudflare or equivalent for the HTTPS control plane (not for UDP)

---

## 5. Secrets Management

| Secret | Where Stored | Rotation |
|---|---|---|
| Stream PSKs | In-memory only (control plane + workers) | Per-stream (new key each broadcast) |
| JWT signing key | Environment variable or secrets file (0600 perms) | Quarterly or on compromise |
| Database credentials | Environment variable | On deploy |
| RTMP stream keys | Database (encrypted at rest with app-level key) | User-managed |
| Device enrollment tokens | Database (hashed, single-use) | Consumed on first use |
| Device Ed25519 private key | `/etc/strata/device.key` on sender (0600 perms) | On re-enrollment |

### Key Hierarchy

```
Platform Root Key (offline, HSM or secure vault)
  └── JWT Signing Key (Ed25519, in control plane memory)
  └── Database Encryption Key (AES-256, in control plane memory)
       └── Encrypted RTMP stream keys (in database)
  └── Per-Stream RIST PSK (ephemeral, in memory only)
```

---

## 6. Sender Device Security

| Concern | Mitigation |
|---|---|
| Stolen device | Remote key revocation via control plane; device key is deleted → agent can't auth |
| Local root access | Device key protected by filesystem permissions; full-disk encryption recommended |
| Network sniffing on sender | All control traffic over TLS; all video over RIST encryption |
| Unauthorized config changes | Agent only accepts config from authenticated control plane WSS |
| Firmware tampering | Signed agent binaries; agent verifies SHA-256 before self-update |

---

## 7. Compliance Considerations

| Requirement | Approach |
|---|---|
| Data in transit encryption | RIST PSK (AES-256) for video; TLS 1.3 for control |
| Data at rest encryption | LUKS full-disk on sender; database-level encryption on VPS |
| Access control | RBAC: admin, operator, viewer roles |
| Audit logging | All API calls logged with user, action, timestamp |
| Data retention | Stream recordings: configurable retention policy per-user |
| GDPR | User data deletion endpoint; no PII in telemetry |
