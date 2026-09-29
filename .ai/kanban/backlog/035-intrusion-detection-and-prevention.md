---
id: 35
title: Intrusion detection and prevention
type: feature
priority: P1
category: security
effort: XL
roles: [router]
components: [steward-agent, steward-controller, steward-web]
packages: [nftables]
needs_packaging: [suricata]
unifi: Intrusion Prevention
created: 2026-09-29
---

Suricata IDS with managed rules (ET Open) and alerts on the controller; inline IPS via NFQUEUE where the hardware can take it.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Notes

With the full ET Open ruleset Suricata wants 1-2 GB; inline IPS at line rate needs x86 or a big arm64 gateway.

## Progress

Not started.
