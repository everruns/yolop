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

The rejection is narrower than the message suggests, confirmed against the live
endpoint: a request with no `reasoning` field at all is accepted, and so is one
naming an effort. What OpenRouter refuses is reasoning it reads as switched off,
which is `effort: "none"` and, on this endpoint, the bare
`reasoning.exclude: true` the OpenRouter driver always sends to keep provider
reasoning private. Naming an effort is therefore the fix available to Yolop, and
`none` is never one of the levels offered for such a surface.

## What

### The effort scale is merged, not taken from a single source

The scale `/effort` offers, and the scale an explicitly requested effort is
validated against, come from the first source that describes the model:

1. **What the provider advertised at discovery.** Gateways describe their own
   catalog: OpenRouter's `/models` carries `supported_parameters` plus a
   `reasoning` block naming the actual effort levels (`supported_efforts`) and
   the default. The gateway catalog is what knows a model's real scale, so the
   advertisement wins wherever one is on record.
2. **The curated registry** (plus Yolop's local profile overrides). Reviewed
   data about a specific model, and the fallback for models with nothing
   advertised on record.
3. **Yolop's reasoning-required families.** Model families whose endpoints reject
   a turn carrying no reasoning, listed by family prefix so a new point release
   or pricing tier is covered the day it ships rather than at the next dependency
   bump. Scoped to the provider surfaces where the mandate holds: mandating
   reasoning is a property of an endpoint, not of the weights, so a family
   reached through a gateway that requires it and through its vendor's own API
   that does not is described once, for the surface that requires it.

Each layer only fills what the one above left empty. The registry never
overrides an advertisement, and Yolop's own metadata never overrides either.

Driver mappings bound layer 1: `everruns-openrouter 0.18.3` maps every
reasoning model to fixed low/medium/high and drops the catalog's
`reasoning.supported_efforts`, so `catalog_scale_override` in
`src/runtime/discovered_profiles.rs` carries the hand-verified scale for the
affected models (currently only `meta/muse-spark-1.3-contributor`: minimal,
low, medium, high, xhigh, default medium). It applies only while the recorded
advertisement is still exactly the driver's generic scale, so it yields the
moment the driver maps the real levels. `max` stays unoffered until
`ReasoningEffort` grows a variant for it. Re-verify against OpenRouter
`/models` before extending the table.

### Offering a level is not choosing one

Only layers 2 and 3 supply the effort Yolop selects by itself for a model the
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

### Every turn carries the level, however it was built

A typed turn gets its controls from `ProviderChoice::input_message`, but a
background wake, a resumed turn and a child-session message are assembled by the
host and carry no controls at all. They used to reach the provider with nothing
on the wire, so a wake on an endpoint that mandates reasoning failed exactly as
a first turn once did. Any such message is now given the model's level before it
is sent: at the shared turn entry point for the hosts, and in the child-session
runner, which dispatches to the runtime without passing through it.

The repair below is the backstop for what that cannot reach, not the mechanism
by which a wake gets its reasoning.

### A mandated-reasoning rejection repairs itself once

When a turn fails and the provider says reasoning is mandatory, and the turn
carried no effort, Yolop selects the model's default effort, tells the user what
it changed, and sends the same input again. The effort sticks to the model, so
the next turn is legal too and the status bar shows the level rather than `n/a`.
The failed attempt stays in history but its assistant text, an apology for the
error the host went on to repair, is not shown as the turn's answer.

The repair belongs to the turn, not to one surface: the TUI, `--print`, and ACP
all start turns through the same entry point and all get it, ACP also refreshing
the client's effort selector to the level now in force.

Exactly one retry, and only for a request that named no effort. What counts is
what the *request* carried, not what the model names. A turn's controls are
captured when it starts and a mid-turn `set_model` cannot revise them (see
[`conversational-control.md`](./conversational-control.md), EVE-595), so
switching onto an endpoint that mandates reasoning fails with no effort on the
wire while the model already names one. Keying off the request repairs that
turn, and the retry honors the level the model already names rather than
overwriting it with a default.

A rejection of a request that *did* name a level is a choice only the user can
make, so it surfaces with the failure and a hint naming the control that fixes
it (`/effort`, or `/model` to switch models). Every other provider failure is
untouched.

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
- `RuntimeHandles::run_turn_with_reasoning_recovery` owns the one-shot retry and
  the level given to a host-built input; it is the entry point every host starts
  a turn through, and the only place that changes the model behind the user's
  back.
- `background_wake::WakeRunner` owns the same for child-session messages, which
  reach the runtime without that entry point.

## Related

- [Model list](model-list.md), the menu of models a session offers, whose entries
  may pin an effort.
- [Conversational control](conversational-control.md), the control surfaces
  (`set_reasoning_effort`, `/effort`, `/setup effort`) this metadata feeds.
- [ACP](acp.md), the `reasoning_effort` config option served to editors.
