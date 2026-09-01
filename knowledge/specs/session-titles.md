---
type: Product Specification
title: Automatic session titles
description: Defines the automatic session titles contract for Yolop.
---

# Automatic session titles

Yolop enables Everruns' `session` capability with automatic title maintenance.
After the first substantive request, the agent assigns a concise title and
updates it only when the conversation's primary theme materially changes.
Greetings, acknowledgements, minor subtopics, follow-ups, and implementation
details do not trigger a rename.

The upstream capability owns this behavioral policy and the
`write_session_title` tool. Yolop keeps the tool's full schema available on the
first turn rather than deferring it behind tool search.

## Event projection

Every effective title mutation emits Everruns' typed
`session.title.updated` event. Yolop treats that event as the durable semantic
record and:

- persists it in the session `events.jsonl` log;
- projects its latest title into `workspace.json` for local session discovery;
- sends ACP clients a `session_info_update` so their session label changes;
- restores the latest event-projected title when resuming a session.

Repeated writes of the current title are no-ops and emit no event. Worktree
metadata updates preserve the projected title and other session metadata.

The runtime session store is the live source of truth. `workspace.json` is a
Yolop-owned projection; replaying the event log repairs that projection after
an interrupted write.
