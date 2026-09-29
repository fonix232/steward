---
name: package-feed
description: How Steward's apk packages are built and published. Covers packages.sh (packages, architectures, pkg_inputs), the SDK build on a rustup toolchain, and the per-architecture feed on gh-pages. Use when adding a package or an architecture, changing a Makefile or a build script, building packages locally, or debugging a failed feed run.
---

# Packages and the feed

## What gets built

`.github/scripts/packages.sh` is the single list:
- `PKGS`: the packages, one top-level directory each.
- `TARGETS`: `<arch>:<sdk>` pairs. Each architecture is built by `openwrt/sdk:<sdk>-openwrt-25.12` and published as its own apk repository, `apk/<arch>/`.
- `pkg_inputs <pkg>`: the paths a package is built from. A change to any of them rebuilds it, and its release number is the date of the last main commit that touched them (`steward.mk`).

Every Makefile includes `steward.mk` after `rules.mk`, with `PKG_NAME` set. That gives `PKG_VERSION` (from the cargo workspace), `PKG_RELEASE`, the license, and for the Rust packages `Steward/Prepare/Cargo`, which copies the workspace into the build directory. The architecture-independent packages (`steward`, `steward-web`) are built into every architecture's repository, so a device needs only its own.

## The SDK build (`.github/scripts/sdk-build.sh`)

`rust/host` from the packages feed builds rustc and LLVM from source, which takes hours per SDK. So CI:
1. exports `STEWARD_RUSTUP=1`, which drops the `rust/host` build dependency (`steward.mk`);
2. takes `rust-package.mk` and `rust-values.mk` from a sparse checkout of the packages feed's `lang/rust`, at the revision in the SDK's `feeds.conf.default` (`url;branch` or `url^sha`); indexing the whole feed takes minutes and nothing in it is built;
3. asks the SDK for the target triple: `make -s -C package/feeds/steward/steward-agent TOPDIR=/builder val.RUSTC_TARGET_ARCH`;
4. installs rustup's stable toolchain for it. For a tier-3 target with no prebuilt std (mips, mipsel), it installs nightly with `rust-src` and sets `CARGO_UNSTABLE_BUILD_STD=std`;
5. puts that toolchain's own `bin/` first on PATH, not rustup's proxies, because `rust-values.mk` moves `CARGO_HOME`.

`rust-values.mk` still makes the SDK's gcc the linker, so the binaries link against the device's musl (`ld-musl-*.so.1`, `libgcc_s`) like any OpenWrt package. Buildroot users get `rust/host` as usual.

Build one architecture locally (roughly 15 minutes on Apple Silicon, emulated amd64):

    mkdir -p out && chmod 777 out
    docker run --rm --platform linux/amd64 -v "$PWD:/feed:ro" -v "$PWD/out:/out" \
        openwrt/sdk:mediatek-filogic-openwrt-25.12 sh /feed/.github/scripts/sdk-build.sh /feed /out [package...]

The packages land in `out/<arch>/`. The moving `-openwrt-25.12` tags ship only `setup.sh`, which downloads that branch's current SDK on every run.

## The feed (`feed.yml`)

1. **plan** (`feed-plan.sh`) finds the gh-pages commit to publish onto: the newest one whose `Source: main@<sha>` is still an ancestor. It lists the packages whose `pkg_inputs` (or `sdk-build.sh`) changed, or that some architecture's repository lacks, and prints the build matrix as `targets=` JSON.
2. **build** runs one job per architecture and uploads the artifact `packages-<arch>` (`out/<arch>/`).
3. **publish** (`publish.sh`):
   - merges the artifacts into `apk/<arch>/` and keeps the last 5 builds of each package;
   - drops architectures no longer in `TARGETS`;
   - re-signs only the repositories that changed (`APK_SIGN_KEY`, `apk mkndx` in the SDK image);
   - copies `feed/steward.pem` and `feed/index.html`;
   - commits with `Source: main@<sha>` and pushes with `--force-with-lease` against the tip the plan saw.

   It runs for every main commit, also when nothing was built.

When main is rewritten, gh-pages goes back to the last commit both histories share.

## Adding

- **A package**: create a top-level directory with a Makefile that includes `steward.mk`, add it to `PKGS`, add a `pkg_inputs` case if it's built from more than its own directory and the defaults, and add it to the README.
- **An architecture**: add `<arch>:<sdk>` to `TARGETS`. `<arch>` must equal the SDK's `CONFIG_TARGET_ARCH_PACKAGES`: sdk-build.sh names the output directory after it, and the device's `/etc/apk/arch` must match it. The plan then builds every package for it.
- **A Rust dependency**: check it builds for mipsel. There are no 64-bit atomics, std is built on nightly, and C code is compiled by the SDK's gcc through `TARGET_CC`. Run the local mipsel build (`ramips-mt7621`) before pushing.

## Checking and debugging

- What's published: `gh api 'repos/fonix232/steward/git/trees/gh-pages?recursive=1' --jq '.tree[].path'`.
- A failed job: `gh run view <id> -R fonix232/steward --log-failed`. A job's log with colour codes needs `gh api --allow-escape-sequences repos/fonix232/steward/actions/jobs/<job>/logs`.
- `publish: ... stale info` (the lease failed): gh-pages moved since the plan. Rerun the workflow.
- `sdk-build: no packages were built`: a package failed to compile. The log above it has `make ... V=s` output.
- The packages on a device: `.ai/skills/device-testing`.
