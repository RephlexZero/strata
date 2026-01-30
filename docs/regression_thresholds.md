# Regression Thresholds

These thresholds define pass/fail criteria for production regressions.

## Impaired E2E
- **Average capacity (link 2):** 0.3–3.0 Mbps
- **Average loss (link 2):** <= 20%
- **Schema presence:** `schema_version=1`, `heartbeat=true`, monotonic `stats_seq`

## Lifecycle Recovery
- **Stale stats reset:** within 3s of missing stats
- **Cooldown period:** 2s (default)

## Receiver Metrics (Future)
- **Latency p95:** <= configured start latency + jitter budget
- **Reorder rate:** <= 2% for stable links
