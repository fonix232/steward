---
id: 8
title: 'Spike: TIP''s renderer on stock OpenWrt'
type: spike
priority: P0
category: foundation
effort: S
roles: [ap]
components: [research]
created: 2026-09-29
---

Can TIP's uCentral renderer drive a stock OpenWrt device? Decides between reusing it and rendering the schema ourselves.

## Acceptance criteria

- [x] Decide whether TIP's renderer can drive stock OpenWrt, with evidence from bifrost

## Progress

Answered: it needs patches for phy= radios, takes over the whole device (would stop uhttpd and rpcd) and depends on TIP-only services. Steward keeps the protocol and schema and renders itself.

Reviewed end to end twice (2026-09-29, 2026-09-30) against TIP's sources and bifrost. The decision stands; four sentences in the ucentral skill were corrected: `bridger`, `ratelimit` and `dhcpsnoop` are in the stock feeds (only `ucentral-state`, `event`, `udevstats` and `spotfilter` are TIP's), `unit.uc` sets the root password only when the config carries one, the services it stops are those TIP has templates for, and TIP's devices trust the CA they were provisioned with (or any self-signed one when allowed).
