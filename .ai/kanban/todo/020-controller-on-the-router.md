---
id: 20
title: Controller on the router
type: feature
priority: P0
category: foundation
effort: S
roles: [router]
components: [steward]
unifi: Console (UDM/UCG)
created: 2026-09-29
---

Installing steward on the router hosts the controller and manages the router itself through its own agent, within the router's memory.

## Acceptance criteria

- [ ] Installing `steward` on the router starts the controller and the router's own agent (both enabled by default now that they work); installing `steward-agent` alone starts an agent that waits as pending until adopted
- [ ] The router's agent finds the controller on its own host first (something listening on 127.0.0.1:15002), before the default gateway (on a router, the ISP)
- [ ] The controller adopts its own host: a device connecting over loopback (only processes on the router can) is adopted without `steward-controller adopt`. Only when the init script asks for it (`--adopt-local`), so manual loopback tests still see pending
- [ ] Controller and agent together stay under 20 MB resident on the device, measured and recorded
- [ ] Verified on bifrost from SDK-built packages: both services run, the host's agent is adopted by itself and listed by `steward-controller devices`, and removing the packages leaves nothing running

## Progress

Not started.
