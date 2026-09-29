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

- [x] The renderer maps uCentral `interfaces[]` to the device's networks. An interface without `vlan` is the device's untagged LAN (`lan`). An interface with `vlan.id` is an L2 VLAN network on the device's VLAN-filtering bridge
- [x] A VLAN that already exists on the bridge (a `bridge-vlan` for that VID, from anyone) is reused as it is, never edited. The renderer only finds or adds the interface on `<bridge>.<vid>`. A new VLAN gets owned `stw_bv<vid>` (bridge-vlan) and `stw_vlan<vid>` (interface, proto none) sections
- [x] Tagged ports come from `ethernet[].select-ports` (LAN*, LANn, WAN*, WANn, matched against the board's ports), or all bridge ports when none are given. `vlan-tag: un-tagged` is refused on a port that other VLANs already use untagged
- [x] SSIDs attach to their interface's network instead of always `lan`
- [x] Owned network sections the configuration no longer needs are deleted; unowned ones are never touched. Refused, with reasons: a bridge without VLAN filtering (converting it is invasive), and routed/DHCP interfaces (ipv4 static or DHCP server: the gateway tasks)
- [x] Every option is checked against the network schema, and the agent stages wireless and network in one rpcd transaction
- [x] Unit tests, plus a dry run on bifrost against its real (openUF-managed) network: reusing VLAN 12, adding VLAN 10, staged and discarded. Applying on a live AP needs the user's yes

## Progress

- Done: `crates/render/src/network.rs` (`Ports`, `Network::from_uci`, `networks()`); `render()` networks first, SSIDs on their interface's network, stale owned sections deleted in both configs.
- Done: the agent stages `network` and `wireless` in one transaction; `render-check` renders both.
- Done: honest answers for what isn't applied: routed/DHCP, non-filtering bridge, bad or duplicate VLAN ids, ports off the board or bridge, untagged conflicts, port requests on a joined VLAN.
- Verified: fmt, clippy, all tests (10 new network tests incl. the netifd option check); SDK builds aarch64 + mipsel; dry run on bifrost twice (VLAN 12 joined untouched, VLAN 10 staged, the port mismatch rejected, configs byte-identical, no uci changes).
- Not done: the live apply on bifrost, which needs the user's yes.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: interface, VLAN and ethernet keys this renderer doesn't handle rejected; a `vlan.proto` other than 802.1q and a VLAN without an id refused with their SSIDs; `ethernet` on the lan refused.
- Second: TIP's `vlan-tag` spelling (`un-tagged`; `tagged` and `auto` tag); malformed ethernet entries and ports refused, not dropped; an interface that isn't an object refused, `role` checked; a port the board lacks on a joined VLAN refused; ownership needs the name and the marker; a port selected both ways keeps its first selection.
- Third: any `ipv4` that asks for something is refused (only addressing and DHCP were). Verified on bifrost: VLANs 1, 3 and 12 joined untouched, new VLANs staged, every refusal shown.
