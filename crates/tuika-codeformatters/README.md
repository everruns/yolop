# tuika-codeformatters

[![crates.io](https://img.shields.io/crates/v/tuika-codeformatters.svg)](https://crates.io/crates/tuika-codeformatters)
[![docs.rs](https://img.shields.io/docsrs/tuika-codeformatters)](https://docs.rs/tuika-codeformatters)

Tree-sitter syntax highlighting for [`tuika`](https://crates.io/crates/tuika)'s
`CodeBlock` and `Markdown` components.

`tuika` owns the *presentation* of code — framing, background, language label,
wrapping — but deliberately depends on no grammar, so its dependency set stays
small. This companion crate fills the gap: a ready-made `tuika::Highlighter`
backed by tree-sitter grammars, mapping token classes onto the host `Theme`'s
`code` palette so highlighted code follows the theme.

```rust
use tuika::{CodeBlock, Theme};
use tuika_codeformatters::TreeSitterHighlighter;

let theme = Theme::default();
let hl = TreeSitterHighlighter::new();
let block = CodeBlock::new("rust", "fn main() {}").highlighter(&hl);
// `block` is a `View`; render it with `tuika::paint` or embed it in a `Flex`.
# let _ = (theme, block);
```

## Example

A runnable, interactive gallery of highlighted snippets across languages
(←/→ or Tab to switch, `q` to quit):

```bash
cargo run -p tuika-codeformatters --example languages
```

<img src="https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika-codeformatters/docs/languages.gif" width="880" alt="Syntax highlighting across languages">

## Supported languages

Rust, Python, TypeScript/JavaScript, TSX/JSX, Go, Java, Ruby, CSS, HTML, C#,
PHP, Zig, Scala, and SQL (with common aliases: `rs`, `py`, `ts`, `js`, `rb`,
`c#`, …). Unknown languages — or source that fails to parse — return `None`, and
the caller renders the block as plain, theme-colored code.

## Compatibility

`ratatui` and `tuika` are part of this crate's public interface, so pin the same
minor versions in your own crate and Cargo will deduplicate them.

## License

MIT
