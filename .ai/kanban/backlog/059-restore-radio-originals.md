---
id: 59
title: Restore radios' original values
type: feature
priority: P2
category: wifi
effort: S
roles: [router, ap]
components: [steward-agent]
depends_on: [11]
created: 2026-09-29
---

The agent records each radio option's original value before it first changes it (`radio-originals.json`, STW-11), but nothing restores them yet. Put them back when a device is forgotten or stops being managed, so the device returns to its own radio settings.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
