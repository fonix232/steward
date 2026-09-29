---
id: 28
title: PoE control
type: feature
priority: P1
category: switching
effort: M
roles: [switch]
components: [steward-agent, steward-controller, steward-render, steward-proto]
packages: [realtek-poe]
unifi: PoE, power cycle
created: 2026-09-29
---

PoE budget and per-port power, and power-cycling a port from the interface.

## Acceptance criteria

- [x] `ethernet[].poe.admin-mode` turns power on or off for the selected ports (`LAN*`, `LAN2`, …, as the networks select them). It's set as `enable` on realtek-poe's own port sections, with the original value recorded as a radio option's is. That generalizes to any config's options (`Plan::device_options`, `originals.json`). The change is applied with the rest of the configuration, under the same rollback, and realtek-poe reloads it.
- [x] Refused with a reason: PoE on a device without realtek-poe, and a selected port it doesn't power. A port given contradictory modes is refused too. So are `ethernet`'s other settings (`speed`, `duplex`, `enabled`, `services`), which aren't supported yet and used to be ignored silently.
- [x] The state document carries PoE from `poe info`: the budget and consumption in `unit.poe`, and each powered port's status, mode, priority and consumption in its `link-state` entry.
- [x] `powercycle` (uCentral's command) turns the named ports off for their `cycle` (default 5 s, up to 60 s) and on again. The device answers at once and cycles in the background. Ports are named as the board names them (`lan3`) or as uCentral selects them (`LAN3`). It's refused, with error 2 and the reason, for:
  - a device without PoE;
  - an unknown port;
  - a port whose power is disabled in the configuration (realtek-poe can't cycle it).
- [x] The controller sends it: `steward-controller powercycle <serial> <port>[:<ms>]…` on the control socket, and `POST /api/devices/<serial>/powercycle` with `{ports: [{name, cycle}]}`. Both wait for the device's answer (up to 15 s) through a waiter per command id.
- [x] Unit tests: the renderer against realtek-poe's default port sections, the state document from a recorded `poe info`, the power-cycle plan, and the controller's command waiter. On bifrost, which has no PoE, a loopback controller and agent carry a `powercycle` end to end, and the device refuses it honestly. PoE hardware isn't available for a live check.

## Tasks

- [x] `Plan::device_options` and `originals.json` in place of radio options
- [x] `crates/render/src/poe.rs`: `Poe::from_uci`, `ethernet` → port `enable`, refusals
- [x] Agent: stage `poe`, PoE in the state document, `powercycle`
- [x] Controller: command waiters, `powercycle` on the control socket and the API
- [x] Tests, the loopback check on bifrost, docs

## Progress

Done:
- `crates/render/src/poe.rs`: `Poe::from_uci` reads realtek-poe's `port` sections (in id order). `ethernet[].poe.admin-mode` sets `enable` on the selected ports: `LANn`, `LAN*`, `*` (new in `Ports::select`, so VLANs take it too); a wildcard skips ports without PoE. It refuses:
  - PoE without realtek-poe;
  - a named port it doesn't power;
  - contradictory modes (the first stands);
  - `ethernet`'s `speed`, `duplex`, `enabled` and `services`, which used to be ignored silently.
- The device's own sections are generalized from radios: `Plan::device_options` holds (config, section, option), and the agent records originals in `originals.json` (`config.section.option`, formerly `radio-originals.json`). STW-59 restores from it.
- Agent:
  - it stages `poe` with `network` and `wireless` when realtek-poe is there, and `render-check` does the same;
  - the state document takes `poe info`: `unit.poe` (budget, consumption) and each powered port's `link-state` entry;
  - `powercycle` (`steward-agent/src/poe.rs`): checked against realtek-poe's config, since `poe manage` answers OK for a port it won't touch. It answers 0 at once and cycles each port in its own task (5 s by default, up to 60 s), or answers 2 with the reason.
- Controller:
  - `proto::Powercycle`;
  - `Device::waiting`, a waiter per command id that the response handler completes;
  - `Hub::powercycle`, which waits up to 15 s;
  - `steward-controller powercycle <serial> <port>[:<ms>]…`;
  - `POST /api/devices/<serial>/powercycle`.
- Docs: the ucentral skill (PoE, the state addition, the agent's commands), device-testing, instructions, README.

Verified:
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. The new tests cover:
  - the renderer (4): realtek-poe's default sections, admin mode on and off, wildcards, refusals;
  - the state document with a recorded `poe info`;
  - the power-cycle plan (both namings, the default, refusals);
  - originals across configs;
  - the hub's waiter (not found, pending, offline, a foreign answer ignored, the answer delivered);
  - the API route (sign-in, bad bodies, pending, unknown).
- On bifrost, a loopback controller (`--adopt-local`) and agent from `/tmp`:
  - `powercycle <serial> lan1:3000 LAN2` went to the agent and came back as "refused: this device has no PoE controller (realtek-poe)", and the CLI exited 1;
  - an unknown serial was refused by the controller;
  - the state document had link-state and no `poe`;
  - the configs were unchanged, with nothing in `uci changes`, and everything was stopped and removed.

Not done:
- Nothing ran on PoE hardware: there's no realtek-poe switch here. The formats come from realtek-poe's source (`main.c`: `load_port_config`, `ubus_poe_info_cb`, `ubus_poe_manage_cb`) and its default config.
- If the agent stops in the middle of a cycle, the port stays off until realtek-poe reloads its config or the switch restarts.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: `admin-mode` is a boolean (a string "false" powered the port); port ids read as realtek-poe reads them (`strtoul`, 1 to 48); a power cycle with realtek-poe stopped answered 2; radios' originals moved into `originals.json`; `ethernet` entries reject every key but `select-ports` and `poe`.
- Second: `select-ports` items that aren't strings refused; the controller's timeout answer says the device may still cycle the ports when it gets to the command.
- Third: verified. On bifrost (no PoE): the refusals, the power cycle refused end to end through the CLI and the API, the timeout and the late answer.
