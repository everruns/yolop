---
type: Architecture Specification
title: Tool-output retention and recovery
description: Defines when retained command output becomes a model-visible recovery path.
---

# Tool-output retention and recovery

Status: implemented by the Everruns built-in output-persistence capability and
installed in Yolop's default coding harness.

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
to `everruns-builtins::PersistOutputHook`. Yolop owns composition, regression
coverage at the installed hook boundary, and agent-loop evaluation. It does not
rewrite the hook result locally.

## Evidence

The dependency-isolated `output-persistence` study checks leading-evidence
preservation and zero recovery calls for complete output. The
`persisted-output-reading` study checks correctness, one recovery call at most,
model-call counts, and result bytes for both small and large limited output.
Ordinary coding controls run against the same dependency-isolated binaries.
