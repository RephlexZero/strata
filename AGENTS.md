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
- `crates/strata-gst/src/bin/strata_pipeline/` — the strata-pipeline binary (cli/sender/receiver/gate/hotswap modules)
- `crates/strata-bonding/src/config.rs` — `SchedulerConfig` (capacity_floor_bps default = 1.5 Mbps; the control plane no longer overrides this for platform-started streams — raw/PLATFORM_REVIEW.md E5 fixed 2026-07-02)

**Key patterns:**
- Per-link alive detection: ≥50% loss for 3+ windows → dead
- Capacity chain: Oracle (PPD) → BBR btl_bw → ack_delivery_bps fallback
- Adaptation defaults: ramp_up=250 kbps/step, ramp_down_factor=0.7, grace_period=5 s
- PAT/PMT interval = 9000 (90 kHz ticks = 100 ms) for loss resilience. **Not 1**:
  the property is in 90 kHz ticks, so `=1` (~11 µs) emits PAT+PMT before nearly
  every packet and tripled wire bandwidth (field: 2.3 Mbps video → 7 Mbps muxed),
  overflowing the paced-queue AQM into self-inflicted loss. PAT/PMT carry the
  HEADER flag → Critical priority → FEC-protected, so 100 ms is ample resilience.
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

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **strata** (6143 symbols, 17651 relationships, 286 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> Index stale? Run `node .gitnexus/run.cjs analyze` from the project root — it auto-selects an available runner. No `.gitnexus/run.cjs` yet? `npx gitnexus analyze` (npm 11 crash → `npm i -g gitnexus`; #1939).

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows. For regression review, compare against the default branch: `detect_changes({scope: "compare", base_ref: "main"})`.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `query({search_query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `context({name: "symbolName"})`.
- For security review, `explain({target: "fileOrSymbol"})` lists taint findings (source→sink flows; needs `analyze --pdg`).

## Never Do

- NEVER edit a function, class, or method without first running `impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `rename` which understands the call graph.
- NEVER commit changes without running `detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/strata/context` | Codebase overview, check index freshness |
| `gitnexus://repo/strata/clusters` | All functional areas |
| `gitnexus://repo/strata/processes` | All execution flows |
| `gitnexus://repo/strata/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->
