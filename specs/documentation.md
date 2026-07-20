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
- `docs/features/` contains focused guides for user-visible features. Use a
  short kebab-case filename such as `sandboxing.md`.
- `specs/` is internal durable product and design memory. It records intent,
  constraints, tradeoffs, and architectural decisions for maintainers.
- `.agents/` and `AGENTS.md` contain contributor and agent workflows, not user
  guidance.

## Direction of links

The public documentation boundary is one-way:

- `README.md` and files below `docs/` MUST NOT link to internal documents below
  `specs/` or `.agents/`, or require users to read them. Public configuration
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

## Enforcement

CI rejects Markdown links from `README.md` and `docs/` into `specs/` or
`.agents/`. Review still owns clarity, task completeness, working examples,
and accurate warnings.

## Public surface

- [`README.md`](../README.md)
- [`docs/`](../docs/)
