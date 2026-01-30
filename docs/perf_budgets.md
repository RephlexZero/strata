# Performance Budgets

These are target budgets for production readiness.

## CPU
- **Per stream:** <= 1 core at 30fps (baseline)
- **Global ceiling:** <= 70% on a 4-core host

## Memory
- **Per stream buffering:** <= 64 MB
- **Total process memory:** <= 512 MB

## Latency
- **End-to-end target:** <= 200 ms
- **p95 jitter impact:** <= 50 ms above target
