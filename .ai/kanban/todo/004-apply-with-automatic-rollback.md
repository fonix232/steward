---
id: 4
title: Apply with automatic rollback
type: feature
priority: P0
category: foundation
effort: M
roles: [router, ap, switch]
components: [crates/ubus]
packages: [rpcd]
created: 2026-09-29
---

uci::Transaction stages UCI changes in an rpcd session of its own, applies them with rpcd's rollback and confirms once the controller is reachable again, so a change that cuts a device off undoes itself.

## Acceptance criteria

- [ ] Changes staged in an rpcd session of the agent's own, granted only the named configs
- [ ] An unconfirmed apply reverts by itself; a confirmed one stays (checked on bifrost)
- [ ] A write outside the grant is refused

## Progress

Not started.
