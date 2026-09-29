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

- [x] Installing `steward` on the router starts the controller and the router's own agent (both enabled by default now that they work); installing `steward-agent` alone starts an agent that waits as pending until adopted
- [x] The router's agent finds the controller on its own host first (something listening on 127.0.0.1:15002), before the default gateway (on a router, the ISP)
- [x] The controller adopts its own host: a device connecting over loopback (only processes on the router can) is adopted without `steward-controller adopt`. Only when the init script asks for it (`--adopt-local`), so manual loopback tests still see pending
- [x] Controller and agent together stay under 20 MB resident on the device, measured and recorded
- [x] Verified on bifrost from SDK-built packages: both services run, the host's agent is adopted by itself and listed by `steward-controller devices`, and removing the packages leaves nothing running

## Progress

Works: installing steward starts the controller and the router's own agent; the agent finds the local controller through UCI (not the ISP gateway); the controller adopts its own host over loopback (--adopt-local from the init script), again if it loses its credential; an agent alone waits pending. Memory on bifrost: controller 2.6 MB + agent 2.5 MB resident. To check: install steward on a router and run steward-controller devices.

Not verified on an actual router: bifrost (an access point) stood in as the host. The case this card exists for, a router whose default gateway is the ISP, is still to be seen.

Reviewed end to end three times (2026-09-29 and 30). The first found that `--adopt-local` adopted any serial claimed over loopback; now only the host's own serial is, and a device adopted from the network is never re-adopted over loopback. The rest verified on bifrost: the host adopted by itself and again after losing its credential, another serial over loopback pending, a network-adopted device never taken over, 4.1 MB + 3.0 MB resident.

Verified from SDK-built packages on bifrost (2026-09-30, with the user's yes): installing `steward` started both services; the agent found the local controller through UCI and connected over loopback, and the controller adopted its own host and delivered its credential; `devices` listed it adopted and connected; 3.3 MB + 3.0 MB resident. `steward-agent` alone ran and tried the default gateway with backoff. Removing the packages left nothing running, and bifrost's configs were unchanged. One thing seen: installing `steward` starts the agent a moment before the controller exists, so its very first try goes to the default gateway (STW-62).
