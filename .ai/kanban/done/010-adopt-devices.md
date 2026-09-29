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

- [x] A device's first connection registers it as pending: the controller answers nothing but `connect`/`state` bookkeeping, and sends it no configuration
- [x] Adopting a device (a controller command for now; the web interface comes with STW web tasks) issues it a credential, which the agent stores and presents on every later connection
- [x] An adopted device that presents no or a wrong credential is refused; forgetting a device revokes its credential
- [x] Controller state (adopted devices, credentials) survives restarts
- [x] Unit tests for the adoption state machine; verified on bifrost: pending → adopt → reconnect with credential → configuration provisioned; a forged credential is refused

## Progress

Works: new devices wait as pending and get nothing; `steward-controller devices | adopt <serial> | forget <serial>` over the control socket; adoption delivers a credential (steward.adopt) the agent keeps at 0600 and presents on every upgrade; the controller keeps only its SHA-256; a missing or forged credential is refused (1008, reason logged); forget revokes. Verified: 3 new tests (mutation-checked), SDK builds aarch64 + mipsel, and every criterion on bifrost. To check: run the loop from .ai/skills/device-testing and adopt a device. Not yet: adopting from the web interface (steward-web tasks).

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: pending devices were unbounded and written to flash; now at most 64, in memory, what they report cut to 128 bytes.
- Second: pending connections bounded with their records (the oldest disconnected one dropped first, and only when all are connected the oldest connected one, closed with it); the control socket survives a failed accept; an agent with a credential but no pin doesn't connect, rather than hand the credential to a controller it would trust on first use.
- Third: verified on bifrost: pending, adopt, reconnect with the credential, a forged one refused, forget; a flood of 150 devices bounded, a connected pending device kept through 100 that hang up.
