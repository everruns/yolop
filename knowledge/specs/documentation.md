---
type: Policy
title: Documentation Specification
description: Defines the boundary between public documentation and internal project knowledge.
---

# Documentation Specification

## Purpose

This specification defines the boundary between Yolop's public documentation
and its internal product memory. Public documentation must help users operate
Yolop without requiring knowledge of repository internals.

## Information architecture

- `README.md` is the public entry point. It explains what Yolop is, presents
  the primary workflows, and links to public guides for details.
- `docs/` is public, task-oriented documentation for external users. A page in
  this directory must stand on its own for someone who installed Yolop.
- `docs/features/` contains focused guides for user-visible features. A guide
  without companion assets may use a short kebab-case filename. A guide with
  captures or diagrams uses a same-named subdirectory so its Markdown, source
  visuals, and generated assets stay together.
- `knowledge/specs/` is internal durable product and design memory. It records intent,
  constraints, tradeoffs, and architectural decisions for maintainers.
- `.agents/` and `AGENTS.md` contain contributor and agent workflows, not user
  guidance. How that internal context is layered and kept non-duplicative is
  defined in [`agent-context.md`](./agent-context.md).

## Direction of links

The public documentation boundary is one-way:

- `README.md` and files below `docs/` MUST NOT link to internal documents below
  `knowledge/specs/` or `.agents/`, or require users to read them. Public configuration
  examples may still name product-owned paths such as `.agents/hooks.json`.
- Public pages MAY link to other public pages, external standards, and public
  API/source artifacts when those links help complete a user task.
- Specs MAY link to public documentation to identify the user-facing surface.
- Internal contributor material MAY link to both specs and public docs.

Removing an internal link must not remove information users need. Move or
summarize the relevant operational guidance in public docs first.

## Public feature guides

A feature guide should include the sections relevant to its risk and
complexity:

1. What the feature does and when a user would use it.
2. Defaults, prerequisites, supported platforms, and compatibility limits.
3. Concrete commands or configuration examples.
4. Security or data-loss warnings placed before the risky action.
5. Expected behavior, including interactions with related public features.
6. Troubleshooting for likely failures.
7. Honest limitations that affect user decisions.

Pages should use product language, not implementation history. Do not include
roadmaps, rejected alternatives, internal provider comparisons, test strategy,
or source-level schemas unless the user must author that schema directly.

## Change requirements

- A new user-visible feature needs a discoverable README mention. Add a public
  feature guide when the README cannot provide enough setup, safety, and
  troubleshooting detail without becoming unwieldy.
- Behavior changes must update the affected README or public guide in the same
  change. Architectural changes must update the affected spec as well.
- Public and internal descriptions must agree, but they should not duplicate
  exhaustive source-level details.
- Renames and removals must repair inbound public links in the same change.
- Documentation-only changes are validated with the public-boundary check and
  review of changed relative links; runtime test suites are unnecessary unless
  behavior also changes.

## Visual evidence

CLI and TUI feature guides should use reproducible VHS captures as the default
visual evidence. A capture is part of the documentation source, not a manually
staged marketing artifact:

- colocate the `.tape` source and generated asset with the owning guide. Put
  guides with visuals below `docs/features/<feature>/`;
- exercise the real Yolop binary and the real user-visible workflow;
- prefer deterministic offline providers, fixtures, waits, and terminal
  dimensions so regeneration is stable;
- use repository-relative output paths and exclude credentials, personal paths,
  session identifiers, and network dependencies;
- keep the terminal crop legible and focused on the feature outcome; and
- regenerate the asset whenever the captured behavior or presentation changes.

Use an animated GIF when typing or state transitions help explain the feature.
Use a VHS-generated PNG for a single terminal state where motion adds no value.
Use diagrams for architecture or relationships that a terminal recording cannot
communicate clearly. Manual screenshots are a fallback for surfaces VHS cannot
drive, not the primary CLI/TUI capture workflow. Every embedded visual needs
descriptive alt text and must remain understandable from the surrounding prose.
Run tapes from the repository root
(`vhs docs/features/<feature>/<capture>.tape`) so their relative output paths
resolve consistently.

