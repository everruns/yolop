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
- The default model must be a `repo::file.gguf` spec, and its chat template must
  emit tool calls as JSON inside `<tool_call>`. Both are hard constraints, not
  preferences. A safetensors default means downloading full-precision weights to
  quantize them locally, several times the bytes for the same result. The
  template shape is narrower still: mistralrs 0.8.1 strips the `<tool_call>`
  wrapper and JSON-parses what is inside, so a model whose template emits the
  `<function=…><parameter=…>` XML variant, Qwen3-Coder among them, produces tool
  calls that never parse. A local model that cannot call tools cannot drive the
  agent loop, so this disqualifies a model no matter how well it codes.
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

The engine dominates build time. The gate exists for that reason alone. It is
*not* about toolchains (the default engine features are pure Rust, so an enabled
build still needs no C/C++ compiler) and not about crates.io publishability (an
optional dependency publishes fine).

### Measured cost

Cold release builds (the shipped profile), separate target directories, no
cache, 4-core container. Crate counts come from diffing a `mistralrs`-only
probe lockfile against this one.

| | Default | `--features local-inference` | Delta |
|---|---|---|---|
| Crates | 763 | 1021 | +258 (+34%) |
| Cold release build | 12 min 35 s | 29 min 09 s | +16 min 34 s (2.32×) |
| Binary on disk | 96.3 MiB | 137.6 MiB | +41.3 MiB (1.43×) |
| Release tarball (`.tar.gz`) | 27.8 MiB | not measured | — |
| Native-toolchain deps | none | none | — |

Debug builds pay a separate, larger price. `mistralrs-core` alone links an
844 MiB debug rlib, and because a feature set defines a whole graph, a `target/`
holding both the routine set and `--all-features` compiled 248 crates twice and
reached 16 GB.

