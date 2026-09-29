---
id: 6
title: Find the controller automatically
type: feature
priority: P0
category: adoption-security
effort: S
roles: [router, ap, switch]
components: [steward-agent]
unifi: Device discovery and L3 adoption
created: 2026-09-29
---

A new device finds its controller without configuration: the default gateway first, then DNS-SD and a DHCP option.

## Acceptance criteria

- [x] No controller configured: the agent tries the default gateway

## Progress

Done: with no controller configured, the agent tries its default gateway. Finding one through DNS-SD or a DHCP option moved to STW-58.

Reviewed end to end twice (2026-09-29, 2026-09-30), nothing found: on bifrost the agent tried the address `ip route` names as its default gateway, backing off at 0, 1, 3 and 7 s.
