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

- [x] Changes staged in an rpcd session of the agent's own, granted only the named configs
- [x] An unconfirmed apply reverts by itself; a confirmed one stays (checked on bifrost)
- [x] A write outside the grant is refused

## Progress

Done: `uci::Transaction` stages in an rpcd session granted only the configs it names, applies with rpcd's rollback and confirms; the revert and the confirm were checked on bifrost when it was done, on a config of the check's own.

Reviewed end to end twice (2026-09-29, 2026-09-30), nothing found. The second review refused a session granted network, wireless, dhcp and firewall both `get system` and an add to system on bifrost, and found the apply, confirm and rollback code unchanged since.
