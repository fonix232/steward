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

- [x] Per device, the controller keeps: its record (standing, credential hash, first seen, adopted), what it reported on connecting (model, firmware, capabilities), when it was last connected and last seen, the address it connected from, its latest state (with when it arrived and the uuid it ran), and the answer to its last `configure` (uuid, error, text, rejections, when).
- [x] Nothing from a device that isn't adopted goes to flash beyond its pending record (model, firmware): capabilities and state are kept for adopted devices only.
- [x] Flash is written only on events: adoption, forgetting, connecting, disconnecting, a configuration stored, a `configure` answer. A state message doesn't write to flash: states are written when their device disconnects, hourly when changed, and when the controller stops (SIGTERM, SIGINT).
- [x] Everything lives in the state directory (`/etc/steward/`, kept by sysupgrade): `devices.json`, `configs/<serial>.json`, `state/<serial>.json`. Every write is atomic (temporary file, fsync, rename); files holding secrets or state are 0600.
- [x] After a restart, `devices` (control socket and API) shows every known device as before: offline ones with their last seen time, last state and last answer.
- [x] Forgetting a device removes its stored state and configuration with its record.
- [x] Unit tests: round trip through a restart, nothing written for a state message, forget removes the files, a corrupt state file doesn't stop the controller (it's skipped and logged).
- [x] On bifrost: the controller–agent loop with a scratch state dir; stop it (SIGTERM), check the files, start it again, and `devices` shows the device's last state before it reconnects. With the SDK package installed (not enabled), `sysupgrade -l` lists `/etc/steward/` files. Removed afterwards.

## Notes

- JSON files, not SQLite: tens of devices, each file small; no extra dependency or size on the router.

## Progress

- Done: `store.rs` (atomic 0600 writes, serial check), `states.rs` (latest states in memory; written on disconnect, hourly, at stop; adopted only), device records with capabilities, last connected/seen, address and the last `configure` answer; stop records last seen; forget removes state and configuration; `devices` shows `seen <n> ago`, `--json devices` the records.
- Verified: fmt, clippy, 48 tests (6 new); SDK builds aarch64 + mipsel; bifrost loopback run (SIGTERM write, offline listing after restart, answer kept, disconnect write, forget cleanup, `sysupgrade -l`), cleaned up.
- Not as written: `sysupgrade -l` was checked with the package's keep file placed by hand, not with the package installed, because the package starts the controller on install.

- Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time: leftover temporary files removed first and the directory fsynced, a damaged `devices.json` stops the controller rather than start empty; no state kept for a device that isn't adopted, a refused connection changes nothing on flash, serials lower-case only; at most 64 KiB of capabilities kept, and an adopted device over that keeps what it reported before. All verified on bifrost.
- Verified with the SDK-built packages installed on bifrost (2026-09-30, with the user's yes): `sysupgrade -l` lists `devices.json` and the CA under `/etc/steward/`, the agent's pin and credential under `/etc/steward-agent/`, and both UCI configs; removed afterwards.