So the routine commands stay on `--features yolop-yep/schema`, which resolves to
the same 519 crates as a default build, and the engine gets one job of its own
with its own cache ([`ci.yml`](../../.github/workflows/ci.yml)). That job both
lints and tests: ten tests are behind the feature (the driver's tool-call
conversion, the downloader's progress accounting), and they are invisible to the
coverage job because it never enables the feature. Running them there is nearly
free, since linting `--all-targets` has already compiled them. Locally, use a
separate `CARGO_TARGET_DIR` when you need the engine compiled.

These rows predate the size-optimized release profile (fat LTO at
`opt-level = "s"`). On the same container that profile took a default binary
from 92.8 MB to 71.8 MB, and the engine delta has not been re-measured under it,
so re-run the workflow before quoting either column.

Absolute times are machine-specific; the ratios are the durable part.
[`local-inference-cost.yml`](../../.github/workflows/local-inference-cost.yml)
reproduces this on a runner.

The numbers are more modest than the crate count suggests, and they change where
the gate earns its keep:

- **Compile time justifies the gate for people who build from source.**
  `cargo install yolop` roughly doubles, and a contributor's cold build pays the
  same. That is worth avoiding for the majority who will never select `local`.
- **Binary size is what the distribution split fixes, not the gate.** Through
  v0.17.0 the release binaries were built with the feature on, so a Homebrew
  install carried the engine whether or not the user ever ran a local model:
  **+41.3 MiB of disk** after install, plus a download that grew by the
  compressed delta (the baseline binary compresses 3.46×, 96.3 → 27.8 MiB; the
  delta itself was never measured, so do not quote a figure for it). That is
  what the lever in [`cli-binaries.yml`](../../.github/workflows/cli-binaries.yml)
  was for, and it has since been pulled: the default artifact no longer carries
  the engine, and the people who want it download a build that does. See
  [Distribution](#distribution).

## Distribution

`local-inference` is **off** in the crate's default features, so
`cargo install yolop` and the contributor `cargo check` loop stay fast. A
release ([Release](release.md)) resolves the same trade by shipping two builds
per target rather than picking one:

| Asset                              | Features                    | For                              |
|------------------------------------|-----------------------------|----------------------------------|
| `yolop-<target>.tar.gz`            | default, no engine          | everyone; what Homebrew installs |
| `yolop-<target>-metal.tar.gz`      | `metal` (macOS)             | local models on an Apple GPU     |
| `yolop-<target>-cuda.tar.gz`       | `cuda` (Linux x86_64)       | local models on an NVIDIA GPU    |

The default asset is the portable one because it is the only build that runs
wherever the target does. An accelerated build carries a runtime requirement
the plain one does not: a `cuda` binary needs an NVIDIA driver present, and
that is not a thing to hand someone who typed `brew install`. There is
deliberately no CPU-only engine build in the list: the [Acceleration](#acceleration)
finding is that an unaccelerated engine measures the wrong thing, so shipping
one as a *download* would only spread that mistake.

A build without the feature still resolves `--provider local` — only the driver
is compiled out. Such a build reports the provider unusable so it stays out of
automatic provider fallback, and fails loudly if selected explicitly, rather
than silently disappearing from the picker.

## Acceleration

The engine runs on **CPU** unless a GPU backend is compiled in, and the
difference is not a detail: an 8B model on CPU is slow enough to read as "local
models are useless" when the real finding is "this was never accelerated".
Evaluating the experiment on an unaccelerated build measures the wrong thing.

Two features turn a backend on. Each implies `local-inference`, and each needs a
vendor toolchain the plain feature does not, which is why they are separate:

```bash
cargo build --release --features metal   # Apple Silicon; macOS only
cargo build --release --features cuda    # NVIDIA; needs the CUDA toolkit
```

Their existence is a second reason no check can use `--all-features`, on top of
the build-cost one above: `--all-features` turns both on, and on a Linux runner
both fail outright. `cuda` pulls `cudarc`, whose build script panics when `nvcc`
is absent; `metal` pulls `objc2`, which refuses to compile off Apple platforms.
Cargo cannot exclude a feature from `--all-features`, so every check names the
features it wants.

`metal` is compiled for both macOS release targets by
[`metal-build-check.yml`](../../.github/workflows/metal-build-check.yml), on
pull requests that touch the manifests. Without it the accelerated backends
would be compiled nowhere at review time, so a dependency bump could break them
and only a release would find out. `cuda` has no review-time equivalent: it is
the slower build and the one no `pull_request` path can justify on runner cost.

`cuda` is now also compiled in CI, by the release itself rather than a check:
[`cli-binaries.yml`](../../.github/workflows/cli-binaries.yml) installs Ubuntu's
`nvidia-cuda-toolkit` for the accelerated Linux build. That proves it compiles,
which is all a GPU-less runner can prove. **Its first evidence that it runs
still has to come from a CUDA host.** Two things about that build are worth
knowing before quoting it as support: the kernels are compiled for one compute
capability, pinned to 8.0 via `CUDA_COMPUTE_CAP` because there is no GPU to
interrogate, and anything newer reaches it through PTX rather than a native
cubin. 8.0 is a floor candle imposes, not a choice: its kernels call
`__hmax_nan` and `__hmin_nan`, which nvcc defines only for
`__CUDA_ARCH__ >= 800`, so a lower capability fails in `compatibility.cuh`
rather than producing a wider-reaching binary. **Pre-Ampere cards are therefore
out of reach of the shipped `cuda` build.**

## The model store

Weights live under `<data_dir>/yolop/models/`, one flat directory per repo
(`/` → `__`). Yolop owns this directory instead of deferring to the engine's own
cache, because an engine-managed cache leaves users with gigabytes they can
neither see nor delete through yolop. `yolop weights list` reports what is on
disk and its size; `yolop weights rm` reclaims it.

**A turn never downloads.** The engine is capable of fetching its own weights,
but it does so inside the first inference call — the turn appears to hang for
several gigabytes with nothing on screen. Instead the driver loads only from the
store and, when the weights are absent, fails immediately with the `yolop weights
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
