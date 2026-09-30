---
id: 68
title: 'Routed networks: overlaps and restarts'
type: bug
priority: P2
category: gateway
effort: S
roles: [router]
components: [crates/render, steward-agent]
depends_on: [32]
created: 2026-09-30
---

- The overlap check sees only static `ipaddr`s: a DHCP client's runtime subnet (the router's `wan`) and WireGuard `addresses` aren't checked, so a routed network can be staged on top of them.
- DHCP and DNS only go where dnsmasq runs, and zones only where fw4 is active (STW-32): if either is stopped for a moment when a configuration is applied, its owned pools, reservations, records or zones are deleted (answered 1, with the reason), and nothing puts them back when the service returns, until a new configuration arrives. Re-apply when the service comes back, or keep what's owned while it's down.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
