---
id: 69
title: Live checks waiting for a yes
type: chore
priority: P1
category: operations
effort: S
roles: [router, ap]
components: [steward-agent, steward-controller]
depends_on: [7]
created: 2026-09-30
---

What the reviews couldn't verify without the user's yes. Done on 2026-09-30, with the user's yes: STW-11's `rollback-check steward_test` on bifrost, and the SDK packages installed and removed on bifrost for STW-7, STW-14 and STW-20. Left:

- STW-7: CI builds and publishes after the first push of the reviewed history, and GitHub Pages is re-enabled.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
