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

  Never answer 0 for a configuration that was changed or only partly applied. Steward's agent answers "nothing to change" (0 or 1) only while no apply is pending on the device, since a pending one may revert what the configuration matched (busy: retried, then 2). An apply it can't confirm is rolled back before it answers 2; should that fail, rpcd reverts it when its window ends, and until then it's the pending apply that keeps the next configuration waiting.
- **`serial`**: the device's label MAC, lower case, no separators. The controller closes a connection whose serial isn't 12 lower-case hex digits (1008). It's private data, so redact it in logs.
- **`uuid`**: the configuration's number (a u64); 0 means none from a controller yet. The controller re-sends its stored configuration when a device reports another uuid.
- **Compressed commands**: when the capabilities say `compress_cmd: true`, the controller may send `params` as `{compress_64, compress_sz}` (zlib, base64). Steward's agent doesn't advertise it.

## State

The agent's `state` event (every minute) follows TIP's `state/*.yml`. `steward-agent/src/state.rs` builds it: `gather()` reads the sources and `document()` is pure.
- **`unit`**: load, `cpu_load` (total, then per core, % since the last report, from `/proc/stat`), `localtime`, uptime, `boottime`, memory, and `temperature` [average, maximum] of the CPU's thermal zones.
  - `localtime` is unix time. OpenWrt's `system info` `localtime` is shifted by the timezone, so it isn't used.
- **`radios`**, one per `network.wireless status` radio: phy, band, channel, `channel_width`, `channels`/`frequency` (every 20 MHz channel spanned), `tx_power`, temperature, `chanUtil`.
  - Live values come from `iwinfo info` on one of the radio's interfaces. Its `htmode` is the operating one (a configured HT40 on 2.4 GHz may run at HT20); the `iwinfo` CLI prints the configured one.
  - `iwinfo survey` needs an interface too: it's empty for a phy name. `chanUtil` is busy over active time on the channel since the last report. The survey counts per channel, so the first report on a new channel (or after a counter reset) has none.
  - The temperature is the hwmon sensor whose `device` resolves to `.../ieee80211/<phy>`. The hwmon names use the driver's original phy numbers (`mt7915_phy1`), not the renamed phys.
