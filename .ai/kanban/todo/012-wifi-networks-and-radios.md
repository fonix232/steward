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

- [ ] A renderer (crates/render, pure, no ubus) turns uCentral `radios[]` and `interfaces[].ssids[]` into UCI changes, given the device's current wireless config
- [ ] Radios: matched by band (2G/5G/6G to the `wifi-device` with that band). Sets channel (a number, or auto), htmode (from channel-mode + channel-width), country, txpower, disabled. These are options on existing sections, so the renderer lists each one it sets: applying (STW-11) records the originals to restore
- [ ] SSIDs: one `wifi-iface` per SSID per band, in sections the agent owns (named `stw_*`, marked `steward '1'`). Covers ssid, mode ap, network (the device's `lan`, until VLANs in STW-13), encryption (none, psk2, psk-mixed, sae, sae-mixed, owe), key, hidden, isolate, ieee80211w. Owned sections no longer in the config are deleted; sections the agent doesn't own are never touched
- [ ] Anything unsupported comes back as a rejection, for configure's answer: a band the device lacks, mesh/WDS modes, enterprise encryption (STW enterprise card), multi-PSK
- [ ] Every UCI option it writes exists in the consumer (bifrost's /usr/share/schema/wireless.*.json), checked by a test against that list
- [ ] Unit tests on fixtures, and a dry run on bifrost: render against its real wireless config, stage in an rpcd session, inspect `uci changes`, discard without applying (no WiFi reload)

## Notes

Radios are addressed by phy= on current OpenWrt.

## Progress

Not started.
