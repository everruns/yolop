---
type: Product Specification
title: Model list
description: Defines the model list contract for Yolop, the ordered cross-provider menu of models a session offers and switches between.
---

# Model list

Status: v1 implemented.

## Why

Providers publish hundreds of models. Yolop used to answer "which model?" with a
catalog: `/model` showed one provider's entire models API response, and ACP was
offered every discovered model of every credentialed provider. A user switches
between a handful, and none of the surfaces knew which handful.

The model list is that handful, made explicit. It is the menu, not a decoration
on top of one: the pickers and ACP serve *it*, and browsing a provider's full
catalog is the path behind it rather than the default view.

It is deliberately not called favourites or bookmarks. Those words describe
marking items in a larger list you still browse; here the list *is* the offer,
and the catalog is the exception.

## What

### One ordered, cross-provider list

An entry names a provider **and** a model, plus an optional reasoning effort and
display label. The pairing is the point: the same weights reached through a
direct account and through OpenRouter are different entries, and selecting one
switches provider and model together. Order is the user's and is preserved
everywhere the list is shown.

Stored as `[[models]]` in `settings.toml` (`crate::config::model_list`):

```toml
[[models]]
provider = "openai"
model = "gpt-5.6-sol"
effort = "high"
```

An unset list means "never configured" and resolves to a built-in default menu
led by `openai/gpt-5.6-sol`. The first edit materializes that default and
appends to it, so a user never silently loses the entries they were using.
Loading is tolerant like the rest of settings: an entry naming an unsupported
provider or missing a model is skipped, never fatal.

A profile may carry its own `[[models]]`, which replaces the global menu rather
than merging into it: a profile that names a menu means "these models, for this
job".

### Surfaces

| Surface | Behavior |
|---|---|
| `/model`, status-bar click | Opens the list. A trailing "Browse all models…" row falls through to the provider picker and that provider's catalog. |
| `/model <id>` | Applies the entry directly when the id is on the list; otherwise the pre-list per-provider picker, as before. |
| ACP `configOptions` | The `model` select option serves the list, filtered to entries whose provider is usable, plus the model in use. |
| `yolop models …` | Shows and edits the list: `list`, `add`, `rm`, `move`, `use`, `reset`. |

Credential filtering differs by surface on purpose. ACP hides entries whose
provider has no credential, because an editor cannot run a login wizard and an
unusable option is a dead end there. The TUI picker and `yolop models list` show
them marked instead: picking a model you have not signed in to yet is a
legitimate way to start using it, and hiding rows from the command that edits
them is how a user loses track of their own list.

### Selecting an unauthenticated model authenticates

Picking a listed model whose provider has no credential is a request to use that
model, so the TUI treats sign-in as part of the same gesture: it opens the
provider's existing credential step, holds the picked entry, and applies it once
authentication succeeds. Every credential path (token paste, environment,
Codex and OpenRouter browser login, custom endpoint) funnels through one hop for
this, so authentication lands on the model the user picked rather than dropping
them in a catalog.

### Administration is an attached CLI, not a tool

`yolop models` is a `CliCapability` on the attached control plane, the same
pattern extensions and session coordination use, and for the same reason: the
agent can edit the list on request without the schemas costing prompt budget
every turn. `list` is read-only; every other operation is consequential. `use`
requires a live session and says so when invoked detached; editing works either
way, because it is an ordinary settings write.

Every mutation reads the resolved list, changes it, and writes the whole thing
back, so ordering is explicit and there is one write path. `set_config` refuses
`models` and points at the CLI rather than offering a second, weaker editor.

A model reference resolves as `provider/model`, `provider:model`, or a bare
model id when it names exactly one entry. Ambiguity is an error naming the
candidate providers, never a guess: distinguishing one model across two
providers is what the list is for.

## Relationship to the per-provider default

`[default_models]` (formerly `[models]` as a table) is remembered state: the
last model chosen per provider, so a pick survives restarts and provider
switches. The list is a menu the user curates. They are different concerns and
now have different keys; a `models` *table* in an existing settings file or
profile is still read as `default_models`.

## Ownership boundary

- `crate::config::model_list` owns the entry type, defaults, and TOML parsing.
- `crate::capabilities::model_list` owns the capability, the `yolop models` CLI,
  the credential filter (`offered_models`), and reference resolution.
- `ModelState::model_options` (ACP) and `crate::tui::setup` (the picker) are
  consumers; neither keeps its own idea of what is offered.
- Live provider/model switching stays with `SetupController::change_provider`
  (see [`conversational-control.md`](./conversational-control.md)); the list
  calls it rather than reimplementing it.
- Provider catalog discovery stays with `crate::capabilities::model_discovery`.

## Related

- [`configuration.md`](./configuration.md), the settings file and its schema.
- [`conversational-control.md`](./conversational-control.md), the contract every
  control surface inherits, including the attached-CLI exception.
- [`acp.md`](./acp.md), per-session model selection over ACP.
