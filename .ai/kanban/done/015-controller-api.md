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

- [x] The controller serves an HTTPS API on its web port (8443), with the same certificate authority as the device channel
- [x] Sign-in uses OpenWrt's own users: `POST /api/login` checks the username and password with rpcd (`session login`), the same accounts as LuCI, and returns a session token. Every other call needs the token, and rpcd's session timeout and logout apply
- [x] `GET /api/devices` lists devices: standing, connected, model, firmware, the running configuration, and the last state. `POST /api/devices/<serial>/adopt` and `/forget` do what the control socket does, through the same code
- [x] `GET` and `PUT /api/devices/<serial>/config` read and store a device's uCentral configuration. PUT gives it a new uuid and sends it to the device when it's adopted and connected
- [x] `GET /api/events` streams changes as Server-Sent Events (connected, disconnected, state, adopted, forgotten, configuration answers), so the interface stays live without polling
- [x] Tests exercise every route in-process (no network); verified on bifrost over HTTPS with curl: login with the root password, list, adopt, store a configuration, and a wrong password or missing token refused

## Progress

Works: HTTPS API on 8443 with sign-in through rpcd (the router's own accounts, as LuCI), devices/adopt/forget/config and a live event stream; the control socket and the API share the hub's operations. Verified in-process and on bifrost over HTTPS. To check: curl -k https://<router>:8443/api/login with the root account.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: rpcd's null session refused without asking rpcd; the `steward` access group checked on every call (read for GETs and the stream, write for changes); the event stream ends with its session; no answer carries a credential's hash.
- Second: writes refused from another origin or site (`Origin`, `Sec-Fetch-Site`), a cookie-signed write needs `X-Steward: 1`, the cookie is `Path=/api`; 10 s for the TLS handshake, the headers and the body; 32 connections at once; a failed accept waits a second; sign-in takes at most 4 KiB.
- Third: verified on bifrost: every CSRF case refused, the time limits and the cap held, sessions from `session create` + `grant`. A sign-in with a real password wasn't repeated (only the user may use it).
