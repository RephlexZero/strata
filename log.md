# Log

Append-only record of decisions, ingests, and meaningful changes. Newest at
the top. One dated entry per day — enough to reconstruct *why* later.

Format: `## YYYY-MM-DD` heading per day, bullet per entry.

## 2026-06-28

- adapt(encoder): **remove the post-FEC residual override on the encoder
  bitrate.** Sibling of the FEC-sizing fix: the residual (`ewma_loss_fec`) folds
  in reorder/late loss the encoder can't fix, so it must not move the bitrate.
  Deleted the two `ewma_loss_fec > 0.15` gates — `loss_pressure` (forced an
  encoder cut) and `loss_suppressed` (blocked ramp-up); both were binary,
  headroom-blind, and pinned/cut the encoder while spare bandwidth existed. The
  encoder now follows the continuous capacity path (per-link channel-loss
  discount → pressure, already correct), goodput shortfall (delivered < 0.7×
  offered — headroom-aware, reorder-immune), AQM self-congestion, and genuine
  per-link melt (`link_collapse`, the half `loss_pressure` legitimately
  bundled). Hardened goodput shortfall with a severe tier (< 0.5× pre-update
  target) that bypasses the post-increase grace — replacing the residual's grace
  pass-through with a trustworthy, staleness-robust signal. Residual is kept only
  for `jitter_loss_context` and the FEC burst-lift. New regression test
  `high_residual_loss_with_headroom_does_not_cut_encoder` (fails on old code);
  renamed `loss_pressure_gated_on_goodput` →
  `mild_residual_loss_with_healthy_goodput_does_not_cut`. 367 unit + all
  integration tests pass, clippy clean. New note Adaptation-Encoder-Cut-Signals
  + index row. Branch `fix/adapt-goodput-not-residual`.
- adapt(fec): **stop the FEC death spiral.** Traced why the post-fix run
  (orangepi-11528) stayed loss-bound. Root cause was NOT "lever 2" (per-link
  loss → EDPF, route around a bad link) — both links were clean (~2% wire
  loss; EDPF already de-rates by loss/delay/jitter). It was
  `recommended_fec_overhead` sizing parity from `ewma_loss_fec` (the *post-FEC
  residual*), which folds in cross-link reorder + late-arrival loss that parity
  cannot repair. Feedback loop: reorder loss → more parity → repair microbursts
  at generation boundaries overflow marginal-link buffers → more late/reorder
  loss → still more parity. Field evidence (run 2026-06-27, receiver
  65.109.5.169): per-link wire loss ~2% but receiver post-FEC residual ~17%
  cumulative (spiking ~36%), FEC overhead pinned at **41.6%** while the encoder
  sat at the **500 floor with 3.7 Mbps spare** and both links idle; 2.66× wire
  redundancy, playout buffer pinned at the 3 s cap 75% of ticks, 925
  discontinuities, 0 resync churn. The `self_congested` guard that pins FEC to
  baseline can't fire at the floor (pressure ≈ 0.085 ≪ 0.7; lowering it
  reintroduces the 2026-06-15 bursty-modem bug).
- adapt(fec): **fix** — size FEC parity to per-link CHANNEL loss
  (`max_link_loss`), not the post-FEC residual: strictly more correct (more
  protection for real channel loss, none for reorder/late). New field
  `max_link_loss` set in `update()`; driver swap in `recommended_fec_overhead`.
  New regression test `fec_overhead_not_inflated_by_reorder_residual`; existing
  `fec_overhead_pinned_under_self_congestion` updated to the channel-loss
  driver. 366 lib + integration tests pass; fmt clean. See
  [Adaptation-FEC-Sizing](wiki/Adaptation-FEC-Sizing.md), related
  [Adaptation-Delay-Pressure](wiki/Adaptation-Delay-Pressure.md).
- adapt(fec): **field-validated** (run orangepi-3870, 120 s, clean EOS,
  10 HLS segments, damaged=0). Hit *worse* radio than the baseline (link 0
  ~8% mean channel loss vs ~2%), yet receiver-side quality improved sharply:
  post-FEC residual mean **7%→1.5%** (90% of ticks <5% vs 63%), discontinuities
  **925→285**, egress-gate drops **1772→348**, playout pinned at the 3 s cap
  **75%→61%**. Death-spiral signature gone: **0** ticks of high-FEC (>25%) with
  low channel loss (<5%); all 21 high-FEC ticks co-occurred with genuine high
  channel loss. Encoder floor time now tracks real loss (mean channel loss
  12.5% at floor vs 8.3% above) — correct adaptation, not a phantom collapse.
  Note redundancy_enabled=false, so the wire overhead was FEC+retransmits only.
- **Open:** secondary contributor to the 2.66× — adaptive-redundancy
  duplication also floods when spare is huge. Watch on field validation.

## 2026-06-27

- Initialized AI workspace (LLM wiki pattern) from ai-workspace-template: added
  CLAUDE.md, hot.md, index.md, log.md, raw/, .claude/settings.json,
  .claude/commands/ (ingest, wiki-new, wiki-lint). AGENTS.md and GEMINI.md
  converted to symlinks → CLAUDE.md. Existing wiki/ pages registered in index.
- adapt: removed the `max_queue_depth >= 90/60` packet-count gates from
  `delay_pressure`/`late_pressure` in adaptation.rs. A deep paced queue is the
  *intended* state during keyframe bursts (queue is byte/drain-time bounded at
  0.5 s), so the gate misread benign bursts as bufferbloat and pinned the
  encoder to the 500 floor (~65 % of ticks, field run orangepi-3924: usable
  ~4.7 Mbps, pressure ~0.1, loss ≈ 0). Bufferbloat is now AQM-drop +
  receiver-delay based; genuine standing-queue congestion still caught via the
  pressure-gated self-congestion path. New regression test
  `deep_paced_queue_without_loss_does_not_cut`; 365 bonding tests pass. New
  wiki note [Adaptation-Delay-Pressure](wiki/Adaptation-Delay-Pressure.md).
  Post-fix field run orangepi-11528 confirmed pure-bufferbloat reduces 57→0;
  that run was loss-bound (~40 % post-FEC) so the avg-bitrate win is not yet
  demonstrated — real loss-driven collapse remains (lever 2 territory).
- meta: inverted doc symlinks — AGENTS.md is now the canonical file;
  CLAUDE.md and GEMINI.md are symlinks → AGENTS.md.
