# Hardware Evaluation — Sender Device

> **Status:** Draft. Captures hardware research for the field sender appliance.

---

## 1. Requirements

The sender device needs to:

| Requirement | Priority | Notes |
|---|---|---|
| HDMI video input | Must-have | Capture a camera or other HDMI source |
| 2–3 USB ports for cellular modems | Must-have | USB 3.0 preferred (some 5G modems need it) |
| Wi-Fi with AP mode | Must-have | Local setup portal on first boot |
| H.264 encoding at 1080p30 | Must-have | Software (x264) or hardware (VPU) |
| Reliable 24/7 operation | Must-have | No thermal throttling, no random lockups |
| Compact form factor | Should-have | Field-portable |
| Good mainline Linux support | Should-have | Kernel drivers, community, Armbian/Ubuntu |
| Hardware video encoder (VPU) | Nice-to-have | Reduces CPU load; x264 software encode works fine as fallback |
| M.2 slot for NVMe or Wi-Fi | Nice-to-have | Local recording, dedicated Wi-Fi module |
| PoE or wide-voltage input | Nice-to-have | Field power flexibility |

---

## 2. HDMI Input: Native vs USB Capture

There are two ways to get HDMI into an SBC:

### Native HDMI RX (RK3588 only)

The Rockchip RK3588 SoC has a built-in HDMI receiver block. Boards that break it
out to a physical HDMI input connector can capture HDMI directly without any
additional hardware.

**Pros:**
- Zero additional cost, zero additional failure point
- Low latency (memory-mapped, no USB overhead)
- Full 4K60 capture capability (though we only need 1080p30)

**Cons:**
- Only available on RK3588 boards that wire up the HDMI RX
- Kernel driver support varies — some boards have better BSP support than others
- V4L2 driver for RK3588 HDMI RX exists in Rockchip BSP kernel but is not yet
  in mainline Linux (as of early 2026)

### USB HDMI Capture Card

A USB dongle (Macrosilicon MS2109, Lenkeng, or similar) that presents as a
standard V4L2 UVC device.

**Pros:**
- Works on ANY board with USB — no SoC-specific driver needed
- Mainstream UVC driver in mainline kernel — rock-solid V4L2 support
- Cheap ($10–30 for 1080p30 capture)
- Field-replaceable (if the dongle dies, swap it)
- Multiple options: MJPEG output (decode on SBC) or YUV (direct, higher USB bandwidth)

**Cons:**
- USB bandwidth: 1080p YUV consumes ~3 Gbps — needs USB 3.0 and competes with cellular modems
- MJPEG mode: more practical bandwidth (~200 Mbps) but adds a decode step
- One more physical thing to connect and potentially lose in the field

### Recommendation

**Use a USB HDMI capture card for v1.** It works on every board, uses a battle-tested
mainline kernel driver (UVC), and is field-replaceable. The latency overhead
(~5–10 ms) is negligible for our use case. If we later standardise on a board
with native HDMI RX and the kernel driver is stable, we can switch — the GStreamer
pipeline is identical either way (`v4l2src device=/dev/video0`).

---

## 3. Board Comparison

All RK3588-based boards share the same SoC capabilities. The differences are in
what peripherals are wired up, board quality, thermal design, and Linux support.

### Radxa ROCK 5B / 5B+

| Spec | ROCK 5B | ROCK 5B+ |
|---|---|---|
| SoC | RK3588 | RK3588 |
| RAM | 4/8/16 GB | 8/16/32 GB |
| HDMI input | **No** | **No** |
| HDMI output | 1x HDMI + 1x micro HDMI (8K+4K) | 1x HDMI + 1x micro HDMI |
| USB 3.0 | 2x USB 3.0 Type-A, 1x USB-C | 2x USB 3.0 Type-A, 1x USB-C |
| USB 2.0 | 2x | 2x |
| Wi-Fi | External M.2 E-key (5B), **onboard AX** (5B+) | Onboard Wi-Fi 6 (AX) + BT 5.0 |
| M.2 | E-key + M-key NVMe | E-key + M-key NVMe |
| Ethernet | 1x 2.5 GbE | 2x 2.5 GbE |
| Power | USB-C PD, 5V/3A barrel | USB-C PD, PoE (5B+ w/ HAT) |
| Form factor | Standard SBC (85x56mm) | Standard SBC |
| Linux support | Excellent — Armbian, Ubuntu, Debian | Excellent |
| Price | ~$80 (8GB), ~$130 (16GB) | ~$90 (8GB), ~$150 (16GB) |

**Verdict:** The **ROCK 5B+** is the strongest overall choice. Onboard Wi-Fi 6
(critical for AP mode — no extra module needed), 2x 2.5GbE, 4x USB ports for
modems, excellent Armbian support, 32GB RAM option if needed. No native HDMI
input, but USB capture solves that cleanly.

### Orange Pi 5 Plus

