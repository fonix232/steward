---
id: 60
title: Local RADIUS server
type: feature
priority: P2
category: wifi
effort: M
roles: [ap, router]
components: [steward-agent, steward-controller, steward-render]
packages: [wpad-openssl, freeradius3]
unifi: RADIUS server
depends_on: [22]
created: 2026-09-29
---

WPA-Enterprise without an external server: hostapd's built-in EAP server on the AP (uCentral's `radius.local`, with its users), or FreeRADIUS on the router, with a server certificate the controller's CA issues.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
