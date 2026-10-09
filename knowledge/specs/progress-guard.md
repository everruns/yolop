---
type: Product Specification
title: Progress guard trajectory control
description: Defines host-enforced transitions when tool use stops producing new evidence.
---

# Progress guard trajectory control

Status: implemented by Yolop's `progress_guard` capability.

## Intent

Long read/search trajectories remain legitimate when each call answers a new
question. The guard intervenes when the trajectory stops changing state: exact
evidence repeats, validations rerun against the same workspace, investigation
crosses its budget without mutation or validation, or external-event probes
become polling. It also classifies bounded command and tool diagnostics for
invocation, missing-command, wrong-path, and usage failures. Two consecutive
equivalent failures on unchanged workspace state trigger a transition away from
the exact failed action. A different command through the same tool remains
available.

Repeated file paging is tracked separately by normalized path and line interval,
so changing offsets does not disguise overlapping reads as new investigations.
Shell paging feeds the same gate: `sed` and `awk` line ranges, `head` and `tail`
windows, and whole-file `cat`, `less`, `more`, `nl`, `bat`, and `wc` reads are
parsed into path intervals, and interpreter one-liners that read files (or any
other shell command referencing workspace source) count as exploration rather
than falling through as unclassified shell use. Four overlapping reads without
semantic navigation warn and redirect toward `read_file` with offset and limit,
`repo_map`, `repo_symbols`, `ast_grep`, or targeted grep. Eight reads recommend a
checkpoint. This resource history survives mutations, while relevant semantic
navigation resets its pressure, and retained intervals are bounded.

Warnings are transition notices, not recurring reminders. Each warning fires
once for its relevant unchanged state. An exact repeated read or search still
returns a compact content-addressed freshness marker on every repeat, but only
the first marker for those bytes carries the warning. New result bytes create a
new evidence state.

Failure classification is deliberately narrow. The guard retains the original
result and adds its transition warning beside it, so evidence is never hidden.
Arbitrary nonzero exits are not classified as misuse. In particular, an
initially failing test is useful diagnostic evidence and remains available for
the normal edit and validation loop. A success, a different failure class, or a
workspace mutation breaks the consecutive-failure streak.

## Checkpoint transition

After 48 exploration tools without mutation or decisive validation, the host
recommends `progress_checkpoint`. Facts, hypothesis, missing evidence, and a next
decisive action make it useful for breaking an investigation loop. It remains
fully visible, and malformed or repeated checkpoint submissions receive bounded
corrections. A checkpoint does not gate other tools.

Checkpoint state decisions are normal structured tool results: `accepted: false`,
a `status` of `not_needed`, `unchanged`, or `no_progress`, and a concrete `message`
that tells the model what to do next. They preserve the pending recommendation,
failure gate, and exploration counters. Only an accepted checkpoint resets that
trajectory. Malformed arguments remain tool errors with bounded corrections.

Counts, overlapping reads, and repeated validation produce advisory warnings.
Neither the checkpoint threshold nor the session tool count removes tools from
the provider-visible list or blocks a new diagnostic action. The host warns once
at 400 calls; the shared completion controller bounds automatic repair turns.

After equivalent invocation, missing-command, wrong-path, or usage failures,
the pre-tool gate rejects only an exact repeat of the latest failed action on
unchanged state. A different tool or different arguments can recover directly.
Mutation or an accepted recovery checkpoint clears that failure gate. No model
family receives a different tool list.

## State and reset boundaries

Mutation resets exploration, repeated-evidence reuse, checkpoint, and
validation state because repository evidence may now mean something different.
Validation resets the exploration trajectory while remaining deduplicated by
workspace-state and normalized command. A different read/search scope resets
only repetition for that scope; it does not erase the session-wide exploration
budget. Different result bytes reset the unchanged-evidence state for that
fingerprint, covering external writers without requiring a filesystem watcher.
`read_many_files` counts as one exploration tool call, and its semantic
signature preserves the requested path order because its result follows that
same order.

State is bounded per session and across live sessions. The active session's
bounded state is owner-only beside its event log and is restored only when its
recorded tool count does not exceed the active replay branch. A rewind behind
that state or an incomplete state-file write therefore discards stale guard
state instead of applying it to an earlier trajectory.

## Ownership

This is Yolop host behavior: the capability composes Everruns' existing
per-reason tool-definition transform with its pre-tool and post-tool hooks. The
runtime rebuilds the provider-visible list on every reasoning step, and the
hooks expose the authoritative call and structured result before the next
invocation. Both transitions therefore stay in the host capability without a
competing runtime loop or `everruns-*` dependency change.

## Evidence

The feature tests drive the registered hooks through a real llmsim turn and the
provider-visible tool transform, including unchanged payload compaction,
warning-once behavior, checkpoint-only gating of blocked paths, checkpoint
acceptance, and resumed exploration. A deterministic advisory-only
baseline/candidate study requires at least 50% fewer calls and result bytes with
the same completed diagnosis. Mutation, validation, long read-only diagnosis,
persisted-session boundaries, expected diagnostic failures, distinct failure
classes, forced recovery, and checkpoint recovery have focused negative-path
tests. Stuck-session regression tests cover shell paging feeding the overlap
gate, rejection of the third consecutive checkpoint without progress, and the
session budget warn and block boundary. The harness study compares the equivalent-failure loop over three trials
per binary and runs five-trial diagnostic and distinct-failure controls.
