---
id: 15
title: Controller API
type: feature
priority: P0
category: foundation
effort: M
roles: [router]
components: [steward-controller]
created: 2026-09-29
---

A REST and live-update API that the web interface (and scripts) use.

## Acceptance criteria

- [ ] The controller serves an HTTPS API on its web port (8443), with the same certificate authority as the device channel
- [ ] Sign-in uses OpenWrt's own users: `POST /api/login` checks the username and password with rpcd (`session login`), the same accounts as LuCI, and returns a session token. Every other call needs the token, and rpcd's session timeout and logout apply
- [ ] `GET /api/devices` lists devices: standing, connected, model, firmware, the running configuration, and the last state. `POST /api/devices/<serial>/adopt` and `/forget` do what the control socket does, through the same code
- [ ] `GET` and `PUT /api/devices/<serial>/config` read and store a device's uCentral configuration. PUT gives it a new uuid and sends it to the device when it's adopted and connected
- [ ] `GET /api/events` streams changes as Server-Sent Events (connected, disconnected, state, adopted, forgotten, configuration answers), so the interface stays live without polling
- [ ] Tests exercise every route in-process (no network); verified on bifrost over HTTPS with curl: login with the root password, list, adopt, store a configuration, and a wrong password or missing token refused

## Progress

Not started.
