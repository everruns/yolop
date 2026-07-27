# yolop-extension-cache-break

A [yolop](https://github.com/everruns/yolop) extension that warns you when the
provider's **prompt cache stops being reused** — the failure that is invisible
in the transcript and shows up only on the bill.

A cached prefix is reused turn after turn while it stays byte-identical. Change
something early in the prompt — a tool set, a system-prompt line, a compaction —
and the prefix no longer matches: every following turn pays full input price for
the whole transcript. Nothing in the conversation looks different. The token
counters do.

This extension samples `cache_read_tokens` once per turn and reports the moment
that number falls off a cliff:

```
⚠ cache break on gpt-5 (24.1k → 1.2k)
```

## How it works

Two facets, no tools:

- **`trace`** — the host forwards each agentic lifecycle event as a
  fire-and-forget `trace/event` notification. This server reads only
  `llm.generation`, and only its `metadata.usage.cache_read_tokens` and
  `metadata.model`. Because the notification is never awaited, a slow or crashed
  server can't stall a turn.
- **`status`** — a `status/changed` push puts the warning in the status bar,
  with the long form and level in `/extensions list`. Warnings are also pushed as `log`
  notifications, so they land in `RUST_LOG` output where there is no status bar
  (`--print`, ACP).

The detection rule, and why each part is there:

- **One sample per turn.** Only the first `llm.generation` of each turn
  (gated on `context.turn_id`) is compared. Later generations in the same turn
  are tool-call continuations whose reuse legitimately differs as the transcript
  grows — including them would report a break on every multi-step turn.
- **Absolute drop, not a ratio.** Reuse falling by ≥ `drop_tokens` (default
  4096) versus the previous turn on the same model is a break. Small wobbles are
  normal; losing the prefix is not.
- **Per model.** Switching models shares no prefix, so baselines are kept per
  model and a switch is never reported as a break.
- **Zero reuse is reported once the session is under way**, baseline or not —
  that is what switching models mid-session looks like. It is *not* reported on
  the session's first turn, where nothing has been cached yet and the provider
  is still writing the prefix; warning there would be warning about a cache that
  never existed.
- **The baseline self-heals.** It is updated on every sample, so a break is
  reported once, and the recovery clears the status field.
- **Unknown is not zero.** A generation with no turn id, or with no cache
  counter at all, yields no verdict — but it still consumes the turn's sample
  slot, so a later continuation can't stand in for a missing entry sample.

A break is not always a bug: an intentional context shrink (compaction) looks
identical in the counters. The extension reports the fact and leaves the
diagnosis to you.

Sub-agent generations never reach here — sub-agents run in a child session, and
the host forwards trace events filtered to the current session only.

## Install

```bash
# From this repo (dev): build the binary onto PATH, then install the package.
cargo install --path crates/yolop-extension-cache-break
/extensions install <path-to>/crates/yolop-extension-cache-break
/extensions enable cache-break
```

The server binary (`yolop-extension-cache-break`) must be resolvable on `PATH`
or in the package's `bin/`, per the usual extension rule.

## Configure

One tunable, an ordinary (non-secret) config field:

| Field | Purpose | Default |
| --- | --- | --- |
| `drop_tokens` | How far cache reuse must fall between turn entries, in tokens, before it is reported | `4096` |

```bash
/config set capabilities.ext:cache-break.drop_tokens 8192
```

## Prior art

The idea is [Burke Holland's `cache-break-notifier`](https://gist.github.com/burkeholland/647ad0e579c06a43346ce6a373261eba)
for GitHub Copilot CLI, rebuilt on YEP. The detection rule is the same — first
main-agent call per turn, per-model baseline, absolute drop, self-healing
baseline. What changed is what the host offers: yolop has no in-process
JavaScript API, so the extension is a capability server; the counters arrive
over the `trace` facet instead of an `assistant.usage` event; the warning goes
to the status bar rather than a log line; and the threshold is a declared config
field rather than a constant in the source.
