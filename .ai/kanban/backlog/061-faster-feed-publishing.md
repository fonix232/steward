---
id: 61
title: Faster feed publishing
type: chore
priority: P3
category: foundation
effort: S
components: [.github]
depends_on: [7]
created: 2026-09-29
---

The publish job spends almost all of its 17 minutes downloading the 285 MB x86-64 SDK only to run `apk mkndx`, and the x86_64 build loses the same time. Cache the SDK between runs, or sign the index with a lighter apk-tools 3 image once it's shown that OpenWrt's apk accepts that index.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
