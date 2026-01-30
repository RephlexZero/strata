# Privilege Matrix

This document describes the minimum Linux privileges needed per component.

## Components

| Component | Purpose | Minimum Privileges |
| --- | --- | --- |
| gst-rist-bonding (sink/src) | Production pipeline | None (user-level) |
| rist-bonding-core | Core scheduling and receiver | None (user-level) |
| integration_node | Test harness + stats relay | None (user-level) |
| rist-network-sim | netns + tc impairments | CAP_NET_ADMIN (or sudo) |
| Tests with netns | impaired_e2e / robustness | CAP_NET_ADMIN (or sudo) |

## Notes
- CAP_NET_ADMIN is required to create network namespaces and configure `tc`.
- CAP_NET_RAW may be required if raw sockets or special routing are used in the future.
- Production deployments should avoid elevated privileges unless strictly necessary.
