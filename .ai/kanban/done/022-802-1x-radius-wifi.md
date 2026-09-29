---
id: 22
title: 802.1X / RADIUS WiFi
type: feature
priority: P1
category: wifi
effort: M
roles: [ap, router]
components: [steward-agent, steward-render, steward-ubus]
packages: [wpad-openssl]
unifi: RADIUS profiles
created: 2026-09-29
---

WPA-Enterprise networks against a RADIUS server, and RADIUS MAC authentication on open and PSK networks. hostapd's built-in EAP server (a RADIUS server on the device itself, with certificates) is its own card.

## Acceptance criteria

- [x] SSIDs with `wpa`, `wpa2`, `wpa-mixed`, `wpa3`, `wpa3-mixed` or `wpa3-192` authenticate against `radius.authentication` (host, port, secret). Accounting (with its interval), `nas-identifier`, `chargeable-user-id`, `dynamic-authorization`, request attributes, `eap-reauth-period` and `key-caching` become the options OpenWrt's wifi scripts read.
- [x] A secondary server is added as a second address when it shares the primary's port and secret, and is refused otherwise: stock hostapd takes one port and secret per server list.
- [x] `radius.authentication.mac-filter` on an open, OWE, PSK or SAE SSID sends each client's MAC to the RADIUS server.
- [x] Management frame protection follows WPA3: required for `sae`, `owe`, `wpa3` and `wpa3-192`, at least optional for `sae-mixed` and `wpa3-mixed`. On 6 GHz only WPA3 and OWE run: the transition modes become their WPA3 form (listed in the answer as substitutions) and the rest are refused for that band.
- [x] Refused with a reason: an enterprise SSID without a complete server; a RADIUS server given by name rather than address (hostapd takes addresses); `radius.local` and `certificates`; `psk2-radius` and `mpsk-radius` (STW-21); `health`. RADIUS-assigned VLANs stay off.
- [x] Secrets never appear in a rejection or in `render-check`'s output. No string with a control character reaches hostapd's configuration.
- [x] An owned section the configuration sets again loses the options it no longer carries, so a network that stops using RADIUS stops sending clients to it.
- [x] Unit tests, every written option checked against the wifi scripts' schema, and on bifrost: a dry run stages an enterprise SSID and discards it, and the stock wifi scripts, run offline, turn its options into the expected hostapd lines. Applying it on a live AP needs the user's yes.

## Tasks

- [x] `Op::Unset` and `Transaction::unset`; owned sections drop stale options (wireless and network)
- [x] `crates/render/src/security.rs`: encryption, MFP, 6 GHz rules, RADIUS options, request attributes, redaction
- [x] Tests: enterprise modes, secondary servers, MAC authentication, 6 GHz, refusals, secrets, stale options
- [x] `render-check` redacts secrets and can write the planned SSIDs for the offline hostapd check
- [x] The offline hostapd check (`.ai/skills/device-testing`), run on bifrost
- [x] Docs: the ucentral skill's coverage and refusals

## Progress

Done:
- `crates/render/src/security.rs` holds an SSID's security, worked out once per SSID and rendered per band:
  - encryption, including the six enterprise modes;
  - MFP: `sae-mixed` used to get `ieee80211w=0` unless it was configured, and now gets at least 1;
  - the 6 GHz rules;
  - RADIUS: servers, a secondary as a second address, accounting, NAS-ID, CUI, dynamic authorization, request attributes, reauthentication period and key caching.
- It refuses servers given by name, incomplete servers, `radius.local` and `certificates` (split out to STW-60), `health`, `psk2-radius` and `mpsk-radius`. Secrets are redacted from every rejection, and strings with control characters are refused (keys must be printable too).
- `Op::Unset` and `Transaction::unset` (rpcd `uci delete` with `options`): an owned section that's set again loses the options it no longer carries, in `wireless` and `network`.
- `render-check` redacts every secret (keys, `*secret*`, `*password*`, `r0kh`/`r1kh`). `--hostapd` writes the planned SSIDs for `.ai/skills/device-testing/hostapd-check.uc`, which runs OpenWrt's own `wifi.validate` and `wifi.ap` offline and prints the hostapd lines.

Verified:
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. There are 8 new renderer tests (enterprise options, every mode's MFP, transition modes, MAC authentication and accounting on PSK, 6 GHz, refusals without secrets, stale options, schema), and 2 unit tests (request attributes, redaction).
- On bifrost:
  - A dry run staged four test SSIDs and discarded them: WPA2-Enterprise on 2.4 and 5 GHz with a secondary server, accounting, DAE and attributes; WPA3-Enterprise; PSK with MAC authentication; SAE-mixed.
  - The offline hostapd check gave `WPA-EAP WPA-EAP-SHA256` with both `auth_server_addr` lines, `WPA-EAP-SHA256` with `ieee80211w=2`, `macaddr_acl=2` on the PSK network, and `ieee80211w=1` on SAE-mixed.
  - Both configs were unchanged afterwards, with nothing in `uci changes` and no files left.
  - rpcd accepted `uci delete` with an `options` list in a discarded session.
- During the first run, openUF was redeployed on bifrost by other work. It restarted, re-provisioned and reloaded its radios, as its log and file times show; Steward applied nothing.

Not done:
- No live apply: an enterprise SSID on bifrost reloads its radios, so it needs the user's yes and a RADIUS server to test against.
- OpenWrt's generator writes an EAP network's accounting server twice. That's stock behaviour, noted in the ucentral skill.
- 6 GHz is covered by unit tests only, since bifrost has no 6 GHz radio.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: dynamic authorization only on enterprise SSIDs (the scripts would write a DAS client without its secret, and the radio would fail); NAS-ID, CUI and DAE refused without a server; MFP on none, psk and wpa listed as a substitution; SAE keys 8 to 63 characters; request attributes within RADIUS's bounds (253 bytes); the offline check flags a DAS client without a secret.
- Second: `eap-reauth-period` 0 to 86400 (hostapd reads it with atoi); values of the wrong kind rejected, and a `mac-filter`, `ieee80211w` or `radius` that can't be read refuses the SSID; unknown keys in `encryption` and `radius` rejected; a key on a mode that takes none refused; an MFP the protocol raises above the one asked listed as a substitution.
- Third: `mac-filter` on an 802.1X SSID refuses it (the scripts do MAC authentication only on open, OWE, PSK and SAE); only `chargeable-user-id` takes `false` without a server; on 6 GHz a raised `ieee80211w` is listed too. Verified on bifrost: every mode and MFP combination, the RADIUS options and the refusals through the offline hostapd check, no secret in any output.
