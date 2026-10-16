---
type: Test Scenario
title: Complete Cloudflare setup from its published prompt
description: Verifies skills installation, OAuth, and a real MCP call from the unmodified Cloudflare setup request.
---

# Complete Cloudflare setup from its published prompt

## Purpose

A user can paste Cloudflare's setup request into Yolop and reach a working
integration. Saved configuration alone must not produce a completion claim.
The external instructions, skill registry, OAuth browser grant, and live model
make this a manual scenario. Automated MCP tests cover same-turn installation,
removal, client precedence, and transport verification separately.

## Preconditions

- Build with `cargo build --workspace --features yolop-yep/schema`.
- `ANTHROPIC_API_KEY`, preferably supplied through Doppler.
- Internet access and a browser signed in to the Cloudflare account to test.
- The operator can approve Cloudflare OAuth when the browser opens.

## Setup

The fixture creates a disposable home, workspace, config, data, skills root,
npm cache, and npm prefix. It does not use the operator's Yolop configuration.
It retains the transcript and events locally. Those files and the OAuth
credential store are private evidence, do not publish them wholesale.

```bash
doppler run -- python3 scripts/test_cloudflare_setup.py
```

## Steps

The fixture submits this exact prompt, without additional setup hints:

> Fetch and execute the appropriate instructions to set me up for Cloudflare from https://developers.cloudflare.com/agent-setup/prompt.md

Approve the browser grant. In print mode the optional CLI question uses the
unattended default; a skipped optional CLI does not fail required setup.
For an interactive run, use the same isolated paths and enter the prompt in
the TUI. Exercise both accepting and declining the optional CLI separately.
Accepting it additionally requires its installation, persistent CLI guidance,
and `cf auth login`, as specified by the current Cloudflare instructions.

## Acceptance criteria

1. The fetched URL is Cloudflare's published `agent-setup/prompt.md`.
2. Yolop installs the current Cloudflare skill set, with valid `SKILL.md`
   files. The required set comes from the live repository, not a fixed count.
3. The enabled global MCP server points to `https://mcp.cloudflare.com/mcp`.
4. Missing OAuth is reported as pending, and Yolop starts login itself rather
   than asking the user to run commands.
5. The browser grant completes and a live read-only Cloudflare MCP call
   succeeds in the original process. No restart or extra repair prompt.
6. Full setup is claimed only after that success. The fixture's exit status and
   `verification.json` confirm installed skills and a successful MCP call;
   inspect the events for criteria 1 through 5.
7. Skipping the optional CLI leaves it uninstalled.

## Expected variation

Skill counts, names, and Cloudflare's instructions can change. Yolop may use its
native skills installer or the publisher's command. OAuth needs human consent;
the automation must not impersonate that consent. Wording and choice of
read-only verification call may vary.

## Known failure modes

- A configured server with no discoverable tools can be an authentication
  failure. `yolop mcp test cloudflare` must report it, rather than treating an
  empty ambient tool search as proof of readiness.
- A missing browser grant is pending work, not a passing setup result.
- Network, registry, or provider failures must be reported as failures.
- Opt-in shell sandboxing blocks network and global writes. Setup requires the
  normal approval path for those commands; it must not disable containment.
