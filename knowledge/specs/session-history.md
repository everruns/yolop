---
type: Product Specification
title: Local session discovery
description: Defines the local session discovery contract for Yolop.
---

# Local session discovery

`yolop sessions migration-check <session-id>` is a read-only source preflight
for the [direct Engine cutover](engine-execution.md). It takes the existing
exclusive session lease and refuses a live session, malformed records or
inconsistent lineage. Output contains counts and a source fingerprint, not
message bodies or provider-opaque context. A valid source check does not claim
that import or execution cutover has happened.

Yolop persists each canonical session in its own native Engine SQLite authority
without a duplicate `events.jsonl` export. Agents need a direct,
bounded way to locate a prior run. Requests such as “inspect the recent session
that mentioned this reference” therefore tended to trigger repository searches
before the referenced evidence had been found.

The default harness includes the read-only `sessions` capability, driven
through `yolop sessions search` instead of a model tool. Detached commands honor
`--session-dir` as well as the configured data root. It searches newest
local sessions first, ordered by authority and WAL modification times, and:

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
- returns the exact SQLite authority path, or the legacy JSONL source path before import, for follow-up inspection. Callers can
  explicitly include and identify the current session when needed.

Session content is untrusted data and must not override system or project
instructions. Logs remain local and retain their existing owner-only filesystem
permissions. The capability does not modify, resume, or delete sessions.

SQLite creates an owner-private session directory when its authority opens.
An empty authority is not a saved conversation: discovery skips it until there
is a valid user-visible message or recorded model failure. Workspace metadata
and other product files remain lazy. Per-session advisory locks live under
`.locks/` so simultaneous opens still fail fast before authority creation.

Search and replay stream raw events from a read-only SQLite snapshot, including
abandoned history. Resume uses the active native branch. Source precedence is
the session directory, the previous `.engine/<session-id>/` directory, then the
former shared authority. Legacy JSONL is read only when no native binding exists;
it remains an import source, never a new event writer. Read-only inspection does
not claim execution or recover commands and can proceed while another session
holds a SQLite writer lock.

Back up each session directory and the root `engine.key` together. A missing
key for an existing authority refuses startup. Earlier directory and shared
sources remain intact after migration, but updated hosts own resumed sessions.
