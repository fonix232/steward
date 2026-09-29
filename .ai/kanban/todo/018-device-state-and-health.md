---
id: 18
title: Device state and health
type: feature
priority: P0
category: visibility
effort: M
roles: [router, ap, switch]
components: [steward-agent]
packages: [ubus, rpcd-mod-iwinfo]
unifi: Device details
created: 2026-09-29
---

CPU, memory, uptime, temperature, radios, ports and interfaces reported by every device.

## Acceptance criteria

- [ ] The agent's `state` follows TIP's state schema (`state/*.yml` in `wlan-ucentral-schema`), built by a pure `state::document(sources, previous)` from what the agent gathers each report: ubus (`system info`, `network.wireless status`, `network.device status`, `network.interface dump`, `iwinfo info`/`survey`), board.json, sysfs and `/proc/stat`. No shell commands.
- [ ] `unit`: load, `cpu_load` (total and per core, percent, since the last report), `localtime` as unix time (OpenWrt's `system info` localtime is shifted by the timezone: fixed), uptime, `boottime`, memory (total, free, cached, buffered), `temperature` [average, maximum] of the CPU thermal zones in °C.
- [ ] `radios`, one per radio: phy, band, channel, channels and frequency (every 20 MHz channel it spans), channel_width, tx_power, temperature (its hwmon sensor, matched to the phy through sysfs), chanUtil (busy over active time on its channel since the last report).
- [ ] `interfaces`, one per logical interface but loopback: name, uptime, ipv4 addresses (`a.b.c.d/nn`), ipv6_addresses, dns_servers, counters (its L3 device's); `ssids` on it: bssid, ssid, mode, band, phy, iface, frequency, `radio` (`#/radios/N`), counters, and `location` (`/interfaces/<i>/ssids/<s>`) for the agent's own SSIDs.
- [ ] `link-state`: board.json's WAN ports under upstream, LAN ports under downstream: carrier, speed and duplex (from `1000F`), counters.
- [ ] Nothing is copied wholesale from a source: a wifi-iface's config holds its key. A test checks that no key reaches the document.
- [ ] A missing source (no Wi-Fi, no iwinfo, no sensors) leaves its part out; it never fails the report.
- [ ] Unit tests against fixtures shaped like bifrost's answers (placeholder addresses and MACs).
- [ ] On bifrost: the controller–agent loop; `--json devices` shows the document, checked by hand against iwinfo, ubus and sysfs (channels, tx power, temperatures, lan4 at 1000 full, the SSIDs and their networks). Report size measured.

## Notes

- Clients (`associations`) are STW-19; LLDP peers come with topology.

## Progress

Not started.
