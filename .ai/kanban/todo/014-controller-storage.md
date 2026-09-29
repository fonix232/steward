---
id: 14
title: Controller storage
type: feature
priority: P0
category: foundation
effort: M
roles: [router]
components: [steward-controller]
unifi: Controller database
created: 2026-09-29
---

Devices, configurations and the latest state persisted on the controller's host, surviving restarts and upgrades.

## Acceptance criteria

- [ ] Per device, the controller keeps: its record (standing, credential hash, first seen, adopted), what it reported on connecting (model, firmware, capabilities), when it was last connected and last seen, the address it connected from, its latest state (with when it arrived and the uuid it ran), and the answer to its last `configure` (uuid, error, text, rejections, when).
- [ ] Nothing from a device that isn't adopted goes to flash beyond its pending record (model, firmware): capabilities and state are kept for adopted devices only.
- [ ] Flash is written only on events: adoption, forgetting, connecting, disconnecting, a configuration stored, a `configure` answer. A state message doesn't write to flash: states are written when their device disconnects, hourly when changed, and when the controller stops (SIGTERM, SIGINT).
- [ ] Everything lives in the state directory (`/etc/steward/`, kept by sysupgrade): `devices.json`, `configs/<serial>.json`, `state/<serial>.json`. Every write is atomic (temporary file, fsync, rename); files holding secrets or state are 0600.
- [ ] After a restart, `devices` (control socket and API) shows every known device as before: offline ones with their last seen time, last state and last answer.
- [ ] Forgetting a device removes its stored state and configuration with its record.
- [ ] Unit tests: round trip through a restart, nothing written for a state message, forget removes the files, a corrupt state file doesn't stop the controller (it's skipped and logged).
- [ ] On bifrost: the controller–agent loop with a scratch state dir; stop it (SIGTERM), check the files, start it again, and `devices` shows the device's last state before it reconnects. With the SDK package installed (not enabled), `sysupgrade -l` lists `/etc/steward/` files. Removed afterwards.

## Notes

- JSON files, not SQLite: tens of devices, each file small; no extra dependency or size on the router.

## Progress

Not started.
