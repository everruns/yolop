---
type: Test Scenario
title: Install a registry skill and render its diagram
description: Verifies that a skills.sh skill installs mid-session and that a Mermaid fence from it paints as a terminal diagram.
---

# Install a registry skill and render its diagram

## Purpose

One session must be able to install a skill it does not have and immediately
answer with it, and a Mermaid fence in that answer must paint as a diagram
rather than as source.

Neither half is reachable from `cargo test`. The install needs the live
[skills.sh](https://skills.sh) registry over the network, and the answer needs a
live provider, a simulated one will not choose to call `search_skills`. The
rendering half is asserted in unit tests at fixed widths
(`markdown_mermaid_fence_renders_as_a_diagram`), but only a real terminal shows
whether a real model's diagram fits the transcript it actually gets.

## Preconditions

- `cargo build` has run; `target/debug/yolop` exists.
- A live provider is authenticated. `ANTHROPIC_API_KEY` via `doppler run` is the
  reference setup.
- Outbound network access to `skills.sh`.
- A terminal at least 150 columns wide. Below roughly 120 the diagram is
  expected to fall back to source, see Known failure modes.

## Setup

Run the checked-in fixture, which writes a disposable source-only project to
`/tmp/yolop-show-me-orbit` and removes any previously installed copy of the
skill:

```bash
docs/features/show-me/demo-setup.sh
```

Start the agent against it:

```bash
doppler run -- target/debug/yolop -C /tmp/yolop-show-me-orbit --provider anthropic
```

## Steps

1. Ask for the skill by name:

   > Find a skill called show-me in the skills registry and install the
   > humanlayer one into this workspace.

2. In the same session, ask it to explain the fixture visually:

   > Use the show-me skill: how does a repository deletion request flow through
   > this service? Answer with one mermaid sequence diagram over Client, HTTP,
   > Shard, Workflow, Tokens and Git. Keep the labels short.

## Acceptance criteria

**Skill installed**

1. The transcript shows a `search_skills` call followed by an `install_skill`
   call. A `write_skill` call instead means the registry tools were unreachable
   and the model fell back to fetching files by hand, that is a failure of this
   scenario even though the skill ends up on disk.
2. `/tmp/yolop-show-me-orbit/.agents/skills/show-me/SKILL.md` exists after the
   first turn.
3. Its contents match what the registry serves, byte for byte:

   ```bash
   sha256sum /tmp/yolop-show-me-orbit/.agents/skills/show-me/SKILL.md
   curl -sS https://skills.sh/api/download/humanlayer/skills/show-me \
     | python3 -c "import hashlib,json,sys; print(hashlib.sha256([f for f in json.load(sys.stdin)['files'] if f['path']=='SKILL.md'][0]['contents'].encode()).hexdigest())"
   ```

4. The skill is used in the second turn without restarting the session.

**Mermaid diagram rendered**

5. The second answer paints participant boxes and lifelines with box-drawing
   characters (`─`, `│`, `>`), with each participant name inside a box.
6. The literal text `sequenceDiagram` does not appear in the transcript. Seeing
   it means the fence fell back to a source code block.
7. The diagram sits in the transcript under the `agent ›` gutter, not spilling
   past the right edge or wrapping mid-box.

## Expected variation

The model chooses its own participants, message labels, and prose. Step counts
and wording differ run to run, and the diagram may include return arrows one run
and not the next. None of that fails the scenario, criteria 5 through 7 are
about the render, not the content.

The registry install count reported by `search_skills` changes over time, and
other `show-me` skills from other owners appear in the results. Only
`humanlayer/skills/show-me` matters.

## Known failure modes

- **Diagram appears as source.** Usually the transcript-width guard, not a bug:
  Yolop falls back to the code block when any rendered line is wider than the
  transcript, so a diagram with long participant aliases and full argument lists
  can need over 200 columns. Widen the terminal or ask for shorter labels and
  re-run before reporting it. See [`tuika.md`](../specs/tuika.md).
- **Model invents steps the fixture does not contain.** A model failure, not a
  Yolop one. Worth noting in the result, but it does not fail criteria 5–7.
- **Install reports success but no tool call is visible.** Check whether the
  transcript was scrolled; tool rows are collapsed in some layouts.

## Related

- [`skills.md`](../specs/skills.md), scopes, precedence, and the registry tools.
- [`tuika.md`](../specs/tuika.md), the rendering boundary this exercises.
- [Registry skills and Mermaid diagrams](../../docs/features/show-me/show-me.md)
, the public guide, whose recording is this scenario run once.
