---
id: 5
title: "Agent\u2013controller connection"
type: feature
priority: P0
category: foundation
effort: M
roles: [router, ap, switch]
components: [steward-agent, steward-controller]
created: 2026-09-29
---

The agent connects to the controller, reports identity, capabilities and state every minute, and reconnects with backoff; the controller keeps a device registry and provisions stored configurations by uuid.

## Acceptance criteria

- [ ] The agent connects, sends `connect` (serial, firmware, capabilities) and `state` every minute, and reconnects with backoff
- [ ] The controller registers devices and sends a stored configuration whose uuid differs
- [ ] Checked end to end on bifrost

## Progress

Not started.
