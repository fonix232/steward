---
id: 2
title: uCentral message types
type: feature
priority: P0
category: foundation
effort: S
components: [crates/proto]
created: 2026-09-29
---

crates/proto types the JSON-RPC 2.0 messages agent and controller exchange: events, commands, and command answers with configure's 0/1/2 error codes.

## Acceptance criteria

- [ ] Requests, notifications and responses parse and serialise as in TIP's PROTOCOL.md
- [ ] `configure` answers carry error 0/1/2 and the rejected list
- [ ] Unit tests, and a JSON-RPC version other than 2.0 is refused

## Progress

Not started.
