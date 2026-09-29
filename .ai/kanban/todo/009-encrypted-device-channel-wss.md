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

- [ ] On first start the controller creates its own CA (key kept private on disk) and a server certificate for the device channel, and serves `wss://` on port 15002
- [ ] The agent connects over `wss://`; plain `ws://` remains only as an explicit opt-in for development
- [ ] The agent pins the controller: before adoption it records the certificate's fingerprint on first use (TOFU) and refuses a different one afterwards
- [ ] Certificates and keys are created with Rust crates only (no openssl CLI), and the whole stack builds for every feed architecture, mipsel included
- [ ] Unit tests for certificate creation and pinning; verified on bifrost: the agent connects over TLS, and a controller with a different certificate is refused

## Progress

Not started.
