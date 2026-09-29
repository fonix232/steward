---
id: 19
title: Clients
type: feature
priority: P0
category: visibility
effort: M
roles: [router, ap, switch]
components: [steward-agent, steward-controller]
packages: [hostapd, rpcd-mod-iwinfo, rpcd-mod-luci, dnsmasq, odhcpd]
unifi: Client list
created: 2026-09-29
---

Who's connected where: WiFi clients (signal, rate, band) and wired clients, with names from DHCP leases.

## Acceptance criteria

- [x] Agent, per SSID: `associations` from `iwinfo assoclist` (TIP's `interface.ssid.association`): station, bssid, rssi, connected, inactive, rx/tx bytes and packets, tx retries and failed, rx_rate/tx_rate (bitrate, mcs, nss, chwidth, sgi, vht, he).
- [x] Agent, per logical interface: `clients` (mac, ipv4_addresses, ipv6_addresses, ports): its SSIDs' stations, its IPv4 neighbours (`/proc/net/arp` on its L3 device), IPv6 from `luci-rpc getHostHints` when present, and MACs the bridge learned on its wired ports (`/sys/class/net/<bridge>/brforward`). Left out: the device's own MACs and the MACs behind its uplink (the port the default gateway is learned on). An FDB-only MAC (no IP seen) goes on the interface of the bridge's untagged network: the FDB has no VLANs (an approximation until topology).
- [x] Agent, where it serves DHCP: `ipv4.leases` (address, hostname, mac) on the interface whose subnet holds them, from `luci-rpc getDHCPLeases`, else `/tmp/dhcp.leases`.
- [x] Agent capabilities carry `macaddr` (lan, wan), so the controller can tell managed devices from clients.
- [x] Controller: `clients` (hub operation, `/api/clients`, control socket and `steward-controller clients`) merges every device's latest state into one entry per MAC: name (lease hostname), addresses, network, and where: wireless (device, SSID, band, signal, rates, connected time) or wired (device, port; the sighting on the port with the fewest MACs, so an AP's port wins over the router's port towards the AP). Managed devices' own MACs and BSSIDs are left out.
- [x] Tests with fixtures shaped like bifrost's answers (placeholder MACs and addresses): associations, clients with the uplink left out, leases per subnet, the brforward and ARP parsers, and the controller's merge (wireless first, leafmost wired port, managed devices left out, names from leases).
- [x] On bifrost: the loop; its real stations appear with signal and rates matching `iwinfo assoclist`, the 43 MACs behind lan4 (uplink) don't, and `clients` lists them. Leases can't be checked live on bifrost (it serves no DHCP): fixtures only; checking on the router needs a yes.
- [x] The web client list is not in this card: it belongs to the web cards (STW-16 and on), which build on `/api/clients`.

## Progress

- Done, agent (`clients.rs`): associations, interface clients (stations, ARP, host hints, brforward; own MACs, uplink and unplaced neighbours on APs left out), leases by subnet, `capabilities.macaddr`.
- Done, controller (`clients.rs`): one entry per MAC (wireless first, leafmost wired port, names from leases, managed devices left out); `/api/clients`, control socket, `steward-controller clients [--json]`.
- Verified: fmt, clippy, 65 tests (11 new); SDK builds aarch64 + mipsel; bifrost: 10 stations equal to iwinfo per SSID, values match, no uplink MACs, controller list correct. Leases: fixtures only (bifrost serves no DHCP); a live check needs the router.
- Reviewed end to end three times (2026-09-29 and 30). The first review found IPv6 host hints missed, a router's WAN neighbours listed as clients, offline devices' last states left out of the merge, and a panic on a closed pipe; all fixed in this commit. The second found them holding on bifrost (30 of 30 stations matching iwinfo, none of the 20 MACs behind the uplink, the CLI into a closed pipe exits 0). No findings. The third found the same on bifrost (30 of 30), with clients now taken from adopted devices' states only.
- Not in this card: the web client list (web cards).
