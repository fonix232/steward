---
id: 62
title: Harden adoption
type: feature
priority: P1
category: adoption-security
effort: M
roles: [router, ap, switch]
components: [steward-controller, steward-agent]
depends_on: [10, 20]
created: 2026-09-30
---

What the end-to-end reviews of adoption left open (the channel's own limits came with STW-5 and STW-10).

- Over loopback, any local process can re-adopt the router's own serial (with no credential, an old one or a wrong one), even while the router's real agent is connected: it gets a new credential and then the router's configuration, secrets included, and the real agent is disconnected. Re-adopt only when no connection holding a valid credential has the serial, or have the local agent present a secret only root can read.
- An adoption approved while its device is offline goes to whoever next connects with that serial, from anywhere. Bind the approval to the connection the admin saw, or require the device to be connected.
- With 64 pending devices connected, the router's own agent arriving over loopback drops one of them: the pending list is trimmed before adopt-local decides. Decide adopt-local first.
- Installing `steward` on a router starts the agent a moment before the controller exists, so its first try goes to the default gateway, the ISP's (seen on bifrost, 2026-09-30). Were anything to answer there, the agent would pin it on first use. Hold the agent's first try until the local controller is installed and started, or never trust a first use on the default gateway when the host runs a controller.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
