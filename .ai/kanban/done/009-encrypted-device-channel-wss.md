---
id: 9
title: Encrypted device channel (wss://)
type: feature
priority: P0
category: adoption-security
effort: M
roles: [router, ap, switch]
components: [steward-controller, steward-agent]
unifi: Encrypted inform
created: 2026-09-29
---

The controller creates its own certificate authority on first start and serves the device channel over TLS.

## Acceptance criteria

- [x] On first start the controller creates its own CA (key kept private on disk) and a server certificate for the device channel, and serves `wss://` on port 15002
- [x] The agent connects over `wss://`; plain `ws://` remains only as an explicit opt-in for development
- [x] The agent pins the controller: before adoption it records the certificate's fingerprint on first use (TOFU) and refuses a different one afterwards
- [x] Certificates and keys are created with Rust crates only (no openssl CLI), and the whole stack builds for every feed architecture, mipsel included
- [x] Unit tests for certificate creation and pinning; verified on bifrost: the agent connects over TLS, and a controller with a different certificate is refused

## Progress

Works: the controller creates its CA and server certificate on first start and serves wss://; the agent pins the CA on first use and refuses any other controller; ws:// only with explicit dev flags; the CA and the pin survive sysupgrade. Verified: 4 new unit tests (+ a mutation check), SDK builds for aarch64 and mipsel, and on bifrost (static and SDK builds): pin, reconnect, impostor refused, ws:// refused, keys 0600. To check: run a controller and an agent and compare the fingerprints they log.

Reviewed end to end three times (2026-09-29 and 30). The first review found a pin file that couldn't be read taken for no pin (so a new controller was pinned) and no time limit on the TLS handshake; both were fixed in this commit, with tests that go red when the fix is undone. The second found them holding on bifrost (an emptied or non-PEM pin stops the session and is left alone; no handshake in 10 s ends the connection). Its one finding, a credential sent to a controller trusted on first use after the pin was lost, is in STW-10's code and fixed there. The third, after the channel's time limits were merged into the TLS code, found them whole on bifrost (a second controller refused, an emptied pin left alone, TLS and the upgrade within one 30 s limit) and an SDK build for mipsel.
