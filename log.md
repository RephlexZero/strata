# Log

Append-only record of decisions, ingests, and meaningful changes. Newest at
the top. One dated entry per day — enough to reconstruct *why* later.

Format: `## YYYY-MM-DD` heading per day, bullet per entry.

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
