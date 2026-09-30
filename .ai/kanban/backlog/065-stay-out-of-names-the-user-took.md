---
id: 65
title: Stay out of names the user took
type: bug
priority: P1
category: foundation
effort: S
roles: [router, ap, switch]
components: [crates/render]
depends_on: [12, 13, 32]
created: 2026-09-30
---

Steward owns the sections it names `stw_*` and marks (`steward '1'`). A section the user named that way without the marker is theirs, but the renderer writes into it: rpcd's named `add` onto an existing section merges into it, and marks it as Steward's from then on. Seen for `wifi-iface` (an SSID's section) and for network sections (a VLAN's `bridge-vlan` or `interface`); `dhcp` already refuses it (`routed::put_owned`). Worse, a firewall zone the user named `stw<vid>` gets a second zone of the same name, and fw4 fails to load the ruleset (`redefinition of symbol`), so the firewall stays down. Keep every section name, as `Sections` does, and refuse what would land on a name that's taken, zones by their `name` too (`Sections::has`).

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
