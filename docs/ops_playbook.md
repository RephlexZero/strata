# Operations Playbook

## Quick Triage
1. Check `rist-bonding-stats` for link `phase`, `loss`, and `capacity`.
2. Confirm `stats_seq` is advancing and `heartbeat` is true.
3. Compare `total_capacity` to target bitrate.

## Common Symptoms

### Link oscillation (flapping between warm/degrade)
- **Indicators:** `phase` toggles, loss spikes, throughput jitter.
- **Actions:** Increase cooldown duration, raise degrade threshold, cap bitrate.

### Low throughput
- **Indicators:** `total_capacity` below target, `loss` rising.
- **Actions:** Reduce encoder bitrate; check link health and OS interface state.

### High latency / jitter
- **Indicators:** rising `rtt` and jitter, `phase` trending degrade.
- **Actions:** Increase receiver latency window; reduce target bitrate; verify link impairment.

## Live Tuning Knobs
- Encoder bitrate
- Link penalties / weights
- Lifecycle cooldown duration
- Receiver latency and skip policy

## Diagnostics
- Compare truth vs tracker plots in impairment tests.
- Inspect per-link `phase`, `loss`, and `capacity`.
- Check for stalled `stats_seq` or missing `heartbeat`.
