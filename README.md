# Steward for OpenWrt

A free and open-source network controller for OpenWrt, aiming at the experience UniFi gives: install it on the router, and the access points, switches and other OpenWrt devices on the network are adopted and managed from one web interface. Topology, per-device and per-client traffic, and IDS/IPS are part of the plan.

**Status: early.** The agent connects to the controller over TLS (`wss://`), pinning the controller's certificate authority the first time it connects, and reports what the device is and its state (load, memory, uptime). With no controller given, it looks for one on its default gateway. A new device waits as pending until it's adopted (`steward-controller adopt <serial>`); adoption gives it a credential it presents from then on. The router's own agent is adopted by itself. The controller sends an adopted device its stored configuration (`/etc/steward/configs/<serial>.json`), and the agent refuses it: applying configurations isn't implemented yet. The configuration renderer and the web interface come next.

## How it fits together (the design)

- **steward-controller** runs on the router (or on any OpenWrt host the devices can reach). It keeps each device's configuration and collects state, clients, traffic and topology.
- **steward-agent** runs on every managed device, the router included. It connects to the controller, turns what the controller sends into UCI and ubus calls, and reports the device's state back. A configuration is applied with a rollback: if the device loses the controller after applying it, it returns to the previous one.
- **steward-web** will be the controller's web interface, served by the controller.
- **steward** installs all three, for the device that hosts the controller.

Agent and controller speak uCentral's protocol (the Telecom Infra Project's OpenLAN): JSON-RPC 2.0 over a WebSocket the device opens to the controller on port 15002, over TLS: the controller is its own certificate authority, and each agent pins it the first time it connects. Both are written in Rust; `crates/proto` holds the messages they share, and `crates/ubus` is the agent's way onto the device's ubus (rpcd's `uci` object included).

## Install (OpenWrt 25.12 and later)

The packages are apk only. On the device that hosts the controller:

    wget -O /etc/apk/keys/steward.pem https://fonix232.github.io/steward/steward.pem
    apk add -X https://fonix232.github.io/steward/apk/$(cat /etc/apk/arch)/packages.adb steward

The router then runs the controller, and manages itself through its own agent, which the controller adopts automatically. On every other device, install `steward-agent` instead: it finds the controller on its default gateway and waits as pending until you adopt it (`steward-controller devices`, then `steward-controller adopt <serial>` on the router). The agent adds the feed to the device, so later versions arrive with `apk upgrade`.

The feed has one repository per package architecture:

| Architecture | Built with the SDK of | Covers, for example |
|---|---|---|
| `aarch64_cortex-a53` | mediatek/filogic | MediaTek MT7622/MT798x, Qualcomm IPQ807x |
| `arm_cortex-a7_neon-vfpv4` | ipq40xx/generic | Qualcomm IPQ40xx |
| `mipsel_24kc` | ramips/mt7621 | MediaTek MT7621 |
| `x86_64` | x86/64 | PCs, virtual machines |

## Repository layout

    Cargo.toml            the cargo workspace; its version is every package's
    crates/proto/         uCentral's messages, shared by agent and controller
    crates/tls/           the device channel's TLS: the controller's CA, the agent's pin
    crates/ubus/          a ubus client in Rust (no libubus), for the agent
    steward-agent/        package: Makefile, Rust crate, files/ (init, UCI config)
    steward-controller/   package: Makefile, Rust crate, files/
    steward-web/          package: the web interface (www/)
    steward/              package: the collection
    steward.mk            what the packages' Makefiles share (version, release, cargo)
    feed/                 the feed's signing key (public half) and its GitHub Pages page
    .github/scripts/      the feed's build and publish scripts (packages.sh lists packages and architectures)
    .github/tests/        the feed scripts' test, against a throwaway origin
    .ai/                  AI tooling: instructions (instructions.md), skills, agents, plans, and the task board (kanban/)

## Building

Tests: `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`, and for the feed's scripts `sh .github/tests/feed-scripts.sh` (needs docker).

Packages: the Makefiles are ordinary OpenWrt package Makefiles, built with `lang/rust`'s `rust-package.mk` from the packages feed. Add the repository as a feed in a 25.12 SDK or buildroot (`src-link steward /path/to/steward`) and build `package/steward-agent/compile` and the rest; `rust/host` is built first.

The feed's CI builds each architecture in the official SDK image (`openwrt/sdk:<target>-openwrt-25.12`) with `.github/scripts/sdk-build.sh`. It uses rustup's prebuilt toolchain instead of `rust/host`, which builds rustc and LLVM from source, and keeps everything else of `rust-package.mk`: the target triple, and the SDK's gcc as the linker, so the binaries link against the device's musl. For MIPS, which rustup ships no standard library for, it builds the standard library on nightly.

To build one architecture locally:

    mkdir -p out && chmod 777 out
    docker run --rm -v "$PWD:/feed:ro" -v "$PWD/out:/out" openwrt/sdk:mediatek-filogic-openwrt-25.12 \
        sh /feed/.github/scripts/sdk-build.sh /feed /out

## The feed

`.github/workflows/feed.yml` builds only the packages whose inputs changed (`.github/scripts/feed-plan.sh`), for every architecture, and publishes them on GitHub Pages as signed apk repositories. The `gh-pages` branch mirrors `main`: each of its commits is the whole feed after one push to `main`, named after the pushed head in a `Source: main@<sha>` line. A package's release number is the UTC date of the last `main` commit that changed what it is built from (its build script included), so it goes up as long as `main`'s commit dates do (the feed refuses to publish an older build), and a published version never changes: the feed never replaces a published file. There are no tags or releases; the feed keeps the last five builds of each package.

## License

MIT. See `LICENSE`.
