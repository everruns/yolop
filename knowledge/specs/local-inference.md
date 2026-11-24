---
type: Architecture Specification
title: Local inference
description: Defines the suspended inference provider, the working weight store, and the security and validation bar for restoring an engine.
---

# Local inference

## Current status

Inference is suspended for security. The latest published `mistralrs` is still
0.8.1 as of 2026-11-23. It pins `candle-core` 0.10.2, which requires vulnerable
`zip ^7.2.0`. The fixes require zip 9.0.5 or later; do not restore that graph or
weaken an audit to enable inference.

`--provider local` still resolves but refuses inference with an actionable
disabled message. With `local-inference`, the registry contains a refusing
driver; without it, the provider is unavailable. Neither case silently falls
back to another provider. Weight listing and removal work in every build;
weight pulls work with `local-inference` through `hf-hub` 1.0. The engine's
suspension is independent of the download client.

## Intent and provider contract

The experiment asks whether a model small enough for a developer laptop can
reliably drive Yolop's tool loop without a separate inference daemon. That is
not yet proven. `ollama` remains a separate route: the OpenAI-compatible driver
pointed at a local server, with that server owning inference and its model store.

A local model spec is a Hugging Face repository or `repo::file.gguf` for a
specific file. Local provider selection carries no base URL or credential.
Reasoning effort is rejected rather than silently ignored because arbitrary
repository IDs have no authoritative effort profile.

Restoring an engine must preserve explicit downloads: a turn loads only
installed weights and otherwise names the `yolop weights pull` command. A
multi-gigabyte transfer must never hide inside the first inference request.

## Feature gate and distribution

`local-inference` is off by default. It currently adds the Hugging Face download
client and its Xet transfer stack, with no inference engine. `metal` and `cuda`
currently imply only `local-inference`; they provide no GPU acceleration while
the engine is absent. Routine and optional builds use separate target
directories, as defined in [AGENTS.md](../../AGENTS.md).

The historical engine justified a build gate: on the measured four-core
container it increased a cold release build from 12m35s to 29m09s and an
unoptimized-for-size release binary from 96.3 MiB to 137.6 MiB. Those figures
predate the current release profile and download client. They are not current
size or build-time claims. Re-measure with
[`local-inference-cost.yml`](../../.github/workflows/local-inference-cost.yml)
when an engine returns.

## Distribution

[Release](release.md) retains separate portable and backend-named artifacts:

| Asset | Current behavior |
| --- | --- |
| `yolop-<target>.tar.gz` | Default build, no inference or weight download client; used by Homebrew |
| `yolop-<target>-metal.tar.gz` | Download client only while suspended |
| `yolop-<target>-cuda.tar.gz` | Download client only while suspended |

Do not describe backend-named downloads as working accelerated inference.
When an engine returns, the plain artifact must remain portable. Backend
artifacts may require platform-specific toolchains and GPU drivers. Their
matrix and packaging rules live in the release workflow rather than a second
copy here.

## The model store

Weights live under `<data_dir>/yolop/models/`, one flat directory per repo
(`/` becomes `__`). Yolop owns this store so users can inspect disk use with
`yolop weights list` and reclaim it with `yolop weights rm`, including after
inference is disabled or removed.

Pulling uses `hf-hub` 1.0's async client, including its Xet transfer path. Each
file downloads into a private temporary directory inside the destination
store, then moves into place only after success. A failed transfer never leaves
a partial installed file that a later pull could mistake for complete. Validate
filenames before passing them to the client, including remote listing entries.

Progress uses cumulative high-water marks for per-file and aggregate Xet
reports, so overlapping updates never double-count bytes. Successful
installation completes a progress entry. Missing optional support files are
tolerable; required weights and configuration still fail the pull. Installed
files are skipped. Resume is at file granularity: an interrupted shard starts
again on the next pull.

## Engine restoration bar

- Require a dependency graph with no known vulnerabilities. A fixed
  `candle-core` release alone is insufficient while the engine pins an older
  version. The upstream restoration route needs candle-core 0.11.1 or later
  and zip 9.0.5 or later. A vendored fix or replacement engine must independently
  prove the same security and distribution contracts.
- Keep the default model a prequantized GGUF spec. Downloading full-precision
  safetensors merely to quantize locally multiplies first-use transfer cost.
- Prove tool-call parsing with the selected model's real chat template. The
  previous engine understood JSON inside `<tool_call>` but not Qwen3-Coder's
  `<function=...><parameter=...>` XML shape. Coding quality without working tool
  calls does not qualify a default.
- Revisit model lifetime and streaming ownership against the replacement API.
  The previous engine borrowed its model from response streams and therefore
  retained loaded models for the process lifetime. Do not assume that design
  is still necessary.
- Wire and compile both acceleration backends. The previous CUDA graph needed
  `nvcc` and compute capability 8.0; validate those requirements again instead
  of treating them as a property of the suspended feature aliases. Compilation
  without a GPU proves only build compatibility. Runtime evidence needs real
  Metal and CUDA hosts. An unaccelerated CPU run does not establish useful GPU
  agent performance.
- Run the gated local target in [harness studies](../../evals/harness_basic/)
  with `YOLOP_LOCAL_MODEL` naming installed weights. Measure tool-loop success,
  startup, memory and binary size before describing the experiment as usable.
