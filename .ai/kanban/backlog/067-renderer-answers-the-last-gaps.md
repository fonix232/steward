---
id: 67
title: 'Renderer answers: the last gaps'
type: bug
priority: P2
category: foundation
effort: M
roles: [router, ap, switch]
components: [crates/render]
depends_on: [12, 13, 22, 28, 32]
created: 2026-09-30
---

Low findings from the third review, each an answer that's less exact than it could be:

- A substitution's shape differs: `security.rs` gives the bare value (`"required"`, `"sae"`), `lib.rs` gives `{path: value}`, which matches PROTOCOL.md ("the JSON that `parameter` was replaced with"). One shape.
- A PoE wildcard that selects no powered port sets nothing and refuses nothing (answer 0).
- An owned VLAN the same configuration drops still blocks its untagged ports, so moving a port's untagged VLAN in one configuration is refused.
- A `channel-mode` of the wrong kind isn't listed when its width is refused as well.
- A CNAME whose alias also has an A record in the same configuration is accepted (dnsmasq starts, but DNS forbids a CNAME beside other data).
- Dynamic addressing gets no zone, and the answer doesn't say so.
- `addressing: none` on the device's own `lan` is answered 0 while the lan keeps its address; TIP's `none` means no IPv4. Decide which reading Steward takes.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
