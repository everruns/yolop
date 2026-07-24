# Changelog

All notable user-visible changes to yolop are recorded here.

The format follows the [release spec](./knowledge/specs/release.md): one section per
released version, newest first, with a `### Highlights` summary, an optional
`### Breaking Changes` block (required for MINOR/MAJOR with breakage), and a
mechanical `### What's Changed` list of merged PRs.

Releases are cut via [`/release`](./.agents/skills/release/SKILL.md), which
tags the version and publishes to crates.io and the Homebrew tap.

## [0.12.0] - 2026-07-24

### Highlights

#### Yolop

- Fullscreen is now the default experience, with a responsive status drawer, an interactive task tree, non-blocking task refresh, and selection that spans transcript windows.
- Extensions can emit trace events, export telemetry to Logfire, and request typed configuration and secrets through the expanded `yolop-yep` SDK and wire protocol.
- MCP OAuth is conversational in both terminal modes, `/tools` updates live, OAuth callbacks are clearer, and tool activity narration is more complete.
- Agent shortcuts now run through Tuika's configurable keymap engine, and skills can be shared from a common agents directory.

#### Tuika

- First-class terminal graphics support now covers Kitty, iTerm2, and Sixel, including streamed images embedded in Markdown.
- A paint-free flexbox solver, terminal capability detection, centralized stylesheets, focus scopes, and an async runner provide stronger foundations for complex apps.
- New components include selectable tables, timelines, diffs, QR codes, framebuffers, image views, and other OpenTUI-parity primitives.
- Scrolling adds horizontal pan and host-controlled offsets; Boxed, Line, Table, TextInput, and Markdown links gain substantial interaction and styling improvements.

### Breaking Changes

- yolop now starts in fullscreen mode by default; use the existing mode controls when the classic inline presentation is preferred.
- Tuika now builds directly on `ratatui-core` rather than the umbrella `ratatui` crate, so applications relying on Tuika's former transitive dependency may need to declare their own Ratatui dependencies.

### What's Changed

