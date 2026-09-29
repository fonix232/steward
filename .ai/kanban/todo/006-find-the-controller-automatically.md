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

- [ ] No controller configured: the agent tries the default gateway (done)
- [ ] DNS-SD (`_steward._tcp`) and a DHCP option (not yet)

## Progress

Not started.
