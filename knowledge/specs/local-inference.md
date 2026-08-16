---
type: Architecture Specification
title: Local inference
description: Defines Yolop's in-process inference provider, why it is feature-gated, and how the gate interacts with distribution.
---

# Local inference

## Why

Yolop's `ollama` provider is not an Ollama integration. It is the OpenAI driver
pointed at `http://127.0.0.1:11434/v1` with a placeholder key — the same code
path as any hosted endpoint, aimed at loopback. Everything that makes local
models awkward lives outside yolop: a second program to install, a daemon to
keep running, and a model store yolop cannot see.

The `local` provider removes that program. The inference engine
([`mistralrs`](https://crates.io/crates/mistralrs)) is linked into the binary
and answers requests in-process, with no socket in between.

What it does *not* remove is the download. Weights are gigabytes and cannot ride
in a crate, so first use fetches them from Hugging Face. The honest framing is
that "install and run a daemon" becomes "wait once" — a smaller promise than
"local models with no setup", and the one this design can actually keep.

This is an experiment. Whether a model small enough to run on a developer laptop
can drive yolop's tool loop well enough to be useful is unproven, and the
provider exists to answer that question with evidence rather than argument.

## What

- Provider name `local`; a model spec is a Hugging Face repo (`Qwen/Qwen3-8B`),
  or `repo::file.gguf` to select one GGUF inside a repo. Safetensors repos are
  quantized in-situ on load.
- No base URL and no credential: `Provider::Local` carries neither, because
  there is no endpoint to address and nothing to authenticate against.
- Reasoning effort is rejected rather than ignored. A repo id is not an entry in
  a model-profile catalog, so there is no effort configuration to resolve an
  explicit request against.
- Engines are loaded once and never evicted. A loaded engine is process-lifetime
  by design, which is why the cache holds `&'static Model`: the engine's
  streaming handle borrows the model, and an `Arc` would make every response
  stream self-referential.

## Feature gate and distribution

The engine adds ~258 crates to a ~763-crate tree and dominates build time — a
`cargo check` with it goes from under a minute to over an hour on a small
machine. The gate exists for that reason alone. It is *not* about toolchains
(the default engine features are pure Rust, so an enabled build still needs no
C/C++ compiler) and not about crates.io publishability (an optional dependency
publishes fine).

Because compile cost lands on builders rather than users, the gate and the
distribution point in opposite directions:

- `local-inference` is **off** in the crate's default features, so
  `cargo install yolop` and the contributor `cargo check` loop stay fast.
- The **release binaries are built with it on**
  (`.github/workflows/cli-binaries.yml`), and the Homebrew formula is generated
  from those tarballs, so the install path most users take
  ([Release](release.md)) ships the engine without anyone compiling it.

A build without the feature still resolves `--provider local` — only the driver
is compiled out. Such a build reports the provider unusable so it stays out of
automatic provider fallback, and fails loudly if selected explicitly, rather
than silently disappearing from the picker.

Accelerated backends (`metal`, `cuda`) do require vendor toolchains and stay
opt-in on top of `local-inference`.

## The model store

Weights live under `<data_dir>/yolop/models/`, one flat directory per repo
(`/` → `__`). Yolop owns this directory instead of deferring to the engine's own
cache, because an engine-managed cache leaves users with gigabytes they can
neither see nor delete through yolop. `yolop models list` reports what is on
disk and its size; `yolop models rm` reclaims it.

**A turn never downloads.** The engine is capable of fetching its own weights,
but it does so inside the first inference call — the turn appears to hang for
several gigabytes with nothing on screen. Instead the driver loads only from the
store and, when the weights are absent, fails immediately with the `yolop models
pull` command that fixes it. The wait is explicit, has a progress bar, and
happens where a progress bar can be drawn.

Listing and removal are plain filesystem work and compile into every build, so a
build without the engine can still clean up what an earlier one downloaded.
Pulling needs the Hugging Face client and is feature-gated with the rest;
without the engine there would be nothing to run the bytes.

## Open questions

- Tool-calling reliability in the agent loop, measured rather than asserted.
  [`evals/harness_basic/`](../../evals/harness_basic/) carries a `local` target
  for this, gated on `YOLOP_LOCAL_MODEL` naming a pulled model so an ordinary
  run skips it. That is the cheap first gate; SWE-bench Verified is the later
  and far more expensive one. **No numbers exist yet** — until they do, this
  provider stays an experiment.
- Downloads resume only at file granularity: an interrupted shard restarts from
  zero on the next pull.
- No quantization choice for safetensors repos; the driver always applies 8-bit
  in-situ quantization on load.
