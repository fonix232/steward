---
id: 63
title: Hold the API against slow and greedy clients
type: bug
priority: P1
category: security
effort: S
roles: [router]
components: [steward-controller]
depends_on: [15]
created: 2026-09-30
---

The API serves 32 connections at once, with time limits on the TLS handshake, the headers and the body (STW-15). What's still open, from the third review:

- No deadline on sending an answer: a client that pipelines requests and never reads the answers holds its connection for ever (seen for an hour in-process, and 90 s with a 2.5 MB send queue on bifrost). 32 of them lock everyone out. Give each answer a write deadline (the event stream exempt but bounded), or close an unauthenticated connection after one answer.
- The cap is global: one host reconnecting every 10 s keeps all 32 slots, and every open event stream holds one for as long as it's open. A cap per address, and one for event streams of their own.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
