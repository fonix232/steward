# Steward for OpenWrt

A free and open-source network controller for OpenWrt, aiming at the experience UniFi gives: install it on the router, and the access points, switches and other OpenWrt devices on the network are adopted and managed from one web interface. Topology, per-device and per-client traffic, and IDS/IPS are part of the plan.

**Status: early.** The agent connects to the controller over plain WebSocket (`ws://`), and reports what the device is and its state (load, memory, uptime). With no controller given, it looks for one on its default gateway. The controller sends a device its stored configuration (`/etc/steward/configs/<serial>.json`), and the agent refuses it: applying configurations isn't implemented yet. TLS, adoption, the configuration renderer and the web interface come next.

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