- **`interfaces`**, one per `network.interface dump` entry but loopback: name, uptime, ipv4 addresses, ipv6_addresses, dns_servers, counters (its L3 device's), `ssids`.
  - Each SSID: bssid (its device's MAC), ssid, mode, band, phy, iface, frequency, `radio` (`#/radios/N`), counters, and `location` (`/interfaces/<i>/ssids/<s>`) for the agent's `stw_<i>_<s>_<band>` sections.
- **`link-state`**: board.json's WAN ports as upstream, LAN ports as downstream: carrier, speed and duplex (netifd's `1000F`), counters.
- Every field is picked by name: `network.wireless status` includes each SSID's key in its config.
- Missing sources leave their parts out.
- About 11 KB a report on bifrost (8 SSIDs, 5 ports, 10 stations; 6 KB without clients). The controller writes states to flash only on events (STW-14), so the size costs bandwidth, not flash.
- **Clients** (`steward-agent/src/clients.rs`):
  - Each SSID's `associations` come from `iwinfo assoclist`: station, rssi, connected (s), inactive (ms, as nl80211 gives it), bytes, packets, retries, and `rx_rate`/`tx_rate` (bitrate in kbit/s, mcs, nss, chwidth, sgi).
  - Each interface's `clients` (mac, addresses, ports) combine its SSIDs' stations, its IPv4 neighbours (`/proc/net/arp` on its L3 device), IPv6 from `luci-rpc getHostHints`, and the MACs the bridge learned on wired ports.
    - `getHostHints` keys its answer by upper-case MAC (every key on bifrost); the agent lowers them, as it does every MAC.
    - The bridge's MACs come from `/sys/class/net/<bridge>/brforward`: 16-byte `__fdb_entry` records, with port numbers mapped through `brif/*/port_no`. It has no VLANs, so MACs without an IP go to `lan`, or else to the first interface on the bridge.
  - Left out:
    - the device's own MACs;
    - everything learned on the uplink, the port the default gateway's MAC is learned on (on bifrost, lan4 carries about 43 MACs);
    - on a device with an uplink, neighbours that no port or SSID places. These are hosts the AP talks to elsewhere, such as the router, an SSH client or monitoring.
    - where the default route isn't on a bridge (a router's WAN), everything on its L3 device, for every interface on it (`wan`, `wan6`): the ISP's gateway, a modem. The interfaces that carry a default route are those with one in netifd's `route` (target `0.0.0.0` or `::`, mask 0). On a bridge (an AP's `lan`), the uplink port says what's upstream instead.
  - `ipv4.leases` (address, mac, hostname) are included where the device serves DHCP, placed by subnet. They come from `luci-rpc getDHCPLeases`, or else from `/tmp/dhcp.leases`.
  - `capabilities.macaddr` (`lan`, `wan`) lets the controller tell managed devices from clients.
- **The controller's client list** (`steward-controller/src/clients.rs`, `/api/clients`, `steward-controller clients`) has one entry per MAC.
  - Only connected devices' states count: an offline AP's stored state says who was on it, not who is. Every adopted device's own MACs and BSSIDs are still left out.
  - Wireless wins. While roaming, the association that was active last: its state's arrival less its `inactive`, because states arrive at different times.
  - A wired client goes where it was seen on the port with the fewest clients, i.e. the AP's port rather than the router's port towards the AP.
  - Names come from leases, which only the router has. A network of APs without an agent on the router has clients with no names and few IPs.
- LLDP peers aren't reported yet.
- **PoE**, a Steward addition to TIP's state, from realtek-poe's `poe info`: `unit.poe` has `budget` and `consumption` (W), and each powered port's `link-state` entry has `poe` (`status`, `mode`, `priority`, `consumption`). Without realtek-poe it's left out.

## Commands Steward's agent runs

- `configure` (above) and `steward.adopt` (below).
- `powercycle` (`{ports: [{name, cycle}]}`): PoE ports off for `cycle` ms (5000 by default, 1 to 60000), then on, through `poe manage`. Ports are named as the board names them (`lan3`) or as uCentral selects them (`LAN3`, `LAN*`). The agent checks them against realtek-poe's config first, because `poe manage` answers OK for a port it won't touch (one whose power the config turns off). It also looks up the `poe` object: with the config there but the daemon stopped, the cycle would fail only after the answer, so it's refused. It answers 0 at once and cycles in the background, or 2 with the reason, cycling nothing. On the controller: `steward-controller powercycle <serial> <port>[:<ms>]…` and `POST /api/devices/<serial>/powercycle`, which wait up to 15 s for the answer (`Device::waiting`, by command id).
  - The agent takes one command at a time, so a `powercycle` sent while it applies a configuration (up to minutes of retries and confirmation) waits its turn and still cycles the ports then, after the controller has stopped waiting. So the controller's timeout doesn't say the cycle failed: it says the device didn't answer command `<id>` in 15 s and may still cycle the ports when it gets to it, and the late answer goes to the controller's log (`<serial>: command <id>: …`).
- Anything else is answered 1, `<method>: not supported`.

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
  - Encryption (`crates/render/src/security.rs`): none, owe, psk, psk2, psk-mixed, sae, sae-mixed, and the enterprise modes wpa, wpa2, wpa-mixed, wpa3, wpa3-mixed, wpa3-192. UCI's `encryption` takes the same names. Keys are 8 to 63 printable characters or 64 hex digits; SAE modes take only the former, because the scripts write a 64-hex key as `wpa_psk` and leave SAE without a password. A `key` on a mode that takes none (none, owe, the enterprise modes) is refused rather than dropped, and the SSID runs as its `proto` says. No `encryption` is an open network; an `encryption` without a `proto` refuses the SSID rather than opening it.
  - MFP: required for sae, owe, wpa3 and wpa3-192; at least optional for sae-mixed and wpa3-mixed (their SAE and WPA3 clients need it); off for none, psk and wpa, which have none (a configured one is a substitution: the scripts would write `ieee80211w=0` and a SHA-256 AKM WPA1 clients don't know); as configured otherwise. A configured `ieee80211w` below what the protocol needs is a substitution as well, so the answer says what runs. `ieee80211w` is `disabled`, `optional` or `required`, and any other value refuses the SSID rather than run it with less protection than was asked for.
  - 6 GHz runs only WPA3 and OWE: sae-mixed becomes sae and wpa3-mixed becomes wpa3 there (a substitution in the answer, and so is an `ieee80211w` asked lower than the required it runs with), and any other mode is refused for that band.
  - RADIUS (`radius`): an enterprise SSID needs `authentication` (host, port, secret); `mac-filter` puts a server on an open, OWE, PSK or SAE SSID, which the wifi scripts turn into `macaddr_acl=2` (the client's MAC as the RADIUS user). `accounting` works on any SSID. An `authentication` server on such an SSID without `mac-filter` isn't used, so it's refused. `mac-filter` on an enterprise SSID refuses it: the scripts do MAC authentication only on open, OWE, PSK and SAE SSIDs, so it would be dropped. A `mac-filter` that isn't a boolean refuses the SSID, and so does a `radius` or `authentication` that isn't an object: read as off, it would run the SSID without MAC authentication.
    - Options: `auth_server`/`auth_port`/`auth_secret`, `acct_server`/`acct_port`/`acct_secret`/`acct_interval`, `nasid`, `request_cui` (chargeable-user-id), `dae_client`/`dae_port`/`dae_secret` (dynamic authorization), `radius_auth_req_attr`/`radius_acct_req_attr`, `eap_reauth_period`, and `auth_cache` (key caching: on unless the config says otherwise, where the scripts' default for EAP is off).
    - Bounds are the schema's: `eap-reauth-period` 0 to 86400 (hostapd reads it with atoi, so from 2^31 it turns negative, "invalid period", and the radio fails) and `interval` 60 to 600. `eap-reauth-period` and `key-caching` are EAP's: on any other SSID they're refused, unless they hold the schema's default (3600, true), which changes nothing. A value of the wrong kind (`"600"`, `"false"`, a `request-attribute` that isn't a list) is rejected and left to the default, and a dynamic authorization port only takes 3799 when it's missing.
    - Dynamic authorization only on enterprise SSIDs: the scripts add its secret to `radius_das_client` only in their EAP branch but write the line for every SSID, and hostapd refuses a client without a secret, which fails the whole radio. `nas-identifier`, `chargeable-user-id` and `dynamic-authorization` on an SSID with no RADIUS server are refused, not dropped.
    - hostapd takes a server's address, never a name. Stock hostapd has one port and secret per server list, so a `secondary` server is a second address when it shares them, and refused otherwise.
    - Request attributes are hostapd's `<id>:s:<text>`, `<id>:d:<number>` (32 bits), `<id>:x:<hex>`, and vendor attributes `26:x:<vendor, 8 hex digits><type><length><value>` (vendor 1 to 65535, the schema's bounds, with at least one attribute, each id 1 to 255). A value is 253 bytes at most, the vendor attribute's 4-byte id and sub-attributes included: hostapd loads a longer one, then fails every request it would go in, so nobody could authenticate.
    - RADIUS-assigned VLANs stay off (`dynamic_vlan` 0).
    - OpenWrt's generator writes an EAP network's accounting server twice. That's stock behaviour, the same for LuCI's networks.
  - Fast roaming and steering (`crates/render/src/roaming.rs`):
    - `roaming` (true or its object) → `ieee80211r`, `ft_over_ds` (`message-exchange`, air by default), `mobility_domain` (`domain-identifier`, 4 hex digits; otherwise the scripts derive one from the SSID, the same on every AP and band), `ft_psk_generate_local` (`generate-psk`, WPA2-PSK only), `r0kh`/`r1kh` from `pmk-r0/r1-key-holder` (a pair, checked as hostapd's `add_r0kh`/`add_r1kh` do: R0 `<MAC>,<R0KH-ID of 1 to 47 characters>,<key>`, R1 `<MAC>,<R1KH-ID, a MAC>,<key>`, keys 32 or 64 hex digits; TIP's own example R1KH-ID `14DD204714E4` isn't a MAC, and a holder hostapd refuses fails the radio) or both from `key-aes-256`. Key holders given beside `key-aes-256` are refused, and the shared key's stand. It needs WPA2 or later with a key or 802.1X.
    - For EAP the stock scripts can't derive the key holders' key: they read `auth_secret`, which validation has renamed to `auth_server_shared_secret`, and `FT_KEY_CANT_BE_DERIVED` fails the whole radio. So an enterprise SSID without key holders gets wildcard ones with a key derived from its name and RADIUS secret (SHA-256), the same on every AP. That includes one whose `key-aes-256` or key holders were refused.
    - `rrm` → `ieee80211k` (`neighbor-reporting`, which also turns on beacon reports), `rnr`, `ftm_responder`, `lci`, `civic` (the scripts write the last three only on a radio that can be an FTM responder). `stationary-ap` is a radio option, refused. `lci` and `civic-location` are 1 to 255 bytes as hex digits, a measurement subelement's most: hostapd reads its config in 4096-byte lines, so a longer value splits into an invalid line and fails the radio.
    - `services: ["wifi-steering"]` → `bss_transition` and `ieee80211k`, and usteer steers the SSID with its own settings (band steering is on by default in the current usteer, via BSS transition requests). The agent asks the running daemon (`usteer get_config`), not its UCI: a later `usteer` section replaces an earlier one's `ssid_list`, and no list means every SSID. Steward never edits usteer. When usteer isn't installed, isn't running, or lists only other SSIDs, steering is refused and the SSID keeps 802.11k/v. Every other service is refused, and so is a `services` that isn't a list of strings.
    - A value of the wrong kind (`"yes"` for `generate-psk` or `neighbor-reporting`, an `rrm` that isn't an object) is rejected and left to the default.
  - No string with a control character is written into an option hostapd reads: most are written unquoted, one per line, so a newline would add a line of its own.
  - Network: its interface's (below).
  - An owned section set again loses the options the configuration no longer gives it (`Op::Unset`), for example a network that stops using RADIUS.
- **Interfaces** are layer-2 networks (`crates/render/src/network.rs`):
  - An interface is an object; anything else is refused (read as one without fields, it would be the device's `lan`). `role` is TIP's `upstream` or `downstream`; both render alike, so another value is rejected on its own.
  - Without `vlan`: the device's own `lan`, untouched.
  - With `vlan.id`: a VLAN on the device's VLAN-filtering bridge (`br-lan` when there are several). A bridge filters when `vlan_filtering` is set or it has `bridge-vlan` sections.
    - The VLAN already exists (a `bridge-vlan` for the id): joined as it is, and so is an interface already on `<bridge>.<vid>`. Ports asked for that differ from the VLAN's are rejected, not applied, and so are ports the board lacks, as for a new VLAN.
    - Otherwise: an owned `bridge-vlan` `stw_bv<vid>`, and an owned `interface` `stw_vlan<vid>` (proto none) unless one exists.
    - Its ports come from `ethernet[].select-ports`: `LAN*`, `LANn`, `WAN*`, `WANn` against board.json's port roles. Without `ethernet`, it's tagged on every bridge port.
    - `vlan-tag` is TIP's `tagged`, `un-tagged` or `auto` (the default, taken as tagged); `untagged` is taken as `un-tagged`. Any other value is rejected with its entry's ports, never read as tagged.
    - An `ethernet` entry that isn't an object, a `select-ports` that isn't a list, and a port name that isn't a string are rejected; the rest of the selection applies.
    - An untagged port must be untagged in no other VLAN.
    - A port selected both tagged and un-tagged for one new VLAN keeps its first selection; the later one is rejected, naming the port.
  - Refused with its SSIDs: a `vlan` without an `id` (it isn't the untagged `lan` either), a `vlan.proto` other than `802.1q` (the default), and an `ethernet` that isn't a list (read as none, it would tag the VLAN on every port). `ethernet` on an interface without a VLAN is refused on its own: the device's `lan` keeps its ports, so it selects nothing, and the SSIDs stay on `lan`.
  - A refused interface's SSIDs are refused with it, never moved to another network.
  - Ownership: a network section is the agent's only when it's named `stw_*` and marked `steward '1'`. A user's VLAN or interface that carries the marker is joined like anyone's, and never deleted.
  - The agent stages `network` and `wireless` in one transaction, so one rollback covers both.
- **Handled keys** (anything else is rejected as `<key> isn't supported yet`, so an ignored field never gets answer 0):
  - top level: `uuid`, `radios`, `interfaces`, `ethernet`;
  - radio: `band`, `channel`, `channel-mode`, `channel-width`, `country`, `tx-power`, `enable`;
  - interface: `name`, `role`, `vlan` (`id`, `proto`), `ethernet` (`select-ports`, `vlan-tag`), `ssids`, `ipv4`;
  - `ethernet` entry: `select-ports`, `poe` (`admin-mode`);
  - SSID: `name`, `wifi-bands`, `bss-mode`, `encryption`, `hidden-ssid`, `isolate-clients`, `radius`, `certificates`, `roaming`, `rrm`, `services`;
  - `encryption`: `proto`, `key`, `ieee80211w`, `key-caching`, `eap-reauth-period`;
  - `radius`: `authentication` (`host`, `port`, `secret`, `secondary`, `request-attribute`, `mac-filter`), `accounting` (`host`, `port`, `secret`, `secondary`, `request-attribute`, `interval`), `dynamic-authorization` (`host`, `port`, `secret`), `nas-identifier`, `chargeable-user-id`, and `local` and `health`, refused on their own; a `secondary`: `host`, `port`, `secret`;
  - request attribute: `id` with `value` or with `hex-value`, or `vendor-id` with `vendor-attributes` (`id`, `value`), each form reading only its own keys;
  - `roaming`: `message-exchange`, `generate-psk`, `domain-identifier`, `pmk-r0-key-holder`, `pmk-r1-key-holder`, `key-aes-256`;
  - `rrm`: `neighbor-reporting`, `reduced-neighbor-reporting`, `ftm-responder`, `lci`, `civic-location`, and `stationary-ap`, refused on its own;
  - `services` entry: `wifi-steering`.

  A value of the wrong kind (`"20"` for a tx power, `"yes"` for a boolean, an SSID without bands) is rejected, never guessed at.
- **PoE** (`crates/render/src/poe.rs`): `ethernet[].poe.admin-mode` sets `enable` on realtek-poe's `port` sections for the ports `select-ports` names (`LAN2`, `LAN*`, `*`, by board.json's roles; a wildcard skips ports without PoE). They're the device's own sections, so each option is in `Plan::device_options` and its original kept in `originals.json` (an agent from before PoE kept radios' in `radio-originals.json`, keyed `<section>.<option>`: they move in as `wireless.<section>.<option>` on first use, and the old file goes). realtek-poe reloads on the config change (a procd trigger), and the rollback covers `poe` with `network` and `wireless`.
  - A port's `id` is read as realtek-poe reads it (`strtoul(id, NULL, 0)`: decimal, `0x` hex, a leading 0 octal, up to the first non-digit), and a port whose id isn't 1 to 48 is dropped, as realtek-poe drops it.
  - `admin-mode` is a boolean: only a missing one takes the schema's default (on), and anything else (`"false"`, `0`, `null`) is refused, as is a `poe` that isn't an object.
  - Refused as well: PoE without realtek-poe, a named port it doesn't power, a `select-ports` pattern that isn't a string (the entry's other patterns stand), a port given both modes (the first stands), an `ethernet` that isn't a list, and `ethernet`'s other keys (`speed`, `duplex`, `enabled`, `services`).
- **Rejected** (the answer's `rejected` list):
  - any `ipv4` that asks for something (all but `addressing: none` and `send-hostname: true`): the device keeps its own addressing, and anything else in it would be dropped; a bridge that doesn't filter VLANs; VLAN ids outside 1 to 4094, or used twice; ports the board or the bridge lacks, or already untagged elsewhere;
  - bands the device lacks; `5G-lower`/`5G-upper`, `HaLow`;
  - channels, widths and modes the band or the radio doesn't have (above);
  - mesh and WDS modes;
  - an enterprise SSID without a complete RADIUS server; a server given by name; `radius.local` and `certificates` (hostapd's built-in EAP server, STW-60); `radius.health`; `psk2-radius` and `mpsk-radius`, multi-PSK (STW-21), `owe-transition`;
  - captive portals, pass-point, rate limits and ACLs;
  - raw hostapd lines.

  A rejection shows what was sent, redacted: every value under a key naming a key, password, passphrase or secret, and raw lines (`*-raw`), become `…`, however deep. An object that holds secrets (`radius`, a RADIUS server, `certificates`) sent as anything else is shown as `…` whole.
- `tests/wireless-schema.json` lists the options the wifi scripts read (from `/usr/share/schema/wireless.*.json`), and `tests/network-schema.json` those netifd reads (its `vlan_attrs` and `iface_attrs`). Tests check every written option against them.

## TLS and trust

TIP's devices trust the gateway CA they were provisioned with (`/etc/ucentral/operational.ca`, TIP's PKI), or any self-signed certificate when `allow-self-signed` is set. Steward's controller is its own CA instead (`crates/tls`):
- **Controller:** on first start it creates the CA and a server certificate in `<state dir>/tls/` (`ca.pem`, `ca.key`, `server.pem`, `server.key`; the keys 0600) and serves both on every handshake.
- **Agent:** it trusts the first controller it reaches and pins its CA in `<state dir>/controller-ca.pem`. From then on, only a server certificate that chains to that CA is accepted. The host name isn't checked, since the pin identifies the controller. Only a missing pin file means first use: one that can't be read or holds no certificate stops the agent, with an error naming the file, rather than trusting the next controller.
- **Moving a device** to another controller means removing its pin (`/etc/steward-agent/controller-ca.pem`) and credential (`/etc/steward-agent/credential`). With a credential and no pin, the agent doesn't connect: the credential is only for the controller the lost pin named, and without the pin it would go to whichever controller the agent trusted next.

## Adoption (Steward's, not uCentral's)

uCentral has no adoption step: TIP's devices come with certificates. Steward adds one:
- **Pending:** a device's first `connect` makes it pending. It stays connected for the controller's bookkeeping and is sent nothing. Pending devices are kept in memory only (anything that connects is one): at most 64, the oldest one that isn't connected dropped first, or when all are connected the oldest, disconnected with its record (so no more stay connected, and it's pending again when it reconnects), a controller restart forgets them until they connect again, and their model and firmware are cut to 128 bytes (a longer serial is refused). Their states aren't kept at all. Only adopting and adopted devices are written to `devices.json`.
- **Adopting:** `steward-controller adopt <serial>` (the control socket, `/var/run/steward-controller.sock`) marks the device adopting. When it's connected, the controller sends `steward.adopt` with `{serial, credential}` (32 random bytes, hex). Once the agent answers error 0, the device is adopted.
- **Adopted:** every later WebSocket upgrade must carry `Authorization: Bearer <credential>`. A missing or wrong one closes the connection with code 1008, and changes nothing: the record (model, firmware) and `devices.json` stay as they were. The controller stores only the SHA-256, in `/etc/steward/devices.json` (0600).
- **Forgotten:** `steward-controller forget <serial>` drops the record, so the credential stops working and the device starts over as pending.
- **Validity** runs 2020–2099, because devices without an RTC boot with their image's build date.
