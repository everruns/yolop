# Changelog

All notable user-visible changes to yolop are recorded here.

The format follows the [release spec](./specs/release.md): one section per
released version, newest first, with a `### Highlights` summary, an optional
`### Breaking Changes` block (required for MINOR/MAJOR with breakage), and a
mechanical `### What's Changed` list of merged PRs.

Releases are cut via [`/release`](./.agents/skills/release/SKILL.md), which
tags the version and publishes to crates.io and the Homebrew tap.

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
