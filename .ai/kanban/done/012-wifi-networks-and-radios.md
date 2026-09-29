---
id: 12
title: WiFi networks and radios
type: feature
priority: P0
category: wifi
effort: L
roles: [ap, router]
components: [steward-agent]
packages: [hostapd, wifi-scripts]
unifi: WiFi networks, radio settings
created: 2026-09-29
---

SSIDs (WPA2/WPA3-SAE, hidden, client isolation), radio band, channel, width and power, rendered into sections the agent owns on each AP.

## Acceptance criteria

- [x] A renderer (crates/render, pure, no ubus) turns uCentral `radios[]` and `interfaces[].ssids[]` into UCI changes, given the device's current wireless config
- [x] Radios: matched by band (2G/5G/6G to the `wifi-device` with that band). Sets channel (a number, or auto), htmode (from channel-mode + channel-width), country, txpower, disabled. These are options on existing sections, so the renderer lists each one it sets: applying (STW-11) records the originals to restore
- [x] SSIDs: one `wifi-iface` per SSID per band, in sections the agent owns (named `stw_*`, marked `steward '1'`). Covers ssid, mode ap, network (the device's `lan`, until VLANs in STW-13), encryption (none, psk2, psk-mixed, sae, sae-mixed, owe), key, hidden, isolate, ieee80211w. Owned sections no longer in the config are deleted; sections the agent doesn't own are never touched
- [x] Anything unsupported comes back as a rejection, for configure's answer: a band the device lacks, mesh/WDS modes, enterprise encryption (STW enterprise card), multi-PSK
- [x] Every UCI option it writes exists in the consumer (bifrost's /usr/share/schema/wireless.*.json), checked by a test against that list
- [x] Unit tests on fixtures, and a dry run on bifrost: render against its real wireless config, stage in an rpcd session, inspect `uci changes`, discard without applying (no WiFi reload)

## Notes

Radios are addressed by phy= on current OpenWrt.

## Progress

Works: crates/render turns uCentral radios and SSIDs into UCI changes (radio options on the device's own sections, listed for restoring; SSIDs in owned stw_* sections), with rejections for what it can't do; every option is checked against the wifi scripts' schema. Verified by tests and a dry run on bifrost (staged and discarded, nothing applied). Applying the result came with STW-11, and SSIDs on their own networks with STW-13.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: rejections redacted; keys, radio and SSID fields this renderer doesn't handle rejected, never dropped; channels and widths checked against the band and the radio (a mode it lacks substituted, and listed); a band given twice refused.
- Second: `encryption`'s own keys checked, `ieee80211w` only TIP's three values; tx-power 0 to 30; ownership needs the `stw_` name and the marker; a band listed twice in `wifi-bands` is one section; a key on a mode without one refused; a failed stage names the op, never its values.
- Third: at this commit, interfaces checked (an interface asking for a VLAN refused with its SSIDs, which would run on `lan`; other keys rejected), passphrases printable and SAE without 64-hex keys, and MFP the protocol forces listed as a substitution. Verified on bifrost: dry runs and the offline hostapd check, byte-identical hostapd lines for valid configurations.
