---
name: package-feed
description: How Steward's apk packages are built and published. Covers packages.sh (packages, architectures, pkg_inputs), the SDK build on a rustup toolchain, and the per-architecture feed on gh-pages. Use when adding a package or an architecture, changing a Makefile or a build script, building packages locally, or debugging a failed feed run.
---

# Packages and the feed

## What gets built

`.github/scripts/packages.sh` is the single list:
- `PKGS`: the packages, one top-level directory each.
- `TARGETS`: `<arch>:<sdk>` pairs. Each architecture is built by `openwrt/sdk:<sdk>-openwrt-25.12` and published as its own apk repository, `apk/<arch>/`.
- `pkg_inputs <pkg>`: everything that changes a package's bytes, `sdk-build.sh` included. A change to any of them rebuilds it, and its release number is the date of the last main commit that touched them (`steward.mk`).

**A published name-version never changes its bytes**: a device, a cache or a mirror that has the file would no longer match the index. So every input that changes a package's bytes belongs in `pkg_inputs`, which gives a rebuild a new release, and `publish.sh` never replaces a published file. The release has minute resolution: a second change to a package within the same minute gets the same name, so its build is dropped for the published one, and it reaches the feed with the next change.

Every Makefile includes `steward.mk` after `rules.mk`, with `PKG_NAME` set. That gives `PKG_VERSION` (from the cargo workspace), `PKG_RELEASE`, the license, and for the Rust packages `Steward/Prepare/Cargo`, which copies the workspace into the build directory. The architecture-independent packages (`steward`, `steward-web`) are built into every architecture's repository, so a device needs only its own.

`apk del` takes a package's files, its UCI config and the feed list with it, and that's what "removes cleanly" means. It leaves `/etc/steward/`, which the controller's init script creates when it starts (each device's configuration is kept there), and `/etc/steward-agent/`, which the agent writes (the pinned CA, its credential, `running.json`, `originals.json`). That's the controller's and the agent's data, not the package's, and it's kept on purpose, so a reinstall picks up where the last one stopped.

## The SDK build (`.github/scripts/sdk-build.sh`)

`rust/host` from the packages feed builds rustc and LLVM from source, which takes hours per SDK. So CI:
1. exports `STEWARD_RUSTUP=1`, which drops the `rust/host` build dependency (`steward.mk`);
2. takes `rust-package.mk` and `rust-values.mk` from a sparse checkout of the packages feed's `lang/rust`, at the revision in the SDK's `feeds.conf.default` (`url;branch` or `url^sha`); indexing the whole feed takes minutes and nothing in it is built. It comes before `feeds update`, which indexes the steward feed by reading the Rust packages' Makefiles, and they include it;
3. asks the SDK for the target triple: `make -s -C package/feeds/steward/steward-agent TOPDIR=/builder val.RUSTC_TARGET_ARCH`;
4. installs rustup's stable toolchain for it. For a tier-3 target with no prebuilt std (mips, mipsel), it installs nightly with `rust-src` and sets `CARGO_UNSTABLE_BUILD_STD=std`;
5. puts that toolchain's own `bin/` first on PATH, not rustup's proxies, because `rust-values.mk` moves `CARGO_HOME`.

`rust-values.mk` still makes the SDK's gcc the linker, so the binaries link against the device's musl (`ld-musl-*.so.1`, `libgcc_s`) like any OpenWrt package. Buildroot users get `rust/host` as usual.

Build one architecture locally (roughly 15 minutes on Apple Silicon, emulated amd64):

    mkdir -p out && chmod 777 out
    docker run --rm --platform linux/amd64 -e STEWARD_ARCH=aarch64_cortex-a53 -v "$PWD:/feed:ro" -v "$PWD/out:/out" \
        openwrt/sdk:mediatek-filogic-openwrt-25.12 sh /feed/.github/scripts/sdk-build.sh /feed /out [package...]