* fix(tui): let full-screen selection span more than one window ([#477](https://github.com/everruns/yolop/pull/477)) by @chaliy
* feat(tui): make fullscreen the default ([#476](https://github.com/everruns/yolop/pull/476)) by @chaliy
* feat(tuika): add keymap engine, route yolop shortcuts through it ([#474](https://github.com/everruns/yolop/pull/474)) by @chaliy
* fix(mcp): conversational OAuth and live /tools in both modes ([#473](https://github.com/everruns/yolop/pull/473)) by @chaliy
* fix(tools): add missing activity narration ([#475](https://github.com/everruns/yolop/pull/475)) by @chaliy
* feat(auth): polish OAuth callback page ([#471](https://github.com/everruns/yolop/pull/471)) by @chaliy
* feat(tuika): OpenTUI feature-parity components (timeline, diff, qr, framebuffer…) ([#469](https://github.com/everruns/yolop/pull/469)) by @chaliy
* docs(readme): remove task tree demo ([#472](https://github.com/everruns/yolop/pull/472)) by @chaliy
* chore(knowledge): migrate specs to OKF ([#465](https://github.com/everruns/yolop/pull/465)) by @chaliy
* fix(tui): avoid blocking on task refresh ([#468](https://github.com/everruns/yolop/pull/468)) by @chaliy
* fix(tuika): keep markdown link destinations Ctrl+clickable ([#466](https://github.com/everruns/yolop/pull/466)) by @chaliy
* feat(skills): use shared agents directory ([#467](https://github.com/everruns/yolop/pull/467)) by @chaliy
* chore(docs): document hero recording ([#464](https://github.com/everruns/yolop/pull/464)) by @chaliy
* fix(tuika): honor enter mode and Ctrl+J newline ([#463](https://github.com/everruns/yolop/pull/463)) by @chaliy
* docs(readme): refresh hero demo ([#462](https://github.com/everruns/yolop/pull/462)) by @chaliy
* feat(tui): add interactive task tree ([#460](https://github.com/everruns/yolop/pull/460)) by @chaliy
* feat(tui): add responsive fullscreen status drawer ([#461](https://github.com/everruns/yolop/pull/461)) by @chaliy
* feat(extensions): trace facet, Logfire export, config & secrets ([#459](https://github.com/everruns/yolop/pull/459)) by @chaliy
* refactor(tuika): build on ratatui-core, drop the umbrella ([#458](https://github.com/everruns/yolop/pull/458)) by @chaliy
* feat(tuika): open Table chrome — caret, header style, selection fg ([#457](https://github.com/everruns/yolop/pull/457)) by @chaliy
* feat(tuika): horizontal pan on Scroll + max-offset accessors ([#456](https://github.com/everruns/yolop/pull/456)) by @chaliy
* feat(tuika): centralized stylesheet layer for roles + markdown ([#455](https://github.com/everruns/yolop/pull/455)) by @chaliy
* feat(tuika): add a terminal Capabilities detection subsystem ([#454](https://github.com/everruns/yolop/pull/454)) by @chaliy
* feat(tuika): add async runner behind the "async" feature ([#452](https://github.com/everruns/yolop/pull/452)) by @chaliy
* feat(tuika): columned, selectable Table component ([#453](https://github.com/everruns/yolop/pull/453)) by @chaliy
* feat(tuika): explicit Boxed border color + FocusScope ([#449](https://github.com/everruns/yolop/pull/449)) by @chaliy
* feat(tuika): host-settable ScrollState::set_offset ([#450](https://github.com/everruns/yolop/pull/450)) by @chaliy
* docs(tuika): showcase image rendering in the README ([#451](https://github.com/everruns/yolop/pull/451)) by @chaliy
* feat(tuika): first-class, paint-free flexbox solve ([#448](https://github.com/everruns/yolop/pull/448)) by @chaliy
* feat(tuika): stream markdown image pixels + add Sixel protocol ([#447](https://github.com/everruns/yolop/pull/447)) by @chaliy
* feat(tuika): honor Line alignment and add Boxed bottom title ([#446](https://github.com/everruns/yolop/pull/446)) by @chaliy
* feat(tuika): render images (Kitty + iTerm2, standalone & markdown) ([#445](https://github.com/everruns/yolop/pull/445)) by @chaliy
* chore(deps): bump vt100 from 0.15.2 to 0.16.2 ([#435](https://github.com/everruns/yolop/pull/435)) by @dependabot[bot]
* chore(deps): bump criterion from 0.5.1 to 0.8.2 ([#434](https://github.com/everruns/yolop/pull/434)) by @dependabot[bot]
* chore(deps): bump dorny/paths-filter from 3 to 4 ([#433](https://github.com/everruns/yolop/pull/433)) by @dependabot[bot]
* docs(tuika): document OverlaySpec with example and gallery demo ([#444](https://github.com/everruns/yolop/pull/444)) by @chaliy

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.11.0...v0.12.0

## [0.11.0] - 2026-07-23

### Highlights

#### yolop

- Rebuilt the full-screen TUI on the new `tuika` view tree: a richer multi-line composer, composited setup/ask/background overlays, a full-history transcript that scrolls, and windowed setup/model pickers.
- A `--theme` selector, persisted in settings, with a live theme picker in the full-screen UI.
- Initial Windows support via a PowerShell shell.
- Sandbox modes with approval policies, opt-in shell containment, and session-wide shell approval.
- A mid-turn steering queue, automatic session titles, and durable native context-compaction checkpoints.

#### tuika

- Streaming `Markdown` and syntax-highlighted `CodeBlock` rendering, powered by the new `tuika-codeformatters` tree-sitter highlighter.
- A mouse subsystem — text selection, clicks, hit-testing, and OSC 52 clipboard copy.
- Bundled standard themes and a neutral theme identity independent of yolop.
- New components: windowing `SelectList`, multi-line `TextInput` (emacs keymap, word-wrap), and a `Rule` separator.
- Emoji rendered as grapheme clusters and a configurable OSC 8 link-scheme policy.

### What's Changed

* feat(tuika): add mouse text interactions ([#442](https://github.com/everruns/yolop/pull/442)) by @chaliy
* chore: clarify GitHub auth fallback ([#441](https://github.com/everruns/yolop/pull/441)) by @chaliy
* fix(tui): render startup messages in all modes ([#440](https://github.com/everruns/yolop/pull/440)) by @chaliy
* feat(tui): add mid-turn steering queue ([#439](https://github.com/everruns/yolop/pull/439)) by @chaliy
* fix(tui): capitalize Codex Subscription label ([#438](https://github.com/everruns/yolop/pull/438)) by @chaliy
* feat(tuika): configurable OSC 8 link scheme policy with mailto ([#437](https://github.com/everruns/yolop/pull/437)) by @chaliy
* perf(tui): window the full-screen transcript to O(viewport) per frame ([#436](https://github.com/everruns/yolop/pull/436)) by @chaliy
* feat: add a --theme selector and persist it in settings ([#431](https://github.com/everruns/yolop/pull/431)) by @chaliy
* feat(tuika): bundle standard themes ([#430](https://github.com/everruns/yolop/pull/430)) by @chaliy
* test(tuika): add Scroll render + paging benchmarks ([#429](https://github.com/everruns/yolop/pull/429)) by @chaliy
* fix(evals): treat provider outages as inconclusive, not a regression ([#428](https://github.com/everruns/yolop/pull/428)) by @chaliy
* fix(tuika): cache flattened settled lines so streaming stays linear ([#427](https://github.com/everruns/yolop/pull/427)) by @chaliy
* feat(tuika): style inline markup in markdown table cells ([#426](https://github.com/everruns/yolop/pull/426)) by @chaliy
* test: add render benchmarks + instruction-count regression gate ([#425](https://github.com/everruns/yolop/pull/425)) by @chaliy
* test(tui): de-flake combined-resize composer test ([#424](https://github.com/everruns/yolop/pull/424)) by @chaliy
* docs(readme): restore Origin of the name to the top ([#423](https://github.com/everruns/yolop/pull/423)) by @chaliy
* docs(tuika): add terminal features guide with demos ([#422](https://github.com/everruns/yolop/pull/422)) by @chaliy
* docs(readme): tighten and restructure for readability ([#421](https://github.com/everruns/yolop/pull/421)) by @chaliy
* docs(tuika): fix missing_docs on main + add Flex/Boxed/Select doctests ([#420](https://github.com/everruns/yolop/pull/420)) by @chaliy
* chore(ci): scope PR jobs to what the diff touches ([#419](https://github.com/everruns/yolop/pull/419)) by @chaliy
* fix(tui): bound and correct the full-screen transcript scroll ([#418](https://github.com/everruns/yolop/pull/418)) by @chaliy
* docs(tuika): document public API and enforce missing_docs lint ([#417](https://github.com/everruns/yolop/pull/417)) by @chaliy
* docs: remove docs.rs badge from README ([#416](https://github.com/everruns/yolop/pull/416)) by @chaliy
* refactor(tui): drop redundant OSC 8 buffer pass, keep HyperlinkBackend ([#415](https://github.com/everruns/yolop/pull/415)) by @chaliy
* chore(deps): update everruns to 0.17.16 ([#414](https://github.com/everruns/yolop/pull/414)) by @chaliy
* feat(tuika): streaming Markdown + CodeBlock, adopt in yolop ([#413](https://github.com/everruns/yolop/pull/413)) by @chaliy
* test(tuika): automate terminal matrix, add nightly cross-terminal CI ([#412](https://github.com/everruns/yolop/pull/412)) by @chaliy
* docs(tuika): restructure README from review feedback ([#411](https://github.com/everruns/yolop/pull/411)) by @chaliy
* fix(background): store artifacts in session directory ([#410](https://github.com/everruns/yolop/pull/410)) by @chaliy
* feat(context): persist native compaction checkpoints ([#409](https://github.com/everruns/yolop/pull/409)) by @chaliy
* feat(session): add automatic session titles ([#408](https://github.com/everruns/yolop/pull/408)) by @chaliy
* test(tuika): split monolithic tests.rs into per-module suites ([#407](https://github.com/everruns/yolop/pull/407)) by @chaliy
* feat(sandbox): make shell containment opt-in ([#406](https://github.com/everruns/yolop/pull/406)) by @chaliy
* docs(tuika): add a recorded gallery hero and capture toolchain ([#405](https://github.com/everruns/yolop/pull/405)) by @chaliy
* feat(approval): allow session-wide shell approval ([#403](https://github.com/everruns/yolop/pull/403)) by @chaliy
* feat(context): collect dynamic environment contributions ([#402](https://github.com/everruns/yolop/pull/402)) by @chaliy
* fix(tui): emit OSC 8 hyperlinks so transcript URLs are clickable ([#401](https://github.com/everruns/yolop/pull/401)) by @chaliy
* fix(tui): preserve context in exit message ([#400](https://github.com/everruns/yolop/pull/400)) by @chaliy
* fix(tui): support configurable enter behavior ([#399](https://github.com/everruns/yolop/pull/399)) by @chaliy
* fix(tui): show runtime system notices ([#398](https://github.com/everruns/yolop/pull/398)) by @chaliy
* fix(tui): remove extra spacing from diffs ([#397](https://github.com/everruns/yolop/pull/397)) by @chaliy
* fix(sandbox): allow standard macOS tool writes ([#396](https://github.com/everruns/yolop/pull/396)) by @chaliy
* chore(deps): refresh locked dependencies ([#395](https://github.com/everruns/yolop/pull/395)) by @chaliy
* fix(context): avoid ephemeral default compaction ([#394](https://github.com/everruns/yolop/pull/394)) by @chaliy
* refactor(runtime): extract system prompt ([#393](https://github.com/everruns/yolop/pull/393)) by @chaliy
* feat(cli): add no-sandbox override ([#392](https://github.com/everruns/yolop/pull/392)) by @chaliy
* refactor(prompt): compact the coding-agent kernel ([#391](https://github.com/everruns/yolop/pull/391)) by @chaliy
* test: skip native-sandbox tests when Landlock is unavailable ([#390](https://github.com/everruns/yolop/pull/390)) by @chaliy
* refactor(tui): route setup pickers through SelectState navigation ([#389](https://github.com/everruns/yolop/pull/389)) by @chaliy
* refactor(tui): unify both composers on one tuika TextInputState ([#388](https://github.com/everruns/yolop/pull/388)) by @chaliy
* feat(tuika): word-wrap TextInput at space boundaries ([#387](https://github.com/everruns/yolop/pull/387)) by @chaliy
* feat(tuika): emacs keymap for TextInputState (readline bindings) ([#386](https://github.com/everruns/yolop/pull/386)) by @chaliy
* feat(acp): close v1 gaps — MCP pass-through, modes, permission gate ([#385](https://github.com/everruns/yolop/pull/385)) by @chaliy
* feat(tuika): TextInput scrolls to its cursor within a bounded height ([#384](https://github.com/everruns/yolop/pull/384)) by @chaliy
* docs(tuika): public docs folder + crisp 2x component demos ([#383](https://github.com/everruns/yolop/pull/383)) by @chaliy
* feat(tui): full-screen model picker renders as a windowed SelectList ([#382](https://github.com/everruns/yolop/pull/382)) by @chaliy
* feat(tuika): windowing SelectList (viewport + scrollbar) ([#381](https://github.com/everruns/yolop/pull/381)) by @chaliy
* refactor(tuika): move gallery integrity check into the demo example ([#380](https://github.com/everruns/yolop/pull/380)) by @chaliy
* feat(tui): full-screen setup pickers render as tuika SelectList ([#379](https://github.com/everruns/yolop/pull/379)) by @chaliy
* feat(tuika): tuika owns a neutral theme identity; yolop builds its own ([#378](https://github.com/everruns/yolop/pull/378)) by @chaliy
* feat(tui): full-screen composer owns its state + routes its own keys ([#377](https://github.com/everruns/yolop/pull/377)) by @chaliy
* refactor: group src modules into domain folders ([#376](https://github.com/everruns/yolop/pull/376)) by @chaliy
* feat(tui): full-screen transcript scrolls full history via tuika Scroll ([#375](https://github.com/everruns/yolop/pull/375)) by @chaliy
* docs(tuika): component gallery with VHS demos ([#374](https://github.com/everruns/yolop/pull/374)) by @chaliy
* feat(tui): surface context compaction in transcript and status gauge ([#373](https://github.com/everruns/yolop/pull/373)) by @chaliy
* feat(tui): composite full-screen overlays (setup/ask/background) on tuika ([#372](https://github.com/everruns/yolop/pull/372)) by @chaliy
* feat(tui): rebuild the full-screen composer and chrome on a tuika view tree ([#371](https://github.com/everruns/yolop/pull/371)) by @chaliy
* feat(tuika): multi-line TextInput component ([#370](https://github.com/everruns/yolop/pull/370)) by @chaliy
* feat(tuika): Rule component (titled horizontal separator) ([#369](https://github.com/everruns/yolop/pull/369)) by @chaliy
* feat(tuika): render emoji as grapheme clusters ([#368](https://github.com/everruns/yolop/pull/368)) by @chaliy
* chore(deps): update everruns to 0.17.15 ([#367](https://github.com/everruns/yolop/pull/367)) by @chaliy
* feat(sandbox): add modes and approval policies ([#366](https://github.com/everruns/yolop/pull/366)) by @chaliy
* feat: initial Windows support via PowerShell shell ([#365](https://github.com/everruns/yolop/pull/365)) by @chaliy
* feat(tui): rebuild the full-screen renderer on the tuika view tree ([#364](https://github.com/everruns/yolop/pull/364)) by @chaliy
* feat(tui): mouse text selection + copy in the full-screen transcript ([#363](https://github.com/everruns/yolop/pull/363)) by @chaliy
* docs(tuika): runnable mouse example (drag-select + OSC 52 copy) ([#362](https://github.com/everruns/yolop/pull/362)) by @chaliy
* feat(tuika): mouse subsystem — selection, clicks, hit-testing, OSC 52 copy ([#361](https://github.com/everruns/yolop/pull/361)) by @chaliy

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.10.0...v0.11.0

## [0.10.0] - 2026-07-20

### Highlights

- Add checkpoint rewind and undo for recovering earlier session state.
- Contain shell execution by default, with clearer sandbox denials and temporary-directory support.
- Add live extension hot-enabling and Open Knowledge Format support through the bundled skill.
- Expand the fullscreen TUI with a richer composer, syntax highlighting, OSC 8 hyperlinks, and safer Tuika interoperability.

### What's Changed

* chore: deep maintenance — de-abstraction surface + audit cleanups ([#359](https://github.com/everruns/yolop/pull/359)) by @mchalyi
* docs: move OKF feature doc into its own folder ([#358](https://github.com/everruns/yolop/pull/358)) by @mchalyi
* refactor: skill-only OKF support (drop detection capability) ([#356](https://github.com/everruns/yolop/pull/356)) by @mchalyi
* feat(sandbox): improve denial visibility and temp support ([#357](https://github.com/everruns/yolop/pull/357)) by @mchalyi
* chore(docs): define public documentation boundary ([#355](https://github.com/everruns/yolop/pull/355)) by @mchalyi
* docs: add user-facing OKF feature doc ([#354](https://github.com/everruns/yolop/pull/354)) by @mchalyi
* feat(tuika): add safe ratatui composition foundation ([#353](https://github.com/everruns/yolop/pull/353)) by @mchalyi
* feat: native Open Knowledge Format (OKF) support ([#349](https://github.com/everruns/yolop/pull/349)) by @mchalyi
* feat(tuika): styled-span text wrapping (`wrap_lines` + `Wrap`) ([#351](https://github.com/everruns/yolop/pull/351)) by @mchalyi
* feat(sandbox): contain shell execution by default ([#348](https://github.com/everruns/yolop/pull/348)) by @mchalyi
* feat(tui): demo and end-to-end proof for OSC 8 hyperlinks ([#350](https://github.com/everruns/yolop/pull/350)) by @mchalyi
* feat(tui): opt-in OSC 8 hyperlinks (`tuika` `HyperlinkBackend`) ([#343](https://github.com/everruns/yolop/pull/343)) by @mchalyi
* fix(tui): stop leaked subprocess output from corrupting the TUI ([commit](https://github.com/everruns/yolop/commit/784883e77c2532753e3cd5f836b4c4f507266532)) by @mchalyi
* feat(session): add checkpoint rewind and undo ([#347](https://github.com/everruns/yolop/pull/347)) by @mchalyi
* feat(extensions): hot-enable an extension on the live session ([#345](https://github.com/everruns/yolop/pull/345)) by @mchalyi
* chore(deps): bump everruns to 0.17.14, unpin Rust to 1.94.0 ([#344](https://github.com/everruns/yolop/pull/344)) by @mchalyi
* fix(tools): honor `/workspace` alias in host-path scope resolution ([#342](https://github.com/everruns/yolop/pull/342)) by @mchalyi
* feat(tui): Codex-inspired composer, highlighting, and status features ([#341](https://github.com/everruns/yolop/pull/341)) by @mchalyi
* fix(tui): unify fullscreen presentation ([#340](https://github.com/everruns/yolop/pull/340)) by @mchalyi
* chore(deps): bump everruns to 0.17.13, pin Rust 1.96.0 ([#339](https://github.com/everruns/yolop/pull/339)) by @chaliy

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.9.0...v0.10.0

## [0.9.0] - 2026-07-19

### Highlights

- Extension platform greatly expanded: extensions can now contribute skills,
  slash commands, and a status bar, install toolchain-free from crates.io,
  scaffold themselves (Rust and TypeScript templates), live-reload while
  enabled, and answer `ui/ask` reverse requests.
- New experimental full-screen renderer built on **tuika**, a standalone
  terminal-UI toolkit crate (layout, overlays, focus, motion, `view!` macro)
  extracted with its own version and extensive snapshot/property test coverage.
- `yolop-yep` gained new wire-protocol capabilities (`ui/ask` channel,
  contributed slash commands and status bar, `schema/yep/v1/schema.json`
  emission) and is published as **0.2.0**.
- Hardened extension conformance (`doctor_extension` probe), release tooling
  (tuika published before yolop), and runtime grep/blocklist behavior.

### What's Changed

* chore(release): make yolop-yep publish-clean, document two-crate publish ([#301](https://github.com/everruns/yolop/pull/301)) by @chaliy
* fix(release): allow crates.io registry checks ([#302](https://github.com/everruns/yolop/pull/302)) by @chaliy
* chore(release): yolop-yep 0.1.1 (clean packaging) + docs use cargo add ([#303](https://github.com/everruns/yolop/pull/303)) by @chaliy
* fix(tui): stabilize resize and overlay rendering ([#305](https://github.com/everruns/yolop/pull/305)) by @chaliy
* feat(herdr): add native session integration by @chaliy
* fix(codex): drop unpaired tool-call items by @chaliy
* feat(extensions): add doctor_extension conformance probe ([#308](https://github.com/everruns/yolop/pull/308)) by @chaliy
* feat(extensions): toolchain-free crates.io install ([#309](https://github.com/everruns/yolop/pull/309)) by @chaliy
* feat(yep): emit schema/yep/v1/schema.json for wire payloads ([#310](https://github.com/everruns/yolop/pull/310)) by @chaliy
* chore(deps): bump the cargo-minor-and-patch group across 1 directory with 5 updates ([#304](https://github.com/everruns/yolop/pull/304)) by @dependabot
* fix(ci): raise search-efficiency-eval timeout for slow provider days ([#311](https://github.com/everruns/yolop/pull/311)) by @chaliy
* feat(tui): experimental full-screen renderer (tuika) behind --fullscreen ([#312](https://github.com/everruns/yolop/pull/312)) by @chaliy
* feat(extensions): scaffold_extension for self-authored extensions ([#313](https://github.com/everruns/yolop/pull/313)) by @chaliy
* feat(tui): tuika motion module — spinner, progress, native OSC 9;4 ([#314](https://github.com/everruns/yolop/pull/314)) by @chaliy
* feat(extensions): TypeScript (Node.js) scaffold template ([#315](https://github.com/everruns/yolop/pull/315)) by @chaliy
* feat(extensions): Rust scaffold template (compiled) ([#316](https://github.com/everruns/yolop/pull/316)) by @chaliy
* test(evals): self-writing extension acceptance case ([#317](https://github.com/everruns/yolop/pull/317)) by @chaliy
* feat(tui): declarative view! macro over the tuika builder API ([#319](https://github.com/everruns/yolop/pull/319)) by @chaliy
* chore(deps): bump everruns to 0.17.12, restore real host-path display ([#318](https://github.com/everruns/yolop/pull/318)) by @chaliy
* test(tui): palette/theme coverage for tuika components ([#320](https://github.com/everruns/yolop/pull/320)) by @chaliy
* test(tui): resize/degenerate-size sweeps + fix flex out-of-bounds placement ([#321](https://github.com/everruns/yolop/pull/321)) by @chaliy
* fix(acp): route reasoning summaries as thoughts by @chaliy
* feat(extensions): extension-contributed status bar (status/changed) ([#323](https://github.com/everruns/yolop/pull/323)) by @chaliy
* test(tui): proptest layout/overlay invariants + fix two bounds bugs ([#325](https://github.com/everruns/yolop/pull/325)) by @chaliy
* test(tui): golden snapshot harness for tuika screens ([#326](https://github.com/everruns/yolop/pull/326)) by @chaliy
* test(tui): terminal-variance PTY smoke + manual matrix for tuika ([#327](https://github.com/everruns/yolop/pull/327)) by @chaliy
* refactor(tuika): extract into a standalone crate with its own version ([#328](https://github.com/everruns/yolop/pull/328)) by @chaliy
* fix(runtime): preserve contextual grep through blocklist ([#329](https://github.com/everruns/yolop/pull/329)) by @chaliy
* ci(release): publish the tuika crate before yolop ([#330](https://github.com/everruns/yolop/pull/330)) by @chaliy
* feat(extensions): extension-contributed skills ([#331](https://github.com/everruns/yolop/pull/331)) by @chaliy
* docs(tuika): add interactive select + overlay examples ([#332](https://github.com/everruns/yolop/pull/332)) by @chaliy
* fix(ci): initialize extension skills in runtime test ([#333](https://github.com/everruns/yolop/pull/333)) by @chaliy
* feat(extensions): extension-contributed slash commands ([#334](https://github.com/everruns/yolop/pull/334)) by @chaliy
* feat(extensions): ui/ask reverse-request channel ([#335](https://github.com/everruns/yolop/pull/335)) by @chaliy
* fix(evals): don't count a no-match search as a tool failure ([#336](https://github.com/everruns/yolop/pull/336)) by @chaliy
* feat(extensions): live reload for enabled extension servers ([#337](https://github.com/everruns/yolop/pull/337)) by @chaliy

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.8.0...v0.9.0

## [0.8.0] - 2026-07-14

### Highlights

- Added a complete extension platform with install/enable management, contributed MCP servers, hook subscriptions, dynamic prompts, and the `yolop-yep` protocol SDK.
- Added live MCP server lifecycle management and interactive OAuth login for remote MCP servers.
- Added per-session ACP model selection and clearer TUI narration and version status.
- Improved Codex authentication, background scheduling, progress-cycle detection, and ACP rendering behavior.

### What's Changed

* fix(progress): detect semantic work cycles ([#299](https://github.com/everruns/yolop/pull/299)) by @chaliy
* test(evals): stabilize search efficiency regression gate ([#298](https://github.com/everruns/yolop/pull/298)) by @chaliy
* docs: consolidate contributing guide ([#297](https://github.com/everruns/yolop/pull/297)) by @chaliy
* feat(acp): support per-session model selection ([#296](https://github.com/everruns/yolop/pull/296)) by @chaliy
* fix(background): scope schedule claims to live sessions ([#293](https://github.com/everruns/yolop/pull/293)) by @chaliy
* feat(tui): show version in status ([#295](https://github.com/everruns/yolop/pull/295)) by @chaliy
* fix(evals): restore standalone Cargo workspaces ([#294](https://github.com/everruns/yolop/pull/294)) by @chaliy
* docs(extensions): user guide for setting up and creating extensions ([#292](https://github.com/everruns/yolop/pull/292)) by @chaliy
* fix(auth): keep TUI responsive during Codex login ([#291](https://github.com/everruns/yolop/pull/291)) by @chaliy
* feat(tui): add human narration for task and related tools ([#288](https://github.com/everruns/yolop/pull/288)) by @chaliy
* fix(runtime): open setup when provider credentials missing ([#290](https://github.com/everruns/yolop/pull/290)) by @chaliy
* feat(mcp): interactive OAuth login for remote MCP servers ([#289](https://github.com/everruns/yolop/pull/289)) by @chaliy
* feat(extensions): generate schema/yep/v1/meta.json with drift guard ([#287](https://github.com/everruns/yolop/pull/287)) by @chaliy
* fix(acp): render commentary as agent messages ([#285](https://github.com/everruns/yolop/pull/285)) by @chaliy
* fix(codex): persist refreshed OAuth tokens to settings ([#286](https://github.com/everruns/yolop/pull/286)) by @chaliy
* chore(deps): bump everruns crates to 0.17.9 ([#284](https://github.com/everruns/yolop/pull/284)) by @chaliy
* feat(extensions): yolop-yep protocol crate + server SDK ([#282](https://github.com/everruns/yolop/pull/282)) by @chaliy
* feat(mcp): load/unload MCP servers live without restart ([#283](https://github.com/everruns/yolop/pull/283)) by @chaliy
* fix(acp): classify tool calls by semantic kind ([#281](https://github.com/everruns/yolop/pull/281)) by @chaliy
* feat(extensions): hook subscriptions and dynamic prompt ([#280](https://github.com/everruns/yolop/pull/280)) by @chaliy
* feat(extensions): contributed MCP servers ([#279](https://github.com/everruns/yolop/pull/279)) by @chaliy
* feat(extensions): install/enable management surface ([#278](https://github.com/everruns/yolop/pull/278)) by @chaliy
* fix(acp): preserve prompt images ([#276](https://github.com/everruns/yolop/pull/276)) by @chaliy

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.7.0...v0.8.0

## [0.7.0] - 2026-07-14

### Highlights

- **YEP capability servers (extensions phase 1).** A new extensions
  mechanism lets capability servers plug into yolop, landing the first phase
  of the YEP (Yolop Extension Proposal) design.
- **Free web search tool.** A built-in web search tool is now available to
  the agent, no external provider key required.
- **Authenticated MCP server management.** First-class tooling to add and
  manage authenticated MCP servers, with bearer auth supplied from the
  environment and the groundwork for MCP OAuth.
- **Richer session and client context.** Sessions expose list-friendly
  metadata and the active client UI, and workspace paths now resolve to
  their real locations.
- **More reliable background execution.** Scheduled monitors run locally,
  obsolete monitors are disarmed, and task status and timeout reporting are
  clearer.

### What's Changed

* fix(background): disarm obsolete scheduled monitors ([#274](https://github.com/everruns/yolop/pull/274)) by @chaliy
* feat(extensions): YEP capability servers (phase 1) ([#275](https://github.com/everruns/yolop/pull/275)) by @chaliy
* test(evals): cover stale history grounding ([#268](https://github.com/everruns/yolop/pull/268)) by @chaliy
* fix(codex): preserve structured stream errors ([#273](https://github.com/everruns/yolop/pull/273)) by @chaliy
* fix(acp): reconstruct tool calls on session replay ([#272](https://github.com/everruns/yolop/pull/272)) by @chaliy
* fix(background): clarify task status and timeout ([#271](https://github.com/everruns/yolop/pull/271)) by @chaliy
* chore(specs): add extensions mechanism proposal (YEP) ([#270](https://github.com/everruns/yolop/pull/270)) by @chaliy
* fix(ci): update Node 24 workflow actions ([#269](https://github.com/everruns/yolop/pull/269)) by @chaliy
* fix(runtime): keep activate skill schema visible ([#267](https://github.com/everruns/yolop/pull/267)) by @chaliy
* feat(session): add list-friendly metadata ([#266](https://github.com/everruns/yolop/pull/266)) by @chaliy
* fix(mcp): supply bearer auth from environment ([#265](https://github.com/everruns/yolop/pull/265)) by @chaliy
* feat(context): expose active client UI ([#264](https://github.com/everruns/yolop/pull/264)) by @chaliy
* fix(background): execute local scheduled monitors ([#263](https://github.com/everruns/yolop/pull/263)) by @chaliy
* feat(auth): prepare MCP OAuth foundation ([#262](https://github.com/everruns/yolop/pull/262)) by @chaliy
* fix(codex): strip empty HTML-comment reasoning summary placeholders ([#261](https://github.com/everruns/yolop/pull/261)) by @chaliy
* fix(agent): improve search efficiency ([#260](https://github.com/everruns/yolop/pull/260)) by @chaliy
* fix(tui): mirror session system notices above composer ([#259](https://github.com/everruns/yolop/pull/259)) by @chaliy
* fix(paths): expose real workspace paths ([#258](https://github.com/everruns/yolop/pull/258)) by @chaliy
* chore(deps): bump everruns to 0.17.7 ([#257](https://github.com/everruns/yolop/pull/257)) by @chaliy
* fix(acp): avoid duplicated command hints ([#256](https://github.com/everruns/yolop/pull/256)) by @chaliy
* chore(runtime): update default provider models to latest catalog ([#255](https://github.com/everruns/yolop/pull/255)) by @chaliy
* feat(tools): add free web search ([#253](https://github.com/everruns/yolop/pull/253)) by @chaliy
* feat(mcp): add authenticated server management ([#254](https://github.com/everruns/yolop/pull/254)) by @chaliy

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.6.0...v0.7.0

## [0.6.0] - 2026-07-11

### Highlights

- **Structural code edits with `ast_edit`.** An opt-in ast-grep–backed tool
  performs syntax-aware find-and-replace across a codebase, with a harness
  eval proving the benefit.
- **Authenticated MCP server management.** First-class tools list, add,
  update, remove, and enable/disable MCP servers, persisting to global
  settings and workspace `.mcp.json` with authenticated (env-var bearer)
  setup documented.
- **GPT-5.6 in the model picker.** New GPT-5.6 entries are selectable from
  the runtime model picker.
- **Smarter background waits.** External-event waits are steered toward
  detached watches so long-running turns don't block on foreground polling.
- **Cleaner print mode.** Plain `-p/--print` runs now emit only the final
  assistant output on stdout; banners, progress, and footers move to stderr.

### What's Changed

* fix(tui): anchor inline viewport to terminal bottom reliably ([#250](https://github.com/everruns/yolop/pull/250)) by @chaliy
* feat(ast-grep): add opt-in ast_edit with harness eval proof ([#249](https://github.com/everruns/yolop/pull/249)) by @chaliy
* feat(mcp): manage authenticated servers ([#248](https://github.com/everruns/yolop/pull/248)) by @chaliy
* feat(background): steer external-event waits to detached watches ([#247](https://github.com/everruns/yolop/pull/247)) by @chaliy
* docs(attribution): update PR footer text ([#246](https://github.com/everruns/yolop/pull/246)) by @chaliy
* chore: guide agents to narrow after repo map ([#245](https://github.com/everruns/yolop/pull/245)) by @chaliy
* feat(runtime): add GPT-5.6 model picker entries ([#244](https://github.com/everruns/yolop/pull/244)) by @chaliy
* chore(deps): bump everruns runtime ([#243](https://github.com/everruns/yolop/pull/243)) by @chaliy
* chore(deps): bump everruns runtime ([#242](https://github.com/everruns/yolop/pull/242)) by @chaliy
* fix(print): emit only final output ([#240](https://github.com/everruns/yolop/pull/240)) by @chaliy
* chore(deps): bump the cargo-minor-and-patch group across 1 directory with 9 updates ([#238](https://github.com/everruns/yolop/pull/238)) by @dependabot

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.5.0...v0.6.0

## [0.5.0] - 2026-07-09

### Highlights

- **Optional LSP navigation.** An opt-in language-server capability adds
  semantic go-to-definition, references, and hover for supported languages,
  with an isolated eval study to measure the benefit.
- **Progress guard.** The runtime now detects stalled investigations and
  escalates with targeted nudges so long-running turns do not spin silently.
- **Background execution on everruns.** Background work is consolidated onto
  the everruns session task model, simplifying sub-agent lifecycle and
  status reporting.
- **Trajectory export.** `--trajectory-out` writes ATIF trajectory files for
  offline analysis and benchmarking.
- **Eval and integration polish.** A `harness_basic` study A/Bs yolop harness
  configs, the Paseo `Into` target lands, and the web UI surfaces failed tool
  errors instead of swallowing them.

### What's Changed

* chore: standardize PR descriptions on functional change + before/after ([#237](https://github.com/everruns/yolop/pull/237)) by @chaliy
* feat(lsp): optional language-server capability with isolated eval ([#236](https://github.com/everruns/yolop/pull/236)) by @chaliy
* docs(readme): add project badges ([#235](https://github.com/everruns/yolop/pull/235)) by @chaliy
* feat(progress-guard): escalate stalled investigations ([#234](https://github.com/everruns/yolop/pull/234)) by @chaliy
* feat(cli): add --trajectory-out ATIF trajectory export ([#233](https://github.com/everruns/yolop/pull/233)) by @chaliy
* feat(runtime): add progress guard ([#232](https://github.com/everruns/yolop/pull/232)) by @chaliy
* feat(evals): add harness_basic study for yolop config A/Bs ([#231](https://github.com/everruns/yolop/pull/231)) by @chaliy
* feat(background): consolidate background execution onto everruns ([#230](https://github.com/everruns/yolop/pull/230)) by @chaliy
* fix(web): surface failed tool errors ([#229](https://github.com/everruns/yolop/pull/229)) by @chaliy
* chore(deps): bump everruns to 0.17.4 ([#228](https://github.com/everruns/yolop/pull/228)) by @chaliy
* feat(into): add paseo target ([#227](https://github.com/everruns/yolop/pull/227)) by @chaliy
* chore(deps): upgrade dependencies ([#226](https://github.com/everruns/yolop/pull/226)) by @chaliy
* docs(release): require post-merge publish verification ([#225](https://github.com/everruns/yolop/pull/225)) by @chaliy
* refactor(skills): hide bundled system skills ([#224](https://github.com/everruns/yolop/pull/224)) by @chaliy

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.4.0...v0.5.0

## [0.4.0] - 2026-07-04

### Highlights

- **Autonomous background work.** Background execution grew from a core task
  runner into sub-agents, proactive wakeups, a read-only Ctrl+B panel,
  status-bar visibility, task caps, model overrides, and cost-efficiency
  reporting.
- **Richer coding context.** yolop now includes multi-language repo maps,
  ranked symbol search, structural ast-grep search, structured global memory,
  project instruction loading, and hidden-file handling improvements.
- **Worktree and session polish.** Native git worktree support, `/worktree`,
  ignored-file copying via `.worktreeinclude`, resume-root restoration, and
  compact provider/worktree status make long-running development loops easier
  to follow.
- **More capable prompts and TUI.** Users can attach images from CLI flags or
  clipboard paste, attach large text pastes as placeholders, invoke commands
  from prompts, use `/help`, and rely on sturdier exit/cancel/layout behavior.
- **Evaluation and runtime refresh.** The release adopts newer everruns
  runtimes, everruns-local backends, ACP 1.0 dependencies, Mira-based eval
  studies, and a containerized SWE-bench Verified harness.

### What's Changed

* chore(deps): bump the cargo-minor-and-patch group with 4 updates ([#222](https://github.com/everruns/yolop/pull/222)) by @dependabot[bot]
* feat(evals): run yolop inside the SWE-bench container ([#221](https://github.com/everruns/yolop/pull/221)) by @chaliy
* chore(evals): bump mira-eval to 0.3.0; adopt case terms and configs ([#220](https://github.com/everruns/yolop/pull/220)) by @chaliy
* feat(runtime): adopt MountFs and WorkspaceHost on 0.17.1 ([`b7594e0`](https://github.com/everruns/yolop/commit/b7594e07b229c521554de2a7d375645703bbbd5b)) by @chaliy
* feat(runtime): bump everruns to 0.17.1 and adopt set_host_root ([#219](https://github.com/everruns/yolop/pull/219)) by @chaliy
* revert: restore Related diagrams section on repo-map page ([#217](https://github.com/everruns/yolop/pull/217)) by @chaliy
* fix(evals): count codex iterations from per-round-trip events ([#216](https://github.com/everruns/yolop/pull/216)) by @chaliy
* fix(tui): drain exit keys after redraw failures ([`731242c`](https://github.com/everruns/yolop/commit/731242c7182613a0bb9258c8a6f8543bc84b5a7f)) by @chaliy
* feat(background): show everruns session tasks ([`77f5694`](https://github.com/everruns/yolop/commit/77f56941659b156f631af26196256e0460bd0525)) by @chaliy
* chore(deps): bump everruns runtime crates to 0.17.0 ([#212](https://github.com/everruns/yolop/pull/212)) by @chaliy
* docs(repo-map): drop Related diagrams section from feature page ([#213](https://github.com/everruns/yolop/pull/213)) by @chaliy
* chore(deps): bump ast-grep-core and ast-grep-language to 0.44 ([#211](https://github.com/everruns/yolop/pull/211)) by @chaliy
* chore(deps): bump agent-client-protocol from 0.15.0 to 1.0.0 ([#207](https://github.com/everruns/yolop/pull/207)) by @dependabot[bot]
* chore(deps): bump actions/checkout from 5 to 7 ([#206](https://github.com/everruns/yolop/pull/206)) by @dependabot[bot]
* feat(evals): astropy-12907-compare preset + image-based checkout ([#210](https://github.com/everruns/yolop/pull/210)) by @chaliy
* fix(tui): extend Ctrl+C exit grace to 5 seconds ([#205](https://github.com/everruns/yolop/pull/205)) by @chaliy
* feat(user-ask): track user requests and validate after turns ([#190](https://github.com/everruns/yolop/pull/190)) by @chaliy
* chore(evals): adopt mira 0.2.0 launchers; bump mira-eval to 0.2.0 ([#203](https://github.com/everruns/yolop/pull/203)) by @chaliy
* fix(tools): repo_map /workspace + hidden-skip; document stateless bash ([#202](https://github.com/everruns/yolop/pull/202)) by @chaliy
* feat: cut gpt-5.5 harness overhead (edit_file + prompt steering) ([#201](https://github.com/everruns/yolop/pull/201)) by @chaliy
* refactor(evals): adopt mira-eval SDK; install mira from registries ([#200](https://github.com/everruns/yolop/pull/200)) by @chaliy
* chore(deps): bump everruns crates to 0.16.2 ([#199](https://github.com/everruns/yolop/pull/199)) by @chaliy
* chore(deps): bump everruns crates to 0.16.1 ([#198](https://github.com/everruns/yolop/pull/198)) by @chaliy
* refactor(evals): single source of truth for swebench target id ([#196](https://github.com/everruns/yolop/pull/196)) by @chaliy
* refactor(evals): Mira-driven single-file SWE-bench Verified study ([`497c2fc`](https://github.com/everruns/yolop/commit/497c2fcc1196a1d0703ba097a2420c207bf0fd5c)) by @chaliy
* feat(runtime): enable spawn_background for bash ([#194](https://github.com/everruns/yolop/pull/194)) by @chaliy
* feat(runtime): adopt everruns-local backends ([`ee4d90c`](https://github.com/everruns/yolop/commit/ee4d90cd9da9d27722c15f75a64e22fff8f4a570)) by @chaliy
* feat(tui): add testable presentation model ([`e28670f`](https://github.com/everruns/yolop/commit/e28670fbdba0539accec61af14a287a7d68548c9)) by @chaliy
* fix(acp): align schema imports with ACP 0.15 ([`cd933c2`](https://github.com/everruns/yolop/commit/cd933c26bdf9335a0934482449376dee26cba9f0)) by @chaliy
* feat(tui): show worktree in status bar with lighter switch notice ([#188](https://github.com/everruns/yolop/pull/188)) by @chaliy
* fix(prompt): require empirical verification ([`0282517`](https://github.com/everruns/yolop/commit/02825174fb199126482cd25c6639bbcec468a709)) by @chaliy
* feat(tui): attach large text pastes as placeholders ([#187](https://github.com/everruns/yolop/pull/187)) by @chaliy
* docs: group repo-map doc with its assets in a subfolder ([#186](https://github.com/everruns/yolop/pull/186)) by @chaliy
* feat(background): sub-agent model override + cost-efficiency report ([`24ebd0f`](https://github.com/everruns/yolop/commit/24ebd0fa8c7b59c0779693bd0d109d98c85f8a27)) by @chaliy
* docs: add yolop name origin and approvals guide ([`065c5a3`](https://github.com/everruns/yolop/commit/065c5a38bde2b631e1c13fee8ce5376c80f63bbc)) by @chaliy
* feat(repo-map): rank query results and add 7 grammars ([#181](https://github.com/everruns/yolop/pull/181)) by @chaliy
* chore(deps): bump everruns runtime to 0.15.0 ([#183](https://github.com/everruns/yolop/pull/183)) by @chaliy
* fix(runtime): require verification before finishing edits ([`38b6097`](https://github.com/everruns/yolop/commit/38b60977c7c4e84c383cd506202b5ef3db454688)) by @chaliy
* feat(bench): representative tracking suite + durable results ([#180](https://github.com/everruns/yolop/pull/180)) by @chaliy
* feat(tui): add human-readable narration for yolop tools ([#173](https://github.com/everruns/yolop/pull/173)) by @chaliy
* feat(tui): add /help command with command list and keyboard shortcuts ([`c9c6b6a`](https://github.com/everruns/yolop/commit/c9c6b6afd884a28842f1b04116b5545e08d2da73)) by @chaliy
* fix(client-commands): reflow CLIENT_COMMANDS_PROMPT to fit 80 columns ([`195d32e`](https://github.com/everruns/yolop/commit/195d32e08b0ad6f3ac393f696f0abb660ed02c8f)) by @chaliy
* refactor(narration): move tool labels to capability narrate() ([`3e72b97`](https://github.com/everruns/yolop/commit/3e72b973763a8ea1659c1334c5a5fa6d059015b5)) by @chaliy
* chore(deps): update Rust dependencies ([#167](https://github.com/everruns/yolop/pull/167)) by @chaliy
* docs(cli): clarify --reasoning-effort applies to all providers ([#177](https://github.com/everruns/yolop/pull/177)) by @chaliy
* refactor(runtime): single canonical Provider enum for identity ([#179](https://github.com/everruns/yolop/pull/179)) by @chaliy
* refactor(runtime): typed capability ids + declarative deps ([#178](https://github.com/everruns/yolop/pull/178)) by @chaliy
* fix(tui): re-anchor inline viewport after terminal resize ([#175](https://github.com/everruns/yolop/pull/175)) by @chaliy
* refactor(capabilities): rename your to yolop framing layer ([#176](https://github.com/everruns/yolop/pull/176)) by @chaliy
* refactor(app): introduce Session facade + transcript boundary ([#174](https://github.com/everruns/yolop/pull/174)) by @chaliy
* feat(tui): render markdown tables with comfy-table ([#163](https://github.com/everruns/yolop/pull/163)) by @chaliy
* fix(goal): pause continuation on turn cancel ([`2c20d12`](https://github.com/everruns/yolop/commit/2c20d123abdd02b708432218bf6451df7db95f51)) by @chaliy
* fix(runtime): order volatile prompt context last ([`6cab4a1`](https://github.com/everruns/yolop/commit/6cab4a1398cf00a8070a9402872cfb0281f2a7f4)) by @chaliy
* refactor(runtime): read only AGENTS.md for project instructions ([#169](https://github.com/everruns/yolop/pull/169)) by @chaliy
* feat(bench): yoloeval harness for benchmarking yolop and peer agents ([#166](https://github.com/everruns/yolop/pull/166)) by @chaliy
* refactor(attribution): extract AttributionCapability into its own module ([#168](https://github.com/everruns/yolop/pull/168)) by @chaliy
* feat(setup): live model/provider/effort tools + delete_skill ([#165](https://github.com/everruns/yolop/pull/165)) by @chaliy
* chore(deps): bump sha2 from 0.10.9 to 0.11.0 ([#154](https://github.com/everruns/yolop/pull/154)) by @dependabot[bot]
* chore(deps): bump rand from 0.9.4 to 0.10.1 ([#155](https://github.com/everruns/yolop/pull/155)) by @dependabot[bot]
* feat(worktree): copy ignored local files via .worktreeinclude ([#164](https://github.com/everruns/yolop/pull/164)) by @chaliy
* refactor(narration): drop yolop fallback, narrate in capabilities ([#162](https://github.com/everruns/yolop/pull/162)) by @chaliy
* fix(model): derive effort from model profiles ([`bcbdb3d`](https://github.com/everruns/yolop/commit/bcbdb3db8fc71e9ae7b54fce23a8b69385245603)) by @chaliy
* fix(runtime): keep recent tool output window ([`78d517a`](https://github.com/everruns/yolop/commit/78d517ac6300d5a7c97bf60a85a18b2393401449)) by @chaliy
* feat(skills): add ast-grep structural-search skill ([#158](https://github.com/everruns/yolop/pull/158)) by @chaliy
* feat(worktree): /worktree command and prune CLI ([#157](https://github.com/everruns/yolop/pull/157)) by @chaliy
* feat(session): native git worktree support ([#156](https://github.com/everruns/yolop/pull/156)) by @chaliy
* refactor(skills): use upstream ScopedSkillsCapability, drop vendored copy ([#153](https://github.com/everruns/yolop/pull/153)) by @chaliy
* feat: bump everruns to 0.14.0 with OpenRouter attribution ([#152](https://github.com/everruns/yolop/pull/152)) by @chaliy
* fix(prompt): address run_yolop_command review comments ([#111](https://github.com/everruns/yolop/pull/111)) by @chaliy
* fix(ui): improve fallback tool narration ([#149](https://github.com/everruns/yolop/pull/149)) by @chaliy
* fix(tui): add missing horizontal bounds check for status bar mouse clicks ([#150](https://github.com/everruns/yolop/pull/150)) by @chaliy
* fix(prompt): split tool guidance sections ([`537f0b7`](https://github.com/everruns/yolop/commit/537f0b761957c4754ed8b1e4f858f2f238754032)) by @chaliy
* fix(tui): tighten idle layout spacing ([`ece0419`](https://github.com/everruns/yolop/commit/ece04198d518e2be44ca51dfc3ef80b7f8daf873)) by @chaliy
* fix(runtime): keep todo schema loaded ([`adf7e5f`](https://github.com/everruns/yolop/commit/adf7e5f4384aeaa224b08a83eba0aa5e837f4a34)) by @chaliy
* feat(tui): paste clipboard images with Ctrl+V ([#145](https://github.com/everruns/yolop/pull/145)) by @chaliy
* feat(cli): attach images to prompts via --image/-i ([#133](https://github.com/everruns/yolop/pull/133)) by @chaliy
* feat(goal): add autonomous /goal completion loops ([#134](https://github.com/everruns/yolop/pull/134)) by @chaliy
* feat(background): read-only Ctrl+B background-tasks panel ([#144](https://github.com/everruns/yolop/pull/144)) by @chaliy
* feat(background): proactive_wake setting and concurrent-task cap ([#143](https://github.com/everruns/yolop/pull/143)) by @chaliy
* feat(background): proactive wake when a task finishes ([#142](https://github.com/everruns/yolop/pull/142)) by @chaliy
* feat(background): TUI status bar count and /background command ([#141](https://github.com/everruns/yolop/pull/141)) by @chaliy
* feat(background): add background sub-agents ([#140](https://github.com/everruns/yolop/pull/140)) by @chaliy
* feat(background): background execution core with scripted tasks ([#139](https://github.com/everruns/yolop/pull/139)) by @chaliy
* feat(ast-grep): add structural search tool ([`f5b47e4`](https://github.com/everruns/yolop/commit/f5b47e44cd4153855ca20c4f3953fe7337ca8f82)) by @chaliy
* feat(repo-map): add multi-language symbol map ([`6267d9a`](https://github.com/everruns/yolop/commit/6267d9afb6c436eef9f649a0b7465f2844e2d3bd)) by @chaliy
* chore: add keywords, categories, authors to crate metadata ([#136](https://github.com/everruns/yolop/pull/136)) by @chaliy
* fix(tui): harden layout and text selection ([`e4d6ee7`](https://github.com/everruns/yolop/commit/e4d6ee7520a3cc2b98dfd5053c098e1321a0d8f2)) by @chaliy
* feat(tui): show provider in compact status ([`fb23052`](https://github.com/everruns/yolop/commit/fb2305221038dcc8c30bd66d9d3ec5414819daba)) by @chaliy
* chore(deps): refresh cargo lockfile ([`1c3028e`](https://github.com/everruns/yolop/commit/1c3028e072b04e41ed72d1c4e9a2b34fca3c1fbe)) by @chaliy
* fix(config): safe provider/model resolution on switch ([#128](https://github.com/everruns/yolop/pull/128)) by @chaliy
* feat(codex): add subscription provider ([`f15d42f`](https://github.com/everruns/yolop/commit/f15d42f4a8eb476589c67e3abb46c0fec7a755a7)) by @chaliy
* feat(app): expand session status bar ([#129](https://github.com/everruns/yolop/pull/129)) by @chaliy
* feat(openrouter): rank model picker with recommended section ([#127](https://github.com/everruns/yolop/pull/127)) by @chaliy
* fix(session): restore workspace root on resume ([`a9989d8`](https://github.com/everruns/yolop/commit/a9989d8bb6afce8a8da1a63b7909203d272bde73)) by @chaliy
* feat(runtime): send yolop metadata to everruns ([`b9770f5`](https://github.com/everruns/yolop/commit/b9770f5bccba441536f7925378a5865a406d5278)) by @chaliy
* chore(deps): bump everruns family to 0.13.0 ([#124](https://github.com/everruns/yolop/pull/124)) by @chaliy
* fix(app): cancel turns with double escape ([`867f12d`](https://github.com/everruns/yolop/commit/867f12dd7cfffebb9645fbc102712ff05c260055)) by @chaliy
* fix(app): render compact write_todos transcript ([`9604a59`](https://github.com/everruns/yolop/commit/9604a596561ee89988d09da5d3e72e408da0988b)) by @chaliy
* chore(deps): bump everruns family to 0.12.0 ([#120](https://github.com/everruns/yolop/pull/120)) by @chaliy
* refactor(hooks): move tools into hooks capability ([`54c4e95`](https://github.com/everruns/yolop/commit/54c4e95554315109ebc3aa2b62358582903daccf)) by @chaliy
* feat(connectors): Daytona sandbox and generic connectors ([#117](https://github.com/everruns/yolop/pull/117)) by @chaliy
* feat(config): add harness capability settings in TOML ([#116](https://github.com/everruns/yolop/pull/116)) by @chaliy
* feat(memory): structured global memory with progressive disclosure ([#118](https://github.com/everruns/yolop/pull/118)) by @chaliy
* feat(btw): enable upstream /btw ephemeral side-question command ([#115](https://github.com/everruns/yolop/pull/115)) by @chaliy
* refactor: use upstream tool_search capability, drop vendored copy ([#114](https://github.com/everruns/yolop/pull/114)) by @chaliy
* chore(deps): bump everruns crates to 0.11.0 ([#113](https://github.com/everruns/yolop/pull/113)) by @chaliy
* feat(commands): add direct shell command ([`feed4a6`](https://github.com/everruns/yolop/commit/feed4a6b4cf52d4ef589988cda4c05df412d39cb)) by @chaliy
* fix(approval): keep approval tools fully loaded ([`8642596`](https://github.com/everruns/yolop/commit/864259608ca8be84f3795106a49afe3cb1f7fba3)) by @chaliy
* feat(tui): add shell command alias ([`a2f40cb`](https://github.com/everruns/yolop/commit/a2f40cb78a9ebec9cccab5252465b8e72d2fe84c)) by @chaliy
* fix(tui): stop mirroring flushed transcript lines ([`3e51b50`](https://github.com/everruns/yolop/commit/3e51b50f045d6a14e2c0ff8f550297fcbb6e48d6)) by @chaliy
* Let prompts invoke TUI commands ([#108](https://github.com/everruns/yolop/pull/108)) by @chaliy
* feat(app): require double Ctrl+C to exit TUI ([`70cd66e`](https://github.com/everruns/yolop/commit/70cd66e418f45fe1de19361c7de1ed5bf4f13ef6)) by @chaliy
* fix(app): grow composer height when wrapped input wraps ([#106](https://github.com/everruns/yolop/pull/106)) by @chaliy
* fix(tool-search): defer MCP tool schemas like the long tail ([#104](https://github.com/everruns/yolop/pull/104)) by @chaliy
* refactor(app): split app.rs into mod/setup/transcript/render modules ([#103](https://github.com/everruns/yolop/pull/103)) by @chaliy
* chore(ci): bump codecov-action v4→v7, set fail_ci_if_error conditional ([#102](https://github.com/everruns/yolop/pull/102)) by @chaliy
* refactor(setup): read config through ConfigService ([#101](https://github.com/everruns/yolop/pull/101)) by @chaliy
* refactor(config): rename provider key, slim ConfigService ([#99](https://github.com/everruns/yolop/pull/99)) by @chaliy

**Full Changelog**: https://github.com/everruns/yolop/compare/v0.3.0...v0.4.0

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
