# `okf` — native Open Knowledge Format support

Status: implemented. Skill in `src/bundled/system-skills/okf/`; detection
capability in `src/capabilities/okf.rs`. **On by default** (the capability is
inert unless a bundle is present).

## Why

[OKF](https://okf.md/spec/) (Open Knowledge Format, Google Cloud, v0.1)
represents a body of knowledge as a plain directory of markdown files with YAML
frontmatter — no database, SDK, or runtime. It is the emerging portable format
for the curated context agents need: architecture, data models, metrics,
processes, conventions. Yolop should be able to **consume** such a bundle as
high-signal context, and **author/validate/maintain** one on request, without
the user having to explain the format each time.

## What

Two cooperating pieces:

### A. The `okf` skill (bundled)

A system skill compiled into the binary, so every session can read, author,
convert to, and validate OKF regardless of network policy. Its `SKILL.md`
carries the whole v0.1 mental model inline — bundle, concept, concept-id, links,
the reserved `index.md`/`log.md`, the required `type` frontmatter field plus
recommended fields, and the three conformance rules — and links the spec for
edge cases. It ships a zero-dependency validator
(`scripts/validate_okf.py`, `--strict` and `--check-links` modes).

Because the skill teaches authoring, **producing** OKF needs no separate
feature: a repository can instruct the agent (e.g. from `AGENTS.md`) to keep its
bundle current, and the agent does so through the skill.

### B. The `okf` detection capability

A passive detector registered on the default harness. At session start it looks
for a bundle and, **only when it confidently finds one**, contributes a short
system-prompt note naming the bundle path and concept count and pointing the
agent at the bundle's `index.md` and the `okf` skill. It exposes no tools —
reading a bundle uses the ordinary file tools; the note simply makes the agent
*aware* the bundle exists without the user prompting. When no bundle is present
it contributes nothing, so non-OKF repositories are unaffected.

## Detection — deliberately conservative

OKF has **no canonical bundle marker**: the spec mandates only a per-file `type`
field, with no required root manifest and no fixed directory name. So the
detector never assumes `.okf/` exists or is authoritative. Resolution order:

1. **Explicit override** — `YOLOP_OKF_BUNDLE_DIR` (workspace-relative or
   absolute). Trusted when it resolves to a directory; the content signature is
   not required because the user declared the location.
2. **Conventional roots** — `.okf/` then `okf/`, each accepted **only** if it
   passes a content signature: it contains an `index.md`, or at least one
   non-reserved `.md` whose frontmatter carries a non-empty `type`. This keeps a
   plain markdown directory (Jekyll, Obsidian, `docs/`) from being mistaken for
   a bundle.
3. **Otherwise nothing.**

The candidate scan is bounded (build/dependency and nested hidden directories
skipped; a hard file-count cap) so a mis-pointed large tree cannot stall
startup. Detection runs once at construction; a bundle created mid-session is
picked up on the next session.

## Non-goals

- No bundle-loading runtime, index, or database — OKF is "just markdown," and
  the file tools already read it. Native support is awareness + know-how, not a
  new storage engine.
- No new authoring tools in the binary; authoring lives in the skill.
- The conventional-root list stays short on purpose; broader or fuzzier
  auto-detection would trade the adoption-safety guard for false positives.
