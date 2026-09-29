---
id: 13
title: Networks and VLANs
type: feature
priority: P0
category: switching
effort: M
roles: [router, ap, switch]
components: [steward-agent, steward-controller]
packages: [netifd]
unifi: Networks
created: 2026-09-29
---

Named networks with VLAN IDs, carried to APs and switches as bridge VLANs, with SSIDs mapped onto them.

## Acceptance criteria

- [ ] The renderer maps uCentral `interfaces[]` to the device's networks. An interface without `vlan` is the device's untagged LAN (`lan`). An interface with `vlan.id` is an L2 VLAN network on the device's VLAN-filtering bridge
- [ ] A VLAN that already exists on the bridge (a `bridge-vlan` for that VID, from anyone) is reused as it is, never edited. The renderer only finds or adds the interface on `<bridge>.<vid>`. A new VLAN gets owned `stw_bv<vid>` (bridge-vlan) and `stw_vlan<vid>` (interface, proto none) sections
- [ ] Tagged ports come from `ethernet[].select-ports` (LAN*, LANn, WAN*, WANn, matched against the board's ports), or all bridge ports when none are given. `vlan-tag: untagged` is refused on a port that other VLANs already use untagged
- [ ] SSIDs attach to their interface's network instead of always `lan`
- [ ] Owned network sections the configuration no longer needs are deleted; unowned ones are never touched. Refused, with reasons: a bridge without VLAN filtering (converting it is invasive), and routed/DHCP interfaces (ipv4 static or DHCP server: the gateway tasks)
- [ ] Every option is checked against the network schema, and the agent stages wireless and network in one rpcd transaction
- [ ] Unit tests, plus a dry run on bifrost against its real (openUF-managed) network: reusing VLAN 12, adding VLAN 10, staged and discarded. Applying on a live AP needs the user's yes

## Progress

Not started.
