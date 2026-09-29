# Steward for OpenWrt

A free and open-source network controller for OpenWrt, aiming at the experience UniFi gives: install it on the router, and the access points, switches and other OpenWrt devices on the network are adopted and managed from one web interface. Topology, per-device and per-client traffic, and IDS/IPS are part of the plan.

**Status: starting.** The design, the plan (`.ai/plans/roadmap.md`) and the task board (`.ai/kanban/`) are in place. So far there's the cargo workspace, uCentral's message types (`crates/proto`), and a ubus client in Rust (`crates/ubus`) whose answers match the `ubus` command's on a device, including UCI changes through rpcd with its rollback: a change that isn't confirmed reverts by itself. The agent, the controller and the packages are being brought up. The target is OpenWrt 25.12 and later, with apk packages.

## How it fits together (the design)

- **steward-controller** runs on the router (or on any OpenWrt host the devices can reach). It keeps each device's configuration and collects state, clients, traffic and topology.
- **steward-agent** runs on every managed device, the router included. It connects to the controller, turns what the controller sends into UCI and ubus calls, and reports the device's state back. A configuration is applied with a rollback: if the device loses the controller after applying it, it returns to the previous one.
- **steward-web** will be the controller's web interface, served by the controller.
- **steward** installs all three, for the device that hosts the controller.

Agent and controller speak uCentral's protocol (the Telecom Infra Project's OpenLAN): JSON-RPC 2.0 over a WebSocket the device opens to the controller on port 15002. Both are written in Rust; `crates/proto` holds the messages they share, and `crates/ubus` is the agent's way onto the device's ubus (rpcd's `uci` object included).

## Building

Tests: `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.

## License

MIT. See `LICENSE`.
