---
id: 64
title: Bound what unadopted connections can hold
type: bug
priority: P1
category: adoption-security
effort: S
roles: [router]
components: [steward-controller]
depends_on: [5, 10]
created: 2026-09-30
---

Messages on the device channel are capped at 1 MiB (STW-5), and a pending device's state isn't kept (STW-14). But a connection keeps its buffers after a large message: 16 pending devices each sending 1 MB took the controller from 3.7 to 21.4 MB on bifrost, and 64 pending plus 32 handshaking connections could hold about 100 MB, a lot for a router. A much smaller message limit until a device is adopted (its state is thrown away anyway), or a memory budget across unadopted connections.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
