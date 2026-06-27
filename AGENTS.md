# Workspace operating guide — Strata

Strata is an open-source bonded cellular video transport (Rust). Knowledge
lives as plain markdown; read `hot.md` + `index.md` first, open `wiki/` pages
only when relevant. Keep this file under ~200 lines.

## Navigation protocol (read in this order)

1. **`hot.md`** — what's in flight right now. Read it first, every session.
2. **`index.md`** — one line per wiki page. Scan to decide relevance.
3. **`wiki/…`** — open only the pages flagged as relevant. Do **not** read the whole folder.
4. **`raw/…`** — original sources. Never read unless explicitly told to, or ingesting a new source.

## Maintenance protocol (after meaningful work)

- Append a dated line to **`log.md`** describing what changed or was decided.
- Update **`index.md`** if any `wiki/` pages were added, renamed, or retired.
- Refresh **`hot.md`** if the current focus shifted.
- Keep `wiki/` notes **atomic**: one concept per file, with frontmatter
  (`summary`, `tags`, `related`, `updated`).

## Strata architecture

**Crates:**
- `strata-bonding` — scheduler, adaptation, capacity oracle, per-link transport
- `strata-transport` — wire format, FEC, ARQ, congestion control (BBR-based)
- `strata-gst` — GStreamer sink/source elements, `strata-pipeline` binary

**Sender path:** GStreamer pipeline → `mpegtsmux` → `stratasink` →
`BondingScheduler` → per-link `TransportLink` (UDP)

**Receiver path:** `strata_receiver` → packet reassembly → `stratasrc` → HLS/RTMP

**Key source files:**
- `crates/strata-bonding/src/adaptation.rs` — `BitrateAdapter`, ramp-up/down, feedback loop
- `crates/strata-bonding/src/scheduler/bonding.rs` — `BondingScheduler`, EDPF, BLEST, IoDS
- `crates/strata-bonding/src/net/transport.rs` — `TransportLink`, capacity estimation, pacing
- `crates/strata-bonding/src/scheduler/oracle.rs` — `CapacityOracle` (PPD-based)
- `crates/strata-transport/src/congestion.rs` — `BiscayController` (BBR-based)
- `crates/strata-gst/src/bin/strata_pipeline.rs` — GStreamer sender pipeline construction
- `crates/strata-bonding/src/config.rs` — `SchedulerConfig` (capacity_floor_bps = 5 Mbps)

**Key patterns:**
- Per-link alive detection: ≥50% loss for 3+ windows → dead
- Capacity chain: Oracle (PPD) → BBR btl_bw → ack_delivery_bps fallback
- Adaptation defaults: ramp_up=250 kbps/step, ramp_down_factor=0.7, grace_period=5 s
- PAT/PMT interval = 1 (every packet) for loss resilience
- SO_SNDBUF = 512 KB to absorb startup burst

**Hardware (dev/test):** 2× Huawei HiLink modems, Band 8 (900 MHz)
- Modem 1: `enp2s0f0u4` (192.168.8.x) · Modem 2: `enp11s0f3u1u3` (192.168.9.x)
- Band lock: `scripts/band-lock.sh`

## Build & test

```bash
cargo check                              # quick compile check
cargo test -p strata-bonding --lib      # 350+ unit tests
cargo test -p strata-bonding --tests    # integration tests (multi-link, pipeline)
STRATA_NETEM_TESTS=1 cargo test ...    # netem/netns tests — needs CAP_NET_ADMIN
```

Pre-existing warnings in `strata-gst` (unused mut, unused var) — not our changes.

## Coding & collaboration rails

- **Surgical changes.** Touch only what the task needs. No drive-by refactors.
- **Simplicity first.** No features, abstractions, or error handling beyond
  what was asked. Three similar lines beat a premature abstraction.
- **Verify before claiming done.** Run it, show the output. Don't assert
  success you haven't observed.
- **Match the surrounding code.** Naming, comment density, and idioms should
  read like what's already there.
- **No sycophancy.** If a request is wrong or based on a wrong premise, say
  so plainly and propose the better path.
- **Ask when genuinely uncertain.** A clarifying question beats a confident
  guess that has to be unwound later.
- **No comments unless the WHY is non-obvious.** Hidden constraint, subtle
  invariant, workaround for a specific bug — otherwise omit.

## House rules

- Everything that must travel between machines must be a **committed file** —
  shell hooks and `~/.claude/` config are per-machine.
- `CLAUDE.md` and `GEMINI.md` are symlinks to `AGENTS.md` (the canonical file).
  Edit `AGENTS.md` only.
- Don't `@import` `wiki/` or `raw/` here — imports load at launch and defeat
  the index-first savings.
