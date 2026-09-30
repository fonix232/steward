---
id: 66
title: Agent session robustness
type: bug
priority: P2
category: foundation
effort: S
roles: [router, ap, switch]
components: [steward-agent, steward-controller]
depends_on: [5, 11, 18]
created: 2026-09-30
---

From the third review, small gaps in the agent's session and the controller's side of it:

- A long `configure` can outlast the controller's 3-minute silence limit: with a LuCI apply pending, staging retries for up to 2 minutes and the confirmation tries take up to 40 s, and the agent sends no state or ping meanwhile. The controller drops the session and the answer is lost (the uuid catches it up on reconnect). Handle commands in a task, or keep pinging while one runs; or count every frame as heard.
- A failed state gather (ubus down for a moment) ends the session, and the previous report the rates start from is lost. Skip that report and keep the previous one.
- The agent's credential write doesn't remove a leftover `credential.tmp` first, so the file keeps whatever mode it had (the controller's `store.rs` does).
- `forget` closes a device's connection with a `try_send`: with its 8-message queue full, the close is dropped, and the connection stays open with no record.

## Acceptance criteria

- [ ] Written when the card is scheduled into Ready to start.

## Progress

Not started.
