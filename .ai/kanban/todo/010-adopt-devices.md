---
id: 10
title: Adopt devices
type: feature
priority: P0
category: adoption-security
effort: L
roles: [router, ap, switch]
components: [steward-controller, steward-agent, steward-web]
unifi: Device adoption
created: 2026-09-29
---

A new device shows as pending; adopting it gives it a credential and the controller's certificate fingerprint to pin. Unadopted devices get nothing.

## Acceptance criteria

- [ ] A device's first connection registers it as pending: the controller answers nothing but `connect`/`state` bookkeeping, and sends it no configuration
- [ ] Adopting a device (a controller command for now; the web interface comes with STW web tasks) issues it a credential, which the agent stores and presents on every later connection
- [ ] An adopted device that presents no or a wrong credential is refused; forgetting a device revokes its credential
- [ ] Controller state (adopted devices, credentials) survives restarts
- [ ] Unit tests for the adoption state machine; verified on bifrost: pending → adopt → reconnect with credential → configuration provisioned; a forged credential is refused

## Progress

Not started.
