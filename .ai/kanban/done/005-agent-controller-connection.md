---
id: 5
title: Agent–controller connection
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

- [x] The agent connects, sends `connect` (serial, firmware, capabilities) and `state` every minute, and reconnects with backoff
- [x] The controller registers devices and sends a stored configuration whose uuid differs
- [x] Checked end to end on bifrost

## Progress

Done: the agent connects, reports `connect` and a `state` every minute, and reconnects with backoff; the controller registers devices and sends a stored configuration whose uuid differs.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: the backoff started over after any session, so a controller that closed at once was asked again every second; the device port had no time limits, served any number of connections before `connect`, and spun on a failed accept.
- Second: the agent gives the controller 30 s to take a connection and ends a session that hears nothing for 3 minutes (it pings with every state); the controller refuses messages over 1 MiB, cuts what a device sends to 128 bytes a value in its log, and drops a device silent for 3 minutes.
- Third: verified on bifrost: states every minute, the backoff after a stopped controller, a huge message refused, a silent device dropped at 180 s.