| Spec | Value |
|---|---|
| SoC | RK3588 |
| RAM | 8/16/32 GB |
| HDMI input | **Yes** (native RK3588 HDMI RX) |
| HDMI output | 2x HDMI (8K+4K) |
| USB 3.0 | 2x Type-A |
| USB 2.0 | 1x Type-A |
| Wi-Fi | External M.2 E-key (not included on all SKUs) |
| M.2 | E-key + M-key NVMe |
| Ethernet | 2x Gigabit |
| Power | USB-C, 5V/4A |
| Form factor | Standard SBC |
| Linux support | Good — Orange Pi OS, Armbian (community), Ubuntu |
| Price | ~$90 (8GB), ~$150 (16GB) |

**Verdict:** Main advantage is native HDMI input. Main disadvantages: no onboard
Wi-Fi (need M.2 module, which competes with the E-key slot), GbE not 2.5GbE,
Armbian support is community-maintained (not official). The HDMI input is
appealing but the BSP kernel driver is less proven than a UVC USB capture card.

### Radxa ROCK 5 ITX

| Spec | Value |
|---|---|
| SoC | RK3588 |
| RAM | 8/16/24 GB |
| HDMI input | **Yes** (native RK3588 HDMI RX) |
| HDMI output | 1x HDMI out |
| USB 3.0 | 4x Type-A (!) |
| USB 2.0 | 2x (via headers) |
| Wi-Fi | Wi-Fi 6 (AX) + BT 5.0 onboard |
| M.2 | M-key NVMe, E-key (Wi-Fi occupied) |
| Ethernet | 2x 2.5 GbE |
| Power | ATX 24-pin or DC barrel (12V) |
| Form factor | Mini-ITX (170x170mm) — larger than an SBC |
| Linux support | Good — Radxa Debian, Armbian |
| Price | ~$120 (8GB) |

**Verdict:** Has everything — HDMI input, 4x USB 3.0, onboard Wi-Fi, 2x 2.5GbE.
But it's a Mini-ITX form factor meant for a case, which makes it less portable.
Good for a **fixed installation** (studio, vehicle mount with enclosure), overkill
for a field-portable unit.

### Raspberry Pi 5

| Spec | Value |
|---|---|
| SoC | BCM2712 (4x Cortex-A76, 2.4 GHz) |
| RAM | 4/8 GB |
| HDMI input | **No** |
| HDMI output | 2x micro HDMI |
| USB 3.0 | 2x Type-A |
| USB 2.0 | 2x Type-A |
| Wi-Fi | Wi-Fi 5 (AC) + BT 5.0 onboard |
| M.2 | Via HAT+ (NVMe) |
| Ethernet | 1x Gigabit |
| Power | USB-C, 5V/5A |
| Form factor | Standard SBC |
| Linux support | Best-in-class — Raspberry Pi OS, Ubuntu, Armbian |
| VPU/encoder | **No hardware H.264 encoder accessible from Linux** |
| Price | ~$60 (4GB), ~$80 (8GB) |

**Verdict:** Best Linux support of any SBC, but critically lacks a usable
hardware H.264 encoder from userspace (the VideoCore VII encoder is not well
exposed). Software x264 encoding at 1080p30 would consume 1.5–2 of 4 CPU cores,
leaving limited headroom for bonding + cellular management. Only 8 GB RAM max.
The Pi is excellent for many things, but it's underpowered for this workload
compared to RK3588 boards.

### Khadas VIM4

| Spec | Value |
|---|---|
| SoC | Amlogic A311D2 (4x A73 + 4x A53) |
| RAM | 8 GB |
| HDMI input | **No** |
| USB 3.0 | 1x Type-C |
| USB 2.0 | 1x Type-A |
| Wi-Fi | Wi-Fi 6 + BT 5.1 onboard |
| Ethernet | 1x Gigabit |
| Linux support | Mixed — Khadas Ubuntu, limited Armbian |
| Price | ~$130 |

**Verdict:** Insufficient USB ports for 2–3 modems. Only 1x USB 3.0 port.
Amlogic Linux support is historically weaker than Rockchip. Not recommended.

---

## 4. Recommendation Matrix

| Board | HDMI In | USB for Modems | Wi-Fi AP | Linux Support | Portability | Score |
|---|---|---|---|---|---|---|
| **Radxa ROCK 5B+** | USB card | 4 ports ✓ | Onboard ✓ | Excellent | Compact | **★★★★★** |
| **Orange Pi 5 Plus** | Native ✓ | 3 ports ✓ | Needs M.2 | Good | Compact | ★★★★☆ |
| **Radxa ROCK 5 ITX** | Native ✓ | 4+ ports ✓ | Onboard ✓ | Good | Large | ★★★★☆ |
| Raspberry Pi 5 | USB card | 4 ports ✓ | Onboard ✓ | Best | Compact | ★★★☆☆ |
| Khadas VIM4 | USB card | 1 port ✗ | Onboard ✓ | Mixed | Compact | ★★☆☆☆ |

