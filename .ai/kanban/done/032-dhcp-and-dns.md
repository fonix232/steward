---
id: 32
title: DHCP and DNS
type: feature
priority: P1
category: gateway
effort: M
roles: [router]
components: [steward-agent, steward-render]
packages: [dnsmasq, firewall4]
unifi: DHCP, DNS records
depends_on: [13]
created: 2026-09-29
---

DHCP ranges and reservations per network, local DNS records.

## Acceptance criteria

- [x] A network Steward made (`stw_vlan<vid>`) takes `ipv4.addressing`:
  - `static`: the device's address and prefix from `subnet` (CIDR), with `gateway` and `use-dns`;
  - `dynamic`: a DHCP client;
  - `none`.

  The device's own `lan` and the networks it joined keep their addressing, as before. Refused: `auto/<n>` subnets (no `globals.ipv4-network` yet), and addressing on `upstream` (WAN) interfaces.
- [x] `ipv4.dhcp` on a static network serves DHCPv4 through dnsmasq: an owned `dhcp` section with the pool (`lease-first`, `lease-count`, dnsmasq's 100 and 150 without them), `lease-time` (6h by default) and the DNS servers handed out (`use-dns`, option 6). The section sets `dhcpv4 'server'`, without which current dnsmasq serves nothing. A pool that doesn't fit the subnet, or takes in the device's own address, is refused, and so is DHCP on a device without dnsmasq.
- [x] `ipv4.dhcp-leases` reserve addresses: an owned `host` section per MAC, at the subnet's address plus `static-lease-offset`, with its `lease-time`. Refused:
  - an address outside the subnet, or the device's own;
  - a MAC or address given twice;
  - `publish-hostname: false`: dnsmasq puts every client's name in DNS.
- [x] A routed network gets its own firewall zone (`stw<vid>`). Its input is rejected but for DHCP and DNS from it, its output is accepted, and it forwards to `wan` when there's a `wan` zone. Anything more is STW-30's. fw4 warns about options it doesn't know, so Steward's firewall sections carry no `steward` marker and are known by their `stw_` names.
- [x] Local DNS records: uCentral has none, so Steward adds a top-level `dns-records` list (`name`, `type` A, AAAA or CNAME, `value`), rendered as dnsmasq `domain` and `cname` sections. Bad names or values are refused.
- [x] `dhcp` and `firewall` join the transaction, so the rollback covers them. Owned sections the configuration no longer wants are deleted. `port-forward` (STW-31), `disallow-upstream-subnet` (STW-30) and the DHCP client's extras are refused.
- [x] Unit tests, with every written option checked against what netifd, dnsmasq's init script and fw4 read. On bifrost a dry run stages a routed VLAN with a DHCP pool, reservations, its zone and DNS records, then discards it. Applying it needs the user's yes.

## Tasks

- [x] `Sections`: owned sections in `dhcp` and `firewall`
- [x] `crates/render/src/routed.rs`: addressing, DHCP pool and reservations, the zone and its rules, DNS records
- [x] Wire into `networks()` and `render()`; stale sections deleted
- [x] Agent and `render-check`: read `dhcp` and `firewall`, stage them
- [x] Tests and option lists; dry run on bifrost; docs

## Progress

Done:
- `crates/render/src/routed.rs` covers:
  - `Sections`: a config's owned sections, with `dhcp`'s marked and `firewall`'s known by their `stw_` names;
  - addressing for Steward's own VLAN networks (static, dynamic or none);
  - the dnsmasq pool (with `dhcpv4 'server'`) and reservations;
  - the network's firewall zone, with DHCP and DNS rules and a forwarding to `wan`;
  - `dns-records` as `domain` and `cname` sections.
- `networks()` addresses only the networks it makes. The device's `lan` and joined networks keep the old refusal, now naming the network. `render()` deletes stale owned `dhcp` and `firewall` sections.
- The agent and `render-check` read `dhcp` and `firewall` when the device has them and stage them in the same transaction.
- The meaning of a reservation's address is Steward's: the subnet's network address plus `static-lease-offset`. TIP's renderer writes the raw offset.

Verified:
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`. There are 7 new renderer tests:
  - pool and reservations;
  - the zone with and without `wan`;
  - stale sections, with an unmarked `stw_` dhcp section left alone;
  - dynamic addressing and none;
  - the refusals: auto subnets, a pool that doesn't fit or takes the device's address, bad lease times, reservation clashes, `publish-hostname`, port forwards, upstream, `lan`, no dnsmasq;
  - DNS records;
  - every option against `tests/routed-schema.json`, recorded from netifd's source, dnsmasq's init script and fw4 on bifrost.

  There are also 3 unit tests (subnets, lease times, DNS names).
- On bifrost a dry run staged VLAN 3999 routed as 198.51.100.1/24 with:
  - a pool of 50 from .100 and one reservation;
  - zone `stw3999` with the DHCP and DNS rules and a forwarding to `wan`;
  - an A record and a CNAME.

  rpcd took all of it in one session (network, dhcp, firewall: 10 operations, 0 rejections) and discarded it. All four configs were unchanged, with nothing in `uci changes`.

Not done:
- No live apply: it reloads the network, dnsmasq and the firewall, so it needs the user's yes, ideally on the router.
- The zone is checked by fw4 itself offline (`fw4-check.sh` on a `stage-export`), not on a live firewall.
- DHCPv6 and router advertisements for routed networks aren't covered: IPv6 is untouched.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: a duplicate CNAME or a CNAME loop, which would stop dnsmasq, refused; reservations without a pool refused; pool numbers whole; `stage-export`, `dnsmasq-check.sh` and `fw4-check.sh` run dnsmasq's and fw4's own code on the staged configs offline; unknown keys in `ipv4`, the pool, reservations and records rejected; the agent's read-back covers `dhcp` and `firewall`.
- Second: an empty `use-dns` refused (it would stop dnsmasq); DHCP and DNS only where dnsmasq runs, and a zone only where fw4 is active (procd's answer, not the config file); `addressing` of the wrong kind refused; a subnet overlapping another network refused; bad lease times refuse their pool or reservation; `send-hostname` handled.
- Third: verified. On bifrost: static, dynamic and none; pools, reservations, zones and records through dnsmasq's and fw4's offline checks; the "isn't running" refusals with dnsmasq and fw4 stopped.
