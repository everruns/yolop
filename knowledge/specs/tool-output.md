---
type: Architecture Specification
title: Tool-output retention and recovery
description: Defines when retained command output becomes a model-visible recovery path.
---

# Tool-output retention and recovery

Status: implemented by the Everruns built-in output-persistence capability and
installed in Yolop's default coding harness, with a bounded structured-result
recovery hook.

## Contract

Full command output may be retained in the session filesystem for diagnostics
without advertising that file to the model. Retention is an internal durability
decision. A model-visible `full_output` path, `output_files` entry, or recovery
annotation is emitted only when persisted stream content is absent from the
inline tool result.

Complete inline output must not invite a second `read_file` or `grep_files`
round. Limited output keeps its leading evidence and offers one bounded
contextual recovery call. Small limited results may use one `read_file`; large
limited results should use one contextual `grep_files` call and must not follow
it with a redundant read.

## Ownership

The distinction between internal retention and model-visible recovery belongs
to `everruns-core::builtins::PersistOutputHook`. Yolop owns composition, regression
coverage at the installed hook boundary, and agent-loop evaluation. Oversized structured results are persisted before the runtime's final text
limit. Yolop returns a valid bounded JSON envelope with `full_output`,
`output_files`, a preview, and command outcome fields, rather than allowing the
provider to receive JSON cut in the middle. Both foreground and background
artifact references use the session filesystem display contract.

Shell timeouts retain the stdout and stderr collected before the deadline,
`success: false`, and `timed_out: true`. On Unix, timeout, output-limit
termination, and cancellation stop the command process group, preventing a
repair from overlapping orphaned pipeline children. Normal completion preserves
deliberately launched services. Nonzero detached commands retain bounded
diagnostics in their failure result and stream complete retained evidence to
`output.log`. The model receives command failures as evidence to diagnose.

Semantic tool errors retain their specific reason in the stored tool-result
message and the provider-visible `Tool error: <reason>` text. ACP also includes
the reason in tool content when marking a call failed. A client's generic
failure label does not replace that diagnostic in the model transcript.

## Evidence

The dependency-isolated `output-persistence` study checks leading-evidence
preservation and zero recovery calls for complete output. The
`persisted-output-reading` study checks correctness, one recovery call at most,
model-call counts, and result bytes for both small and large limited output.
Ordinary coding controls run against the same dependency-isolated binaries.
