---
id: 58
title: 'Find the controller: DNS-SD and a DHCP option'
type: feature
priority: P2
category: adoption-security
effort: M
roles: [ap, switch]
components: [steward-agent, steward-controller]
packages: [umdns, odhcpd, dnsmasq]
unifi: Inform URL discovery (DHCP option 43, DNS)
depends_on: [6]
created: 2026-09-29
---

Agents find a controller that isn't their default gateway: the controller announces `_steward._tcp` over mDNS, and the router can hand the controller's address out in a DHCP option. Split from STW-6, which covers the default gateway.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
