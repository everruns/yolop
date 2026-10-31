---
type: Architecture Specification
title: Tool-call shape enforcement and repair
description: Defines layered prevention, validation, and bounded recovery for malformed model-authored tool calls.
---

# Tool-call shape enforcement and repair

Status: implemented in Yolop's default coding harness.

## Contract

Tool arguments are untrusted model output. A provider accepting a request does
not authorize execution, and provider-side structured generation is an
optimization rather than the validation boundary. Yolop applies these layers:

1. Tool schemas use closed objects, required fields, concrete item types, and
   short descriptions that state error-prone container shapes. A tool required
   by a host transition keeps its full schema loaded.
2. The Codex Responses driver sends `strict: true` only when the advertised
   schema fits OpenAI's Structured Outputs subset: the root is an object, every
   object is closed, every property is required, and only known-supported
   schema constructs occur. Deferred stubs, optional properties, and unknown
   constructs stay best-effort (`strict: false`). Other providers retain their
   existing driver behavior.
3. Everruns' `tool_call_repair` capability performs bounded deterministic
   salvage and allows one corrective re-prompt for calls it cannot salvage.
4. Immediately before execution, Yolop validates every call against the tool's
   full authoritative JSON Schema, including when the provider saw a deferred
   stub. The first error blocks only that call and returns JSON containing the
   trusted path, expected shape, received JSON type, and one-retry instruction.
5. A failed call, including a validation rejection, automatically reveals that
   tool's full registered schema on the next model step through the existing
   transcript-derived tool disclosure. Other sessions remain deferred.
6. Individual executors retain semantic validation as defense in depth.

## Diagnostic safety and bounds

Validation diagnostics never include argument values or model-authored unknown
property names. They expose JSON types and schema-authored shape metadata only,
so credentials or user data in a malformed value are not copied into the
transcript. Repeated-failure repair instructions remain in the provider-visible
error text as well as the structured result; error rendering cannot hide them. Expected shapes are depth- and property-bounded, one error is
reported per call, diagnostics are capped at 8 KiB, arguments over 1 MiB are
rejected before execution, and compiled validators use a bounded cache.

An invalid trusted schema is logged and skips the pre-execution layer for that
tool rather than disabling execution globally; the executor remains the final
boundary. This is a configuration defect, not a model-repair opportunity.

## Tool shapes per model

Validation and repair make a wrong call cheaper; a familiar tool shape makes it
rarer. The opt-in [native edit tools](native-edit-tools.md) capability offers
OpenAI models Codex's `apply_patch` beside `edit_file`, chosen per turn from the
model, and is the A/B for whether a trained-in shape reduces edit retries.

## Provider boundary

Everruns owns the Codex OAuth lifecycle and OpenAI protocol drivers; Yolop
adapts its settings and registry. Shared Responses and Chat Completions adapters
carry strict tool schemas when conversion is sound. Their inverse converts
introduced optional null placeholders back to omitted properties before calls
are recorded or approved. Explicit nullable values, required nulls, unknown
fields, non-strict schemas and ambiguous unions remain for authoritative
validation. Yolop never repairs those values inside the execution gate.

## Evidence

Focused tests cover strict-compatible and fallback serialization, full-schema
validation behind a deferred stub, value-free structured corrections, valid
pass-through, eager checkpoint disclosure, and default harness wiring. The
feature smoke deliberately asks a live provider to call a typed tool before a
synthetic acknowledgement.
