---
id: 34
title: Package Suricata
type: feature
priority: P1
category: security
effort: L
roles: [router]
components: [feed]
needs_packaging: [suricata]
created: 2026-09-29
---

Suricata isn't in OpenWrt's feeds: build it for every architecture in Steward's feed.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Notes

A community Suricata 6 port stalled on the Rust toolchain; the feed's rustup build may unblock it.

## Progress

Not started.
