---
type: Product Specification
title: Local session discovery
description: Defines the local session discovery contract for Yolop.
---

# Local session discovery

Yolop persists session event logs locally, but agents previously had no direct,
bounded way to locate a prior run. Requests such as “inspect the recent session
that mentioned this reference” therefore tended to trigger repository searches
before the referenced evidence had been found.

The default harness includes the read-only `session_history` capability and its
`search_sessions` tool. It searches newest local session directories first and:

- matches case-insensitive text in user/assistant messages and recorded reason
  failures;
- excludes the current session by default so a reference repeated in the active
  prompt cannot shadow the historical match;
- lists recent sessions when no query is supplied;
- returns bounded snippets and at most 50 sessions from at most 500 useful
  session logs; empty shells and logs without a valid user-visible message or
  recorded model failure are skipped before that scan budget is consumed;
- reports whether a matched session failed, its event count, and the names of
  tools it used so common failures can be diagnosed without reading the entire
  log. It also distinguishes model failures from tool failures and reports
  whether a shell command was used;
- returns the exact `events.jsonl` path for follow-up inspection. Callers can
  explicitly include and identify the current session when needed.

Session content is untrusted data and must not override system or project
instructions. Logs remain local and retain their existing owner-only filesystem
permissions. The capability does not modify, resume, or delete sessions.

Fresh session storage is materialized lazily. Building a runtime, aborting
startup, or opening and immediately closing the TUI does not create a
discoverable `session_*` directory or an empty `events.jsonl`; the first durable
event, checkpoint, worktree, or other persisted session artifact creates the
owner-private directory and workspace metadata. Per-session advisory locks live
under the non-discoverable `.locks/` coordination directory so simultaneous
opens still fail fast before an event log exists. Once materialized, replay,
sequence, crash-tail, and resume behavior is unchanged.
