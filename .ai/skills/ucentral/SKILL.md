---
name: ucentral
description: uCentral (TIP OpenLAN), the protocol and configuration schema Steward's agent and controller speak. Covers where TIP's specs live, message shapes, configure answers, serials and uuids, capabilities, and what TIP's renderer does and why Steward doesn't run it. Use when adding or changing a message, a command, the state or capabilities document, or the configuration renderer.
---

# uCentral

## Sources (read these, not TIP's website)

- Protocol: `PROTOCOL.md` in `Telecominfraproject/wlan-cloud-ucentralgw`: `gh api repos/Telecominfraproject/wlan-cloud-ucentralgw/contents/PROTOCOL.md --jq .content | base64 -d`.
- Configuration schema: `Telecominfraproject/wlan-ucentral-schema`:
  - `schema/*.yml`, merged into `ucentral.schema.json`
  - the state document: `state/*.yml`, `ucentral.state.pretty.json`
  - example configurations: `feeds/ucentral/ucentral-schema/files/etc/ucentral/examples/` in `Telecominfraproject/wlan-ap`
- TIP's renderer (`renderer/templates/`), for how schema fields map to UCI: radios and SSIDs especially.

## Messages

JSON-RPC 2.0 over a WebSocket the device opens to the controller, on port 15002. `crates/proto` types them.

- **Device → controller events**, notifications with no `id`: `connect` (serial, uuid, firmware, capabilities), `state`, `healthcheck`, `log`, `crashlog`, `event`, `cfgpending`, `ping`, `recovery`.
- **Controller → device commands**, requests with an `id`: `configure`, `reboot`, `upgrade`, `factory`, `leds`, `trace`, `wifiscan`, `request`, `ping`, `powercycle`, and the rest in PROTOCOL.md. The device answers every one with `{"result": {"serial", "uuid"?, "status": {"error", "text", "when"?, "rejected"?}}}`.
- **`configure` answers**:
  - `0`: applied as sent.
  - `1`: applied with substitutions, each listed in `rejected` as `{parameter, reason, substitution?}`.
  - `2`: not applied.

  Never answer 0 for a configuration that was changed or only partly applied.
- **`serial`**: the device's label MAC, lower case, no separators. It's private data, so redact it in logs.
- **`uuid`**: the configuration's number (a u64); 0 means none from a controller yet. The controller re-sends its stored configuration when a device reports another uuid.
- **Compressed commands**: when the capabilities say `compress_cmd: true`, the controller may send `params` as `{compress_64, compress_sz}` (zlib, base64). Steward's agent doesn't advertise it.

## Capabilities

TIP builds them in `system/capabilities.uc` from `board.json`, nl80211 and `/etc/ucentral/*`: `compatible`, `model`, `platform` (`ap`/`switch`), `network` (ports per role), `wifi` (per phy path: bands, channels, htmode, antennas), `macaddr` and `country_codes`. Steward's agent sends a subset for now (`steward-agent/src/device.rs`).

## TIP's renderer: what Spike A found (bifrost, SNAPSHOT)

- **It doesn't run on current OpenWrt unchanged.** `wifi/phy.uc` (twice) and `libs/wiphy.uc` `path_to_section` assume radios addressed by `path=` and phys named `phyN`. Current OpenWrt uses `phy=` (named in `board.json`, for example `wl0`), and three patches made it render.
- **It takes over the device.** Its UCI batch is additive onto a pristine baseline (`/etc/config-shadow`) and replaces `/etc/config` wholesale. Each service TIP has a template for (uhttpd, rpcd, dropbear, lldpd, umdns and the rest) is stopped unless the config enables it (`services_state()`, which `ucentral.uc` acts on); for `default.json`, where no interface lists `http`, that includes uhttpd and rpcd.
- **It depends on TIP-only services**, missing from stock feeds: `ucentral-state`, `event`, `udevstats`, `spotfilter`. The other daemons it configures, `bridger`, `ratelimit` and `dhcpsnoop`, are in the stock feeds (`bridger` in the base feed, `ratelimit` and `udhcpsnoop` in packages).
- **Rendering has side effects**: `unit.uc` sets the root password (`passwd root`, reported to the gateway over ubus) when the config carries `unit.system-password` or `unit.random-password`. Templates run shell commands.
- **There is no rollback.**

So Steward renders the schema itself, into UCI sections the agent owns and marks (TIP marks its own with `ucentral_path`), and applies them with rpcd's rollback (`.ai/instructions.md` § Design rules).

## TLS and trust

TIP's devices trust the gateway CA they were provisioned with (`/etc/ucentral/operational.ca`, TIP's PKI), or any self-signed certificate when `allow-self-signed` is set. Steward's controller is its own CA instead (`crates/tls`):
- **Controller:** on first start it creates the CA and a server certificate in `<state dir>/tls/` (`ca.pem`, `ca.key`, `server.pem`, `server.key`; the keys 0600) and serves both on every handshake.
- **Agent:** it trusts the first controller it reaches and pins its CA in `<state dir>/controller-ca.pem`. From then on, only a server certificate that chains to that CA is accepted. The host name isn't checked, since the pin identifies the controller. Only a missing pin file means first use: one that can't be read or holds no certificate stops the agent, with an error naming the file, rather than trusting the next controller.
- **Moving a device** to another controller means removing its pin; adoption (STW-10) will manage this.
- **Validity** runs 2020–2099, because devices without an RTC boot with their image's build date.
