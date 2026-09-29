---
id: 7
title: Packages and per-architecture feed
type: feature
priority: P0
category: foundation
effort: L
components: [steward.mk, .github/scripts, feed]
created: 2026-09-29
---

Four apk packages (steward, steward-agent, steward-controller, steward-web) and a signed apk repository per architecture on GitHub Pages, published by CI with gh-pages mirroring main.

## Acceptance criteria

- [ ] Four packages build in the official SDK for every architecture in `packages.sh`
- [ ] CI builds only what changed and publishes a signed apk repository per architecture to gh-pages
- [ ] SDK-built binaries run on bifrost and install and remove cleanly

## Progress

Not started.
