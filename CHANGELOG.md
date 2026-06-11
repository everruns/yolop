# Changelog

All notable user-visible changes to yolop are recorded here.

The format follows the [release spec](./specs/release.md): one section per
released version, newest first, with a `### Highlights` summary, an optional
`### Breaking Changes` block (required for MINOR/MAJOR with breakage), and a
mechanical `### What's Changed` list of merged PRs.

Releases are cut via [`/release`](./.agents/skills/release/SKILL.md), which
tags the version and publishes to crates.io and the Homebrew tap.

## [0.3.0] - 2026-06-11

### Highlights

- **Soft approval with paranoia levels.** yolop batches safe work and pauses
  for plain-language consent only before destructive or outward-facing steps.
  A central `approval_mode` (protective/normal/off) tunes the threshold and is
  configurable via `/setup approval` or the `set_approval_mode` tool.
- **MCP server support.** External MCP servers can be wired in, with
  provider-agnostic deferred tool loading (tool search) to keep large tool
  sets out of the prompt until needed.
- **ACP matured.** A `zed` integration command, persisted-session loading,
  enriched slash-command support, and a move to the upstream
  `agent-client-protocol` SDK.
- **Live model discovery and better providers.** Models are listed live from
  provider APIs; OpenRouter gets a first-class Responses driver with
  reasoning-effort support; the Anthropic shortlist adds Claude Fable 5 and
  Opus 4.7/4.8 (incl. 1M-context ids).
- **Configurable everything.** Schema-described settings with
  `get_config`/`set_config`, workspace + global skills management, and scoped
  user hooks.

### Breaking Changes

