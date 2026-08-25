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
become polling.

Warnings are transition notices, not recurring reminders. Each warning fires
once for its relevant unchanged state. An exact repeated read or search still
returns a compact content-addressed freshness marker on every repeat, but only
the first marker for those bytes carries the warning. New result bytes create a
new evidence state.

## Checkpoint transition

After 48 exploration tools without mutation or decisive validation, the host
requires `progress_checkpoint` before another exploration, status, or waiting
tool can execute. The checkpoint is bounded and structured:

- one to eight established facts;
- one current hypothesis;
- up to six missing pieces of evidence;
- one next decisive action, classified as mutation, validation, or no-change
  diagnosis.

Because the gate can make this tool the only permitted exploration transition,
`progress_checkpoint` is never deferred: its full schema remains in the
provider-visible tool set. Container descriptions explicitly require JSON
arrays for `facts` and `missing_evidence`. Calls that still misshape a field are
blocked before the checkpoint executor and receive the structured correction
defined by [tool-call shape enforcement](tool-calling.md).

An accepted checkpoint resets the exploration tranche and re-enables exploration.
Submitting the same checkpoint again on unchanged state is rejected. Mutation
or validation clears the gate directly, so the guard cannot trap a decisive
action behind its own checkpoint. On the next reasoning step, the host removes
the statically blocked exploration and waiting tools from the provider-visible
list, including `tool_search`. `progress_checkpoint` remains fully visible,
while mutation tools and argument-dependent `bash` remain available for a
decisive mutation or validation. The pre-tool hook remains the enforcement
backstop for a stale or provider-invented call. A final answer needs no tool, so
the agent can still report a complete read-only diagnosis instead of
manufacturing a change.

## State and reset boundaries

Mutation resets exploration, repeated-evidence reuse, checkpoint, and
validation state because repository evidence may now mean something different.
Validation resets the exploration trajectory while remaining deduplicated by
workspace-state and normalized command. A different read/search scope resets
only repetition for that scope; it does not erase the session-wide exploration
budget. Different result bytes reset the unchanged-evidence state for that
fingerprint, covering external writers without requiring a filesystem watcher.

State is bounded per session and across live sessions. The active session's
bounded state is owner-only beside its event log and is restored only when its
recorded tool count does not exceed the active replay branch. A rewind behind
that state or an incomplete state-file write therefore discards stale guard
state instead of applying it to an earlier trajectory.

## Ownership

This is Yolop host behavior: the capability composes Everruns' existing
per-reason tool-definition transform with its pre-tool and post-tool hooks. The
runtime already rebuilds that provider-visible list on every reasoning step, so
the transition needs no competing runtime loop or `everruns-*` dependency
change.

## Evidence

The feature tests drive the registered hooks through a real llmsim turn and the
provider-visible tool transform, including unchanged payload compaction,
warning-once behavior, checkpoint-only gating of blocked paths, checkpoint
acceptance, and resumed exploration. A deterministic advisory-only
baseline/candidate study requires at least 50% fewer calls and result bytes with
the same completed diagnosis. Mutation, validation, long read-only diagnosis,
and persisted-session boundaries have focused negative-path tests.