### Primary Pick: Radxa ROCK 5B+

- Onboard Wi-Fi 6 for AP mode — no extra module, no M.2 slot conflict
- 4 USB ports (2x 3.0, 2x 2.0) — plenty for 2–3 modems + USB HDMI capture
- 2x 2.5 GbE — future ethernet bonding option
- M.2 NVMe — local recording to fast storage
- Excellent Armbian support (Radxa is a first-party Armbian partner)
- 16/32 GB RAM options for headroom
- Proven thermal design with official heatsink/fan case

### Alternative: Orange Pi 5 Plus (if native HDMI input matters)

The native HDMI input eliminates the USB capture card, freeing a USB port and
removing one potential failure point. But it requires:
- Buying a separate M.2 Wi-Fi module for AP mode
- Reliance on BSP kernel HDMI RX driver (less proven than UVC)
- Community Armbian (not official)

Use the OPi5+ if you validate that the HDMI RX kernel driver is stable on your
target kernel version.

### Fixed Installation: Radxa ROCK 5 ITX

If the device lives in an enclosure (vehicle, studio rack), the ITX board gives
you everything native: HDMI input, 4x USB 3.0, onboard Wi-Fi, 2x 2.5GbE. The
larger form factor doesn't matter if it's permanently mounted.

---

## 5. Bill of Materials (ROCK 5B+ Config)

| Component | Part | Est. Cost |
|---|---|---|
| SBC | Radxa ROCK 5B+ (16 GB) | $150 |
| HDMI capture | USB3 HDMI capture dongle (1080p60 UVC) | $20 |
| Cellular modem × 2 | Quectel RM520N-GL (5G) USB adapter | $80 × 2 |
| SIM cards × 2 | Data plans on different carriers | varies |
| Antennas × 2 | LTE/5G external antennas (SMA) | $15 × 2 |
| Storage | 128 GB NVMe M.2 (local recording) | $25 |
| Power | USB-C PD charger (30W+) | $15 |
| Case | Radxa official heatsink case + fan | $15 |
| microSD | 32 GB (boot) | $8 |
| **Total** | | **~$420** |

For a 3-modem setup, add ~$95 (modem + antenna).

---

## 6. Software Image

The production sender should ship as a pre-built OS image:

```
Base:     Armbian Bookworm (Debian 12) minimal server
Packages: GStreamer 1.x, ModemManager, NetworkManager, hostapd, dnsmasq
Custom:   strata-agent (systemd service)
          integration_node binary
          libgstristbonding.so (GStreamer plugin)
          /etc/strata/agent.conf (pre-configured for AP mode on first boot)
```

### First-Boot Flow

See [04-sender-agent.md §10](04-sender-agent.md#10-ap-wi-fi-onboarding) for the
captive portal onboarding flow.

---

## 7. Thermal Considerations

H.264 encoding at 1080p30 with x264 `zerolatency` draws ~2–3W of CPU power on
RK3588 (uses 1–2 of the A76 cores). The bonding engine adds minimal CPU load
(~5%). Total board power draw under streaming load: ~8–12W.

| Board | Cooling | Sustained 1080p30 Encode | Notes |
|---|---|---|---|
| ROCK 5B+ | Heatsink + fan (official case) | Stable, no throttle | Fan kicks in above 60°C |
| OPi 5 Plus | Heatsink + fan | Stable | Similar thermal design |
| RPi 5 | Active cooler required | Borderline — uses most CPU | Will throttle without active cooling |

**Key:** Always use active cooling. Passive heatsinks alone are insufficient for
sustained encoding in warm environments (outdoor field use in summer).

---

## 8. Hardware VPU Encoding (Future)

The RK3588's VPU can encode H.264/H.265 at up to 8K30 with near-zero CPU usage.
GStreamer support is via `mpph264enc` (Rockchip MPP) or the newer `v4l2h264enc`
(V4L2 M2M). Both require BSP kernel patches and specific GStreamer plugin versions.

**Current status (early 2026):**
- `mpp` (Rockchip Media Process Platform) works with BSP kernel + patched GStreamer
- Mainline V4L2 M2M driver (`hantro` / `rkvdec2`) is not yet complete for encode
- Armbian ships MPP packages on some images

**Impact on pipeline:**

```bash
# Software encode (current — works everywhere)
v4l2src ! videoconvert ! x264enc tune=zerolatency bitrate=5000 ! mpegtsmux ! rsristbondsink ...

# Hardware encode (future — near-zero CPU)
v4l2src ! videoconvert ! mpph264enc ! h264parse ! mpegtsmux ! rsristbondsink ...
```

This is a pipeline-level change — no transport engine modifications needed.
Pursue VPU encoding once the ROCK 5B+ Armbian image has stable MPP support.
