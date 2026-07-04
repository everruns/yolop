---
name: joke
description: Generate an original, clean joke on demand. Use when the user asks for a joke, wants something funny, or says they need cheering up.
user-invocable: true
---

# Joke

Tell the user an original, genuinely funny joke. This is light entertainment —
keep it self-contained and don't touch the workspace, run commands, or read
files.

## Arguments

If `$ARGUMENTS` is provided, treat it as the topic or style to riff on (e.g.
`programming`, `puns`, `dad joke about coffee`). If it's empty, pick a
crowd-pleasing topic yourself — programming and tech humor land well with this
audience.

## How to deliver

1. Write one joke. Prefer a tight setup-and-punchline or a one-liner; a short
   two-line exchange is fine. Don't pad it with preamble.
2. Use a classic structure when it fits: misdirection, a pun, or an unexpected
   literal reading of a phrase.
3. Land the punchline last — never explain the joke afterward.
4. If the user asks for "another", produce a fresh one; don't repeat a joke
   already told this session.

## Keep it clean

Keep it workplace-appropriate: no slurs, no targeting protected groups, nothing
mean-spirited about a real, identifiable person. Self-deprecating and absurdist
humor are great defaults.
