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

- [x] Four packages build in the official SDK for every architecture in `packages.sh`
- [ ] CI builds only what changed and publishes a signed apk repository per architecture to gh-pages
- [x] SDK-built binaries run on bifrost and install and remove cleanly

## Progress

Packages build in the SDK for aarch64 and mipsel, run on bifrost, and install and remove cleanly. Not verified yet: CI hasn't run on this history. The feed is built and published by the first push, after which GitHub Pages needs re-enabling.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit: the build script counts as a package input; a missing package is built for the architecture that lacks it only; a published file is never replaced; the sparse checkout comes before `feeds update`; the SDK must match `STEWARD_ARCH`; and the plan refuses to publish a build older than the feed's (a rewritten main with older dates). An SDK build for aarch64 at the head passed, and the feed scripts' harness passes 70 checks.

Installed and removed on bifrost from an SDK build of this history (2026-09-30, with the user's yes): all four packages, both services running, removal leaving nothing behind but the controller's and the agent's data. Still to verify: CI builds and publishes after the first push of this history (then GitHub Pages needs re-enabling).