- **Tool-call approval gating removed** ([#69](https://github.com/everruns/yolop/pull/69)):
  the opt-in `--ask` flag and its per-tool-call approval gate (TUI prompt, MCP
  pre-tool hook, ACP `session/request_permission` bridge) are gone. yolop was
  already autonomous by default, so default behavior is unchanged.
  - Before: `yolop --ask` gated every tool call behind y/n approval.
  - After: use the soft-approval layer ([#98](https://github.com/everruns/yolop/pull/98)) —
    set `approval_mode` to `protective` via `/setup approval` for
    consent-before-critical-actions behavior.

### What's Changed

* feat(approval): add soft-approval capability with paranoia levels ([#98](https://github.com/everruns/yolop/pull/98)) by @chaliy
* feat(config): schema-described settings with get_config/set_config ([#97](https://github.com/everruns/yolop/pull/97)) by @chaliy
* fix(tui): tolerate unanswered cursor queries, unpin ratatui 0.30.1 ([#96](https://github.com/everruns/yolop/pull/96)) by @chaliy
* chore(maintenance): harden maintenance docs, refresh deps ([#92](https://github.com/everruns/yolop/pull/92)) by @chaliy
* docs(readme): document EVERRUNS_SYSTEM_ALLOWLIST_ENABLED ([#90](https://github.com/everruns/yolop/pull/90)) by @chaliy
* feat(setup): add Opus 4.7/4.8 and 1M ids to anthropic shortlist ([#89](https://github.com/everruns/yolop/pull/89)) by @chaliy
* feat(openrouter): use first-class OpenRouter Responses driver ([#88](https://github.com/everruns/yolop/pull/88)) by @chaliy
* chore(deps): bump everruns crates to 0.10.0 ([#87](https://github.com/everruns/yolop/pull/87)) by @chaliy
* feat(setup): connection status, fast model pick, custom endpoint ([#86](https://github.com/everruns/yolop/pull/86)) by @chaliy
* feat(hooks): support scoped user hooks ([#85](https://github.com/everruns/yolop/pull/85)) by @chaliy
* feat(skills): manage workspace and global skills ([#84](https://github.com/everruns/yolop/pull/84)) by @chaliy
* feat(acp): load persisted sessions ([#83](https://github.com/everruns/yolop/pull/83)) by @chaliy
* feat(models): list provider models live from models APIs ([#82](https://github.com/everruns/yolop/pull/82)) by @chaliy
* fix(model): use raw model ids in setup ([#81](https://github.com/everruns/yolop/pull/81)) by @chaliy
* fix(tui): show resumed history in composer view ([#80](https://github.com/everruns/yolop/pull/80)) by @chaliy
* test(tui): in-process turn/resume tests and shared PTY harness ([#79](https://github.com/everruns/yolop/pull/79)) by @chaliy
* fix(tui): anchor startup composer at bottom ([#78](https://github.com/everruns/yolop/pull/78)) by @chaliy
* fix(tui): survive transient terminal I/O failures in event loop ([#77](https://github.com/everruns/yolop/pull/77)) by @chaliy
* chore(deps): bump agent-client-protocol to 0.14.0 ([#76](https://github.com/everruns/yolop/pull/76)) by @chaliy
* feat(openrouter): support reasoning effort ([#75](https://github.com/everruns/yolop/pull/75)) by @chaliy
* docs(readme): restore demo gif for inline rendering ([#74](https://github.com/everruns/yolop/pull/74)) by @chaliy
* fix(acp): remove approval-looking tool kinds ([#73](https://github.com/everruns/yolop/pull/73)) by @chaliy
* docs(readme): swap demo gif for mp4 video ([#72](https://github.com/everruns/yolop/pull/72)) by @chaliy
* docs(readme): reposition as full coding agent with demo recording ([#71](https://github.com/everruns/yolop/pull/71)) by @chaliy
* feat(models): add Claude Fable 5 to Anthropic model options ([#70](https://github.com/everruns/yolop/pull/70)) by @chaliy
* refactor(approval): remove tool-call approval gating ([#69](https://github.com/everruns/yolop/pull/69)) by @chaliy
* chore(deps): bump everruns to 0.9.0 ([#68](https://github.com/everruns/yolop/pull/68)) by @chaliy
* refactor(acp): use upstream protocol sdk ([#67](https://github.com/everruns/yolop/pull/67)) by @chaliy
* feat(tool-search): vendor provider-agnostic deferred tool loading ([#66](https://github.com/everruns/yolop/pull/66)) by @chaliy
* feat(mcp): MCP server support with approval-gated tool calls ([#65](https://github.com/everruns/yolop/pull/65)) by @chaliy
* fix(openrouter): use Chat Completions so tool calls work ([#64](https://github.com/everruns/yolop/pull/64)) by @chaliy
* chore(deps): bump everruns-* to 0.8.38 ([#63](https://github.com/everruns/yolop/pull/63)) by @chaliy
* chore(deps): bump softprops/action-gh-release from 2 to 3 ([#62](https://github.com/everruns/yolop/pull/62)) by @chaliy
* feat(cli): add version metadata ([#61](https://github.com/everruns/yolop/pull/61)) by @chaliy
* fix(tui): simplify composer newline shortcut ([#60](https://github.com/everruns/yolop/pull/60)) by @chaliy
* docs(commands): add command spec, cover client commands, require tests ([#57](https://github.com/everruns/yolop/pull/57)) by @chaliy
* chore(deps): bump everruns crates to 0.8.37 ([#56](https://github.com/everruns/yolop/pull/56)) by @chaliy
* refactor(commands): make all slash commands capability-based ([#55](https://github.com/everruns/yolop/pull/55)) by @chaliy
* docs(readme): document Homebrew tap trust ([#54](https://github.com/everruns/yolop/pull/54)) by @chaliy
* fix(setup): keep hint separated from overflowing credential label ([#53](https://github.com/everruns/yolop/pull/53)) by @chaliy
* fix(ci): drop redundant version from generated Homebrew formula ([#52](https://github.com/everruns/yolop/pull/52)) by @chaliy
* feat(acp): enrich slash command support ([#51](https://github.com/everruns/yolop/pull/51)) by @chaliy
* feat(cli): add zed acp integration command ([#50](https://github.com/everruns/yolop/pull/50)) by @chaliy

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.2.0...v0.3.0

## [0.2.0] - 2026-06-03

### Highlights

- **Agent Client Protocol support.** yolop now speaks ACP, so it can be
  driven as an agent backend by ACP-compatible editors and clients.
- **Reworked setup onboarding.** A modal overlay picker walks through
  provider, model, and reasoning-effort selection, replacing the older
  flat onboarding flow.
- **Configurable attribution.** Commit attribution is now configurable
  instead of hardcoded, and OpenAI is no longer recommended by default
  during setup.
- **TUI input and rendering polish.** Mac and shifted multiline composer
  shortcuts work correctly, and transcript rendering (including narration
  line labels) is cleaner.

### What's Changed

* feat(acp): add Agent Client Protocol support ([#48](https://github.com/everruns/yolop/pull/48)) by @chaliy
* fix(tui): label narration transcript lines ([#47](https://github.com/everruns/yolop/pull/47)) by @chaliy
* fix(tui): polish transcript rendering ([#46](https://github.com/everruns/yolop/pull/46)) by @chaliy
* fix(tui): support shifted printable input ([#45](https://github.com/everruns/yolop/pull/45)) by @chaliy
* fix(setup): avoid recommending OpenAI ([#44](https://github.com/everruns/yolop/pull/44)) by @chaliy
* test(tui): isolate multiline shortcut test ([#43](https://github.com/everruns/yolop/pull/43)) by @chaliy
* test(tui): isolate multiline composer shortcut ([#42](https://github.com/everruns/yolop/pull/42)) by @chaliy
* feat(setup): make attribution configurable ([#41](https://github.com/everruns/yolop/pull/41)) by @chaliy
* fix(tui): support mac multiline shortcut ([#40](https://github.com/everruns/yolop/pull/40)) by @chaliy
* feat(tui): add setup overlay picker ([#39](https://github.com/everruns/yolop/pull/39)) by @chaliy
* feat(tui): improve setup onboarding flow ([#38](https://github.com/everruns/yolop/pull/38)) by @chaliy

Additional changes landed via direct commits to `main`: modal model and
reasoning-effort setup ([e685a18](https://github.com/everruns/yolop/commit/e685a18ba736e71a8356fd931ec7b9fcf1e5de98)).

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.1.0...v0.2.0

## [0.1.0] - 2026-05-31

First public release of yolop — a minimal terminal coding agent built on
[`everruns-runtime`](https://crates.io/crates/everruns-runtime).

### Highlights

- **Terminal coding agent.** A ratatui-based TUI that drives the everruns
  runtime agent loop, with live streaming of delta events as the model works.
- **Provider setup built in.** `/provider`, `/token`, and `/onboard` commands
  configure OpenAI or Anthropic and persist settings to TOML; OpenAI is the
  default provider, Anthropic the secondary.
- **Session persistence.** Reasoning artifacts and the session log are
  persisted, so sessions can be resumed with `--session`.
- **Skills and personalization.** Skills are sourced from workspace, global,
  and system scopes; a personalization layer adds a central memory surface.
- **Offline smoke testing.** The bundled `llmsim` provider runs the full loop
  with no API key (`yolop --provider llmsim -p "hi"`).

### What's Changed

* chore(maintenance): refresh lockfile and re-enable EVE-489 tests ([#32](https://github.com/everruns/yolop/pull/32)) by @chaliy
* test(agent): scripted llmsim scenario tests for the agent loop ([#31](https://github.com/everruns/yolop/pull/31)) by @chaliy
* chore(deps): bump everruns-* crates to 0.8.36 ([#29](https://github.com/everruns/yolop/pull/29)) by @chaliy
* feat(skills): source skills from workspace, global, and system scopes ([#28](https://github.com/everruns/yolop/pull/28)) by @chaliy
* feat(your): personalization layer with central memory ([#26](https://github.com/everruns/yolop/pull/26)) by @chaliy
* fix(app): pass SettingsStore to build_with_options in TUI test helper ([#25](https://github.com/everruns/yolop/pull/25)) by @chaliy
* chore(claude): SessionStart hook to fix agent-set git identity ([#24](https://github.com/everruns/yolop/pull/24)) by @chaliy
* chore(claude): disable AI attribution in commits and PR bodies ([#22](https://github.com/everruns/yolop/pull/22)) by @chaliy
* refactor(tui): extract ViewState + snapshot-test the render chrome ([#21](https://github.com/everruns/yolop/pull/21)) by @chaliy
* chore(ship): require comments addressed, answered inline, resolved ([#20](https://github.com/everruns/yolop/pull/20)) by @chaliy
* test(session_log): cover replay edge cases for corrupt or partial logs ([#19](https://github.com/everruns/yolop/pull/19)) by @chaliy
* test(integration): cover --session resume and malformed session-id ([#18](https://github.com/everruns/yolop/pull/18)) by @chaliy
* test(tools): cover bash approval-denial and bad-argument paths ([#17](https://github.com/everruns/yolop/pull/17)) by @chaliy
* test(approval,diff): add unit tests for gate semantics and diff helper ([#16](https://github.com/everruns/yolop/pull/16)) by @chaliy
* feat(tui): /provider, /token, /onboard with persisted TOML settings ([#14](https://github.com/everruns/yolop/pull/14)) by @chaliy
* chore(release): add release skill, spec, workflows, Homebrew tap ([#11](https://github.com/everruns/yolop/pull/11)) by @chaliy
* feat(session): persist reasoning artifacts for session restore ([#10](https://github.com/everruns/yolop/pull/10)) by @chaliy
* test(tui): end-to-end streaming tests against llmsim ([#9](https://github.com/everruns/yolop/pull/9)) by @chaliy
* feat(tui): stream live delta events from the runtime ([#3](https://github.com/everruns/yolop/pull/3)) by @chaliy
* chore(ci): add dependabot config for cargo and actions ([#2](https://github.com/everruns/yolop/pull/2)) by @chaliy
* feat: port coding-cli from everruns to standalone yolop project ([#1](https://github.com/everruns/yolop/pull/1)) by @chaliy

Additional changes landed via direct commits to `main`: TUI provider-setup
consolidation, escape-key handling fix, command-suggestion restore, capability
surface simplification, brand/logo assets, and README slimming.
