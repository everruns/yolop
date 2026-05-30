# Changelog

All notable user-visible changes to yolop are recorded here.

The format follows the [release spec](./specs/release.md): one section per
released version, newest first, with a `### Highlights` summary, an optional
`### Breaking Changes` block (required for MINOR/MAJOR with breakage), and a
mechanical `### What's Changed` list of merged PRs.

The first tagged release will be cut via [`/release`](./.agents/skills/release/SKILL.md);
until then, the `Cargo.toml` version sits at `0.1.0` and unreleased changes
accumulate below.

## [Unreleased]

### What's Changed

* chore(deps): bump everruns-* crates to 0.8.36
* chore(release): add release skill, spec, workflows, and Homebrew tap publishing