## Capture toolchain

Reproducing any VHS capture, the feature guides above and the README hero
below, needs the same tools on `PATH`:

- **VHS**, which drives **ttyd** and **ffmpeg** (both must be installed
  separately) and renders frames through a headless Chromium it fetches via
  `go-rod` on first run into `~/.cache/rod`.

Install:

- `ttyd` and `ffmpeg` come from the system package manager (e.g.
  `apt-get install ttyd ffmpeg`).
- VHS ships prebuilt binaries; when a release download is unavailable, build it
  from source with `go install github.com/charmbracelet/vhs@latest` (this needs
  a Go toolchain new enough for VHS, `GOTOOLCHAIN=auto` lets Go fetch one).

In a container or as root, set `VHS_NO_SANDBOX=true`; to reuse an already
installed browser instead of the `go-rod` download, point `ROD_BROWSER_BIN` at
its `chrome` binary. These environment knobs belong in the shell around a
recording, not in committed tapes, so tapes stay portable.

`Set FontFamily` is a CSS font stack, and a family the recording host lacks
fails silently: Chromium ignores fontconfig aliases for an unknown family and
substitutes a *proportional* default, so every glyph lands in a monospace cell
with visible gaps. macOS-only families therefore need a fallback that exists
elsewhere, tapes name `Menlo, DejaVu Sans Mono, monospace`, which keeps Menlo
on macOS and stays monospaced on Linux. Inspect a rendered frame before
committing a capture; the failure looks like wide letter-spacing, not a crash.

## README hero capture

The README hero is a checked-in VHS workflow, not a one-off screen recording:

- `docs/demo.tape` owns the terminal presentation and interaction;
- `docs/demo-setup.sh` creates the disposable project under
  `/tmp/yolop-hero-*`; and
- `docs/demo.gif` is the generated asset embedded by `README.md`.

Unlike feature-guide captures, the hero intentionally uses an authenticated
Codex subscription with `gpt-5.6-sol` so it demonstrates the advertised live
agent. This is an explicit live-provider exception to the offline-capture
preference above. Authentication comes from the operator's existing yolop
settings; the tape and rendered frames must never contain credentials, personal
paths, or session identifiers.

Regenerate from the repository root:

```bash
cargo build
vhs validate docs/demo.tape
vhs docs/demo.tape
```

The explicit build warms incremental compilation; the tape also builds while
capture is hidden and allows up to five minutes for a cold build. Keep
`NO_COLOR` cleared so the TUI palette survives the recorder. Keep loop offset
unset: frame one must be the idle shell, followed by typing `yolop`, TUI
startup, the task, and its verified final response in chronological order.

The fixture and prompt should bound model work rather than scripting model
output. Prefer exact dependency versions, existing dependencies, a small test
surface, and low reasoning effort. If the live turn becomes slow, simplify
those inputs before increasing the visible wait. Tune `PlaybackSpeed` in the
tape, not by rotating or trimming the finished GIF, to target about one minute
and less than 5 MiB.

Before committing, inspect both endpoints and the asset metadata:

```bash
bash -n docs/demo-setup.sh
ffprobe -v error -select_streams v:0 \
  -show_entries stream=width,height,nb_frames,duration \
  -of default=noprint_wrappers=1 docs/demo.gif
```

Frame one must show the empty shell prompt. The last frame must show completed
tests and the requested success phrase. Re-run the capture after rebasing when
fullscreen presentation changes, so the hero matches current `main`.

## Enforcement

CI rejects Markdown links from `README.md` and `docs/` into `knowledge/specs/` or
`.agents/`. Review still owns clarity, task completeness, working examples,
and accurate warnings.

## Public surface

- [`README.md`](../../README.md)
- [`docs/`](../../docs/)