The packages land in `out/<arch>/`. `STEWARD_ARCH` is optional: given, the build fails unless the SDK builds for that architecture (CI passes the matrix's). The moving `-openwrt-25.12` tags ship only `setup.sh`, which downloads that branch's current SDK on every run.

## The feed (`feed.yml`)

1. **plan** (`feed-plan.sh`) finds the gh-pages commit to publish onto: the newest one whose `Source: main@<sha>` is still an ancestor. A package whose `pkg_inputs` changed is built for every architecture; one that an architecture's repository lacks (a new architecture) is built for that architecture only. It prints the build matrix as `targets=` JSON, one entry per architecture with something to build, each with its own package list. It fails when a build would be older than the base's newest of that package (a release is a commit date, so that happens only when main was rewritten with older dates): apk wouldn't upgrade to it. A rewrite of main must keep its commit dates rising.
2. **build** runs one job per matrix entry, passing `STEWARD_ARCH` (sdk-build.sh fails if the SDK builds for another architecture, whose output publish would never pick up), and uploads the artifact `packages-<arch>` (`out/<arch>/`).
3. **publish** (`publish.sh`):
   - merges the artifacts into `apk/<arch>/` without replacing a published file. A new build of a file the base already has is dropped (`kept ... as published`). So is a new build of a file that a replaced gh-pages commit published (a re-run for the same commit, a rewritten main): the published file comes back from gh-pages' history (`restored`);
   - keeps the last 5 builds of each package;
   - drops architectures no longer in `TARGETS`;
   - re-signs only the repositories that changed (`APK_SIGN_KEY`, `apk mkndx` in the SDK image);
   - copies `feed/steward.pem` and `feed/index.html`;
   - commits with `Source: main@<sha>` and pushes with `--force-with-lease` against the tip the plan saw.

   It runs for every feed run on main (each push, named after the pushed head), also when nothing was built. A push of several commits gets one gh-pages commit, and a run still queued when a newer one queues is cancelled.

When main is rewritten, gh-pages goes back to the last commit both histories share.

`rebuild` (workflow_dispatch, `REBUILD=true`) builds every package for every architecture: a test of the whole build. The published files stay as they are.

## Adding

- **A package**: create a top-level directory with a Makefile that includes `steward.mk`, add it to `PKGS`, add a `pkg_inputs` case if it's built from more than its own directory and the defaults, and add it to the README.
- **An architecture**: add `<arch>:<sdk>` to `TARGETS`. `<arch>` must equal the SDK's `CONFIG_TARGET_ARCH_PACKAGES`: sdk-build.sh names the output directory after it (and fails the build when they differ), and the device's `/etc/apk/arch` must match it. The plan then builds every package for that architecture alone.
- **A Rust dependency**: check it builds for mipsel. There are no 64-bit atomics, std is built on nightly, and C code is compiled by the SDK's gcc through `TARGET_CC`. Run the local mipsel build (`ramips-mt7621`) before pushing.

## Checking and debugging

- The feed scripts: `sh .github/tests/feed-scripts.sh` (docker, about 15 seconds; CI runs it in test.yml). It runs `feed-plan.sh` and `publish.sh` against a throwaway origin in an alpine container, with stand-in packages made by `apk mkpkg` and a stand-in for docker that signs the indexes with a throwaway key. It covers what each kind of change rebuilds and for which architectures, that a published file keeps its bytes, retention, signing, and the history cases (a re-run, a rewritten main, unrelated histories, a gh-pages that moved, a pull request, a rebuild). Run it after changing either script, `packages.sh` or how `steward.mk` numbers releases, and add a case for a new behaviour.
- What's published: `gh api 'repos/fonix232/steward/git/trees/gh-pages?recursive=1' --jq '.tree[].path'`.
- A failed job: `gh run view <id> -R fonix232/steward --log-failed`. A job's log with colour codes needs `gh api --allow-escape-sequences repos/fonix232/steward/actions/jobs/<job>/logs`.
- `publish: ... stale info` (the lease failed): gh-pages moved since the plan. Rerun the workflow.
- `sdk-build: no packages were built`: a package failed to compile. The log above it has `make ... V=s` output.
- `sdk-build: this SDK builds for <x>, not <y>`: the `TARGETS` entry's architecture isn't its SDK's `CONFIG_TARGET_ARCH_PACKAGES`.
- The packages on a device: `.ai/skills/device-testing`.
