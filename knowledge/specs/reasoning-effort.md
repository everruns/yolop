---
type: Product Specification
title: Reasoning effort
description: Defines how Yolop resolves a model's reasoning-effort metadata across profile sources and how it recovers from endpoints that mandate reasoning.
---

# Reasoning effort

Status: v1 implemented.

## Why

Reasoning effort is a per-model control, and Yolop used to learn about it from
one place: the curated model-profile registry in `everruns_provider`. That
registry is authored data on a release cadence, so it is always behind the
catalog of a gateway like OpenRouter, where a model id can name an endpoint no
release has described yet.

A model missing from the registry lost the control twice over. `/effort` had
nothing to offer ("current model profile does not expose reasoning efforts"),
and every turn went out with no reasoning control at all, so an endpoint that
mandates reasoning rejected the turn with a 400 the user could neither read nor
act on:

```
Reasoning is mandatory for this endpoint and cannot be disabled.
```

Two answers exist for the same question, and Yolop was reading only one of them.

## What

### The effort scale is merged, not taken from a single source

The scale `/effort` offers, and the scale an explicitly requested effort is
validated against, come from the first source that describes the model:

1. **The curated registry** (plus Yolop's local profile overrides). Authoritative
   wherever it speaks: it is reviewed data about a specific model.
2. **What the provider advertised at discovery.** Gateways describe their own
   catalog: OpenRouter's `/models` carries `supported_parameters`, and its driver
   turns that into a profile per model. This is how a model the registry has
   never seen still gets a scale.
3. **Yolop's reasoning-required families.** Model families whose endpoints reject
   a turn carrying no reasoning, listed by family prefix so a new point release
   or pricing tier is covered the day it ships rather than at the next dependency
   bump. Scoped to the provider surfaces where the mandate holds: mandating
   reasoning is a property of an endpoint, not of the weights, so a family
   reached through a gateway that requires it and through its vendor's own API
   that does not is described once, for the surface that requires it.

Each layer only fills what the one above left empty. Discovery never overrides a
registry answer, and Yolop's own metadata never overrides either.

### Offering a level is not choosing one

Only layers 1 and 3 supply the effort Yolop selects by itself for a model the
user has given none. A provider catalog saying a model *accepts* efforts is not
the same as its endpoint needing one, and sending a level the user never chose
changes how a model behaves whose own default is fine. So a discovered scale
fills the picker and validates an explicit choice, while the effort actually
sent stays unset until the user picks one, the curated profile names a default,
or the family is one whose endpoint mandates reasoning.

Discovery is asynchronous while the effort selector is not, so what a provider
advertised is cached per process by the paths that already list models: the
pre-turn availability check and the `/model` browser. Before any discovery call
has run the layer is simply empty, which is why layer 3 exists: a model whose
endpoint mandates reasoning has a scale and a default from the first turn.

### A mandated-reasoning rejection repairs itself once

When a turn fails and the provider says reasoning is mandatory, and the turn
carried no effort, Yolop selects the model's default effort, tells the user what
it changed, and sends the same input again. The effort sticks to the model, so
the next turn is legal too and the status bar shows the level rather than `n/a`.

Exactly one retry, and only for a turn that named no effort. A rejection of a
turn that *did* name a level is a choice only the user can make, so it surfaces
with the failure and a hint naming the control that fixes it (`/effort`, or
`/model` to switch models). Every other provider failure is untouched.

## Ownership boundary

- `crate::runtime::reasoning` owns the reasoning-required families, the
  fallback scale, recognizing the provider's mandate error, and choosing the
  recovery effort.
- `crate::runtime::discovered_profiles` owns the process-wide cache of
  provider-advertised profiles; the discovery paths write to it, the effort
  lookups read it.
- `merged_reasoning_effort_config` in `crate::runtime` owns the merge order every
  effort surface resolves through, and
  `ProviderChoice::auto_reasoning_effort_for_model` owns the narrower question of
  which effort Yolop selects unasked.
- `crate::runtime::session` owns the one-shot retry, and it is the only place
  that changes the model behind the user's back.

## Related

- [Model list](model-list.md), the menu of models a session offers, whose entries
  may pin an effort.
- [Conversational control](conversational-control.md), the control surfaces
  (`set_reasoning_effort`, `/effort`, `/setup effort`) this metadata feeds.
- [ACP](acp.md), the `reasoning_effort` config option served to editors.
