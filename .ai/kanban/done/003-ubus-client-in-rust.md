---
id: 3
title: ubus client in Rust
type: feature
priority: P0
category: foundation
effort: M
roles: [router, ap, switch]
components: [crates/ubus]
packages: [ubus, rpcd]
created: 2026-09-29
---

The agent's way onto the device: a pure-Rust ubus client (no libubus, no bindgen) for lookup and invoke.

## Acceptance criteria

- [x] Lookup and invoke against ubusd, without libubus
- [x] blob/blobmsg encoding matches libubox byte for byte (unit test)
- [x] On bifrost, answers equal the `ubus` CLI's, key order included

## Progress

Done.
