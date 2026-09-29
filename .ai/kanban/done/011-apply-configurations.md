---
id: 11
title: Apply configurations
type: feature
priority: P0
category: foundation
effort: M
roles: [router, ap, switch]
components: [steward-agent]
packages: [rpcd]
created: 2026-09-29
---

configure goes through the renderer and uci::Transaction, is confirmed once the controller answers again, persists the running uuid and is answered 0, 1 or 2 honestly.

## Acceptance criteria

- [x] configure goes through the renderer and uci::Transaction, applied with rpcd's rollback (60 s)
- [x] Confirmed only once the controller answers again (a probe connection that doesn't register); otherwise rpcd reverts it
- [x] The running uuid persists; a configuration that rolled back is refused until a new uuid (no apply/rollback loop)
- [x] configure answered 0, 1 (with rejections) or 2, honestly
- [x] Radio options' original values recorded before the agent first changes them
- [x] Verified live on bifrost (with the user's yes): apply+confirm, rollback, refusal, cleanup; wireless byte-identical afterwards

## Progress

Works: the agent applies radios and SSIDs from configure with rpcd's rollback, confirms after reaching the controller again, keeps the running uuid, refuses a configuration that rolled back, records radio originals, answers 0/1/2. Verified by tests, SDK builds, a no-op run and the live test on bifrost. Not in this card: restoring the radios' original values, which STW-59 covers.

Reviewed end to end three times (2026-09-29 and 30), fixed in this commit each time:
- First: radios' HT modes read before rendering (a mode the radio lacks kept it down); no-ops found by reading the staged configs back, not `uci changes`; a pending LuCI Save & Apply retried every 10 s for 2 minutes, then answered 2; confirmation tried at 5, 15 and 30 s after the apply.
- Second: an apply that can't be confirmed is rolled back from its own session before the answer; "nothing to change" is answered only when no other apply is pending (a fresh session's `uci confirm`, which can never confirm or revert anything, per rpcd's source); staging errors name the op, never its values.
- Third: verified. On bifrost: a no-op answered 0, a rolled-back uuid refused until a new one, the probe connection not registering. The new rpcd paths ran live on bifrost too (`rollback-check steward_test`, 2026-09-30, with the user's yes): another session's apply refused while one is pending, `pending()` true then false after the revert, the agent's own rollback reverting at once; bifrost's configs unchanged.
