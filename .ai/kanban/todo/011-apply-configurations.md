---
id: 11
title: Apply configurations
type: feature
priority: P0
category: foundation
effort: M
roles: [router, ap, switch]
components: [steward-agent]
packages: [rpcd]
created: 2026-09-29
---

configure goes through the renderer and uci::Transaction, is confirmed once the controller answers again, persists the running uuid and is answered 0, 1 or 2 honestly.

## Acceptance criteria

- [ ] configure goes through the renderer and uci::Transaction, applied with rpcd's rollback (60 s)
- [ ] Confirmed only once the controller answers again (a probe connection that doesn't register); otherwise rpcd reverts it
- [ ] The running uuid persists; a configuration that rolled back is refused until a new uuid (no apply/rollback loop)
- [ ] configure answered 0, 1 (with rejections) or 2, honestly
- [ ] Radio options' original values recorded before the agent first changes them
- [ ] Verified live on bifrost (with the user's yes): apply+confirm, rollback, refusal, cleanup; wireless byte-identical afterwards

## Progress

Not started.
