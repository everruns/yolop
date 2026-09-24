---
type: Product Specification
title: Action guard idle-promise control
description: Muse-only Classifier check that continues the turn when a reply promises action but makes no tool call.
---

# Action guard

Muse-only guard against idle actionable replies: the model says it will
verify, check, or act, but ends the turn with zero tool calls. The turn
completion gate treats tool-free text as Achieved, so without this guard the
host presents the promise and nothing runs.

## Detection

A Classifier (Jev, TypeSafe backend) judges the final text. The check runs
only when the sync gate says Achieved with zero tool calls on a Muse session
(model id contains `muse`). Threshold is 0.7 on the `actionable_promise`
noul question. Misses, errors, and a missing key keep Achieved (fail open).

## Enforcement

A hit rewrites the verdict to InProgress with reason
`promised action but made no tool call`. The existing continuation budget
bounds the follow-up turn, which carries a nudge to call the tool. CLI, TUI,
and ACP loops all enforce it.

## Configuration

Key: `TYPESAFE_API_KEY` env first, `tokens.typesafe` setting second
(`yolop config set tokens.typesafe ...`). Absent key keeps the Disabled
classifier and the guard dormant.

Model: `--classifier-model` flag wins, then the `classifier_model` setting
(`yolop config set classifier_model ...`), then the backend default. There
is no TUI sidebar control by design.
