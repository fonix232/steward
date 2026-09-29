---
id: 23
title: Band steering and fast roaming
type: feature
priority: P1
category: wifi
effort: M
roles: [ap]
components: [steward-agent, steward-render]
packages: [usteer, wpad-openssl]
unifi: Band steering, fast roaming
depends_on: [22]
created: 2026-09-29
---

Steer clients to the better band and AP, with 802.11k/v/r.

## Acceptance criteria

- [x] `roaming` (true, or its object) turns on 802.11r for WPA2-or-later SSIDs with a key or 802.1X:
  - `message-exchange` sets FT over the air (the default) or over the DS;
  - `domain-identifier` sets the mobility domain (4 hex digits); without it the wifi scripts derive one from the SSID, the same on every AP;
  - `generate-psk` applies to PSK networks;
  - key holders come from `pmk-r0-key-holder` and `pmk-r1-key-holder`, or are both built from `key-aes-256`.

  It's refused on open, OWE and WPA1-only SSIDs.
- [x] An enterprise SSID roams without key holders in its configuration. The stock scripts can't derive an FT key for EAP (`FT_KEY_CANT_BE_DERIVED` fails the whole radio), so Steward derives one from the SSID's name and its RADIUS secret, the same on every AP.
- [x] `rrm` sets:
  - 802.11k neighbor reports (`neighbor-reporting`);
  - reduced neighbor reports;
  - the FTM responder, with `lci` and `civic-location`.

  `stationary-ap`, a radio setting, is refused.
- [x] An SSID whose `services` list `wifi-steering` gets 802.11v BSS transition and 802.11k, and usteer steers it between bands and APs with its own settings. The SSID keeps 802.11k/v but steering is refused, with the reason, when:
  - usteer isn't installed;
  - it isn't running;
  - its `ssid_list` names only other SSIDs.

  The device's own usteer settings are never edited.
- [x] FT keys and key holders are secrets: never in a rejection or `render-check`'s output.
- [x] Unit tests (the steering refusals among them) and the schema check. On bifrost, a dry run finds its running usteer and accepts steering, since it steers every SSID. The offline hostapd check shows:
  - the FT AKMs (FT-PSK, FT-SAE, FT-EAP);
  - the mobility domain;
  - `bss_transition` and the RRM lines.

  Applying it on a live AP needs the user's yes.

## Tasks

- [x] `crates/render/src/roaming.rs`: 802.11r, 802.11k and RRM, steering
- [x] `Usteer` in `Wireless`, gathered by the agent (config and whether it runs)
- [x] Tests, and the offline check's redaction of key holders
- [x] Dry run and offline check on bifrost
- [x] Docs: the ucentral skill

## Progress

Done:
- `crates/render/src/roaming.rs` covers:
  - 802.11r from `roaming`: over-the-air or DS exchange, mobility domain, `generate-psk` for WPA2-PSK, key-holder pairs, and `key-aes-256` as wildcard holders;
  - `rrm`: 802.11k, RNR, the FTM responder, LCI and civic location;
  - steering: `services` listing `wifi-steering` sets `bss_transition` and `ieee80211k`, and is refused when usteer can't give it.
- It works around a stock bug. On EAP the wifi scripts read `auth_secret` after validation renamed it, so their FT key derivation fails with `FT_KEY_CANT_BE_DERIVED`, which fails the whole radio. An enterprise SSID without key holders gets wildcard holders with a key derived from its name and RADIUS secret (SHA-256), the same on every AP.
- `Usteer` sits in `Wireless`. The agent (and `render-check`) asks the running daemon (`usteer get_config`), because its UCI misleads: a later section replaces an earlier `ssid_list`. Steward never edits usteer's settings.
- The offline hostapd check now redacts `r0kh` and `r1kh`.

Verified:
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. There are 9 new renderer tests (defaults, the roaming object, EAP key holders being stable and bound to the secret, refusals on open, OWE and WPA1, bad values without their keys, RRM, steering, the steering refusals, schema), and 2 unit tests (key holders, usteer's list).
- On bifrost, a dry run found its running usteer (it steers every SSID) and accepted steering. It staged and discarded:
  - PSK with roaming, steering and RRM on both bands;
  - SAE-mixed over the DS with a set mobility domain;
  - WPA2-Enterprise with roaming.
- The offline hostapd check gave:
  - `WPA-PSK FT-PSK` with `bss_transition=1`, `rrm_neighbor_report=1`, `rrm_beacon_report=1` and `rnr=1`, and one mobility domain for both bands;
  - `SAE FT-SAE … FT-PSK` with `ft_over_ds=1` and `mobility_domain=5a5a`;
  - `WPA-EAP FT-EAP` with Steward's key holders, and no FT key error.
- The wireless, network and usteer configs were unchanged, with nothing in `uci changes` and no files left.

Not done:
- No live apply: it reloads the radios and needs the user's yes, plus two APs and a client to watch a fast transition.
- DAWN isn't supported. usteer is OpenWrt's default and already runs on the APs.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: key holders checked as hostapd's `add_r0kh` and `add_r1kh` check them (an R1KH-ID is a MAC, an R0KH-ID 1 to 47 characters).
- Second: `lci` and `civic-location` at most 255 bytes (a longer config line fails the radio); `services` entries other than steering refused; key holders beside `key-aes-256` refused; unknown `roaming` and `rrm` keys and values of the wrong kind rejected; an enterprise SSID with a bad `key-aes-256` still gets derived key holders.
- Third: verified. On bifrost: FT-PSK, FT-SAE and FT-EAP with their key holders, the mobility domain, RRM and steering lines, and every refusal, through the offline hostapd check.
