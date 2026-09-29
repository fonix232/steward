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

So Steward renders the schema itself (`crates/render`), into UCI sections the agent owns and marks (TIP marks its own with `ucentral_path`), and applies them with rpcd's rollback (`.ai/instructions.md` § Design rules).

## What the renderer covers

- **Radios**, matched by band (`2G`/`5G`/`6G`): channel, htmode, country, txpower (0 to 30 dBm, the schema's range), and disabled from `enable`.
  - htmode comes from `channel-mode` + `channel-width`. VHT on 2.4 GHz becomes HT.
  - A 2.4 GHz radio without a width gets 20 MHz, because the schema's default of 80 doesn't exist there.
  - Checked against the band: channels 1–14 on 2.4 GHz (up to 40 MHz); 32–144 and 149–177 in steps of 4 on 5 GHz (up to 160 MHz); 1–233 in steps of 4 on 6 GHz (up to 320 MHz, HE or EHT only).
  - Checked against the radio: `Radio::htmodes`, from `iwinfo info` on its phy (`network.wireless status` → `<radio>.config.phy`, or one of its interfaces), filled in by `Wireless::read_htmodes`. The schema's default mode is HE, and HE on a radio without it would turn on `ieee80211ax` and keep the radio down (bifrost's 2.4 GHz radio runs HT20/HT40 only). A mode the radio lacks is replaced by the best it runs, the same width in a lower mode (EHT, HE, VHT, HT) first, then narrower widths, and listed with its `substitution` (answer 1). With nothing that fits, htmode is left as it is and the request rejected. Unknown capabilities aren't checked.
  - A band given twice: the first entry is applied, the others are refused.
- **SSIDs:** one `wifi-iface` per SSID per band, `stw_<interface>_<ssid>_<band>`. A band listed twice in `wifi-bands` is one section.
  - Ownership: a `wifi-iface` is the agent's only when it's named `stw_*` and marked `steward '1'`. Only those are deleted when the configuration no longer has them: a section of the user's that carries the marker isn't one.
  - Encryption: none, owe, psk, psk2, psk-mixed, sae, sae-mixed. Keys are 8 to 63 printable characters (hostapd reads its config a line at a time, so a newline would add a line) or 64 hex digits; SAE modes take only the former, because the scripts write a 64-hex key as `wpa_psk` and leave SAE without a password. No `encryption` is an open network; an `encryption` without a `proto` refuses the SSID rather than opening it. A `key` on a mode that takes none (none, owe) is rejected, redacted, and the SSID runs as its proto says: dropped quietly, it would answer 0 for an SSID its operator believes is keyed.
  - MFP: `ieee80211w` is `disabled` (the default), `optional` or `required`, TIP's values; anything else refuses the SSID rather than running it with less protection than asked. sae and owe run with it required, sae-mixed at least optional (its SAE clients need it), none and psk (WPA1) without it (the scripts write `ieee80211w=0` there); an `ieee80211w` asked otherwise is a substitution, so the answer says what runs.
  - Network: the device's `lan`, until STW-13. So an interface asking for a VLAN is refused with its SSIDs (they'd run on the `lan`), and an interface that isn't an object, a `role` other than `upstream` or `downstream`, and any other interface key are rejected.
- **Handled keys** (anything else is rejected as `<key> isn't supported yet`, so an ignored field never gets answer 0):
  - top level: `uuid`, `radios`, `interfaces`;
  - interface: `name`, `role`, `vlan` (refused with its SSIDs), `ssids`;
  - radio: `band`, `channel`, `channel-mode`, `channel-width`, `country`, `tx-power`, `enable`;
  - SSID: `name`, `wifi-bands`, `bss-mode`, `encryption`, `hidden-ssid`, `isolate-clients`;
  - encryption: `proto`, `key`, `ieee80211w`.

  A value of the wrong kind (`"20"` for a tx power, `"yes"` for a boolean, an SSID without bands) is rejected, never guessed at.
- **Rejected** (the answer's `rejected` list):
  - bands the device lacks; `5G-lower`/`5G-upper`, `HaLow`;
  - channels, widths and modes the band or the radio doesn't have (above);
  - mesh and WDS modes;
  - enterprise and RADIUS modes;
  - multi-PSK, captive portals, pass-point, rate limits and ACLs;
  - raw hostapd lines.

  A rejection shows what was sent, redacted: every value under a key naming a key, password, passphrase or secret, and raw lines (`*-raw`), become `…`, however deep.
- `tests/wireless-schema.json` lists the options the wifi scripts read (from `/usr/share/schema/wireless.*.json`). A test checks every written option against it.

## TLS and trust

TIP's devices trust the gateway CA they were provisioned with (`/etc/ucentral/operational.ca`, TIP's PKI), or any self-signed certificate when `allow-self-signed` is set. Steward's controller is its own CA instead (`crates/tls`):
- **Controller:** on first start it creates the CA and a server certificate in `<state dir>/tls/` (`ca.pem`, `ca.key`, `server.pem`, `server.key`; the keys 0600) and serves both on every handshake.
- **Agent:** it trusts the first controller it reaches and pins its CA in `<state dir>/controller-ca.pem`. From then on, only a server certificate that chains to that CA is accepted. The host name isn't checked, since the pin identifies the controller. Only a missing pin file means first use: one that can't be read or holds no certificate stops the agent, with an error naming the file, rather than trusting the next controller.
- **Moving a device** to another controller means removing its pin (`/etc/steward-agent/controller-ca.pem`) and credential (`/etc/steward-agent/credential`). With a credential and no pin, the agent doesn't connect: the credential is only for the controller the lost pin named, and without the pin it would go to whichever controller the agent trusted next.

## Adoption (Steward's, not uCentral's)

uCentral has no adoption step: TIP's devices come with certificates. Steward adds one:
- **Pending:** a device's first `connect` makes it pending. It stays connected for the controller's bookkeeping and is sent nothing. Pending devices are kept in memory only (anything that connects is one): at most 64, the oldest one that isn't connected dropped first, or when all are connected the oldest, disconnected with its record (so no more stay connected, and it's pending again when it reconnects), a controller restart forgets them until they connect again, and their model and firmware are cut to 128 bytes (a longer serial is refused). Only adopting and adopted devices are written to `devices.json`.
- **Adopting:** `steward-controller adopt <serial>` (the control socket, `/var/run/steward-controller.sock`) marks the device adopting. When it's connected, the controller sends `steward.adopt` with `{serial, credential}` (32 random bytes, hex). Once the agent answers error 0, the device is adopted.
- **Adopted:** every later WebSocket upgrade must carry `Authorization: Bearer <credential>`. A missing or wrong one closes the connection with code 1008. The controller stores only the SHA-256, in `/etc/steward/devices.json` (0600).
- **Forgotten:** `steward-controller forget <serial>` drops the record, so the credential stops working and the device starts over as pending.
- **Validity** runs 2020–2099, because devices without an RTC boot with their image's build date.
