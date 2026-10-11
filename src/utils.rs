//! Deterministic local helpers for scripts and agents (`yolop utils ...`).
//!
//! `yolop utils rg` is a line oriented, UTF-8 only text search with an `rg`
//! compatible subset of flags. It walks with the same `ignore` rules the
//! agent search tool uses, so scripted results match interactive ones, and it
//! is always installed, unlike an external `rg` binary.
//!
//! Deliberately out of scope for v1: multiline matches (`-U`), PCRE2
//! (`-P`), JSON output (`--json`), file type aliases (`-t`), and binary
//! files (a file with a NUL byte is skipped). Anything outside the supported
//! subset should fall back to a real `rg` install.
//!
//! The same engine backs the `rg` model tool. Model output is bounded on
//! purpose: `max_lines` caps total emitted lines, `max_bytes` total bytes
//! (whole lines only, never torn mid-line), and `max_columns` truncates long
//! content lines. Any cut sets `truncated` so callers know the results are
//! partial. The CLI leaves the line and byte caps unset, like `rg`.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Exit codes follow `rg`: 0 matches found, 1 no match, 2 error.
pub const EXIT_MATCH: i32 = 0;
pub const EXIT_NO_MATCH: i32 = 1;
pub const EXIT_ERROR: i32 = 2;

/// Plain options for the search engine. The clap layer in `main.rs` maps the
/// CLI flags onto this struct, the `rg` model tool maps its JSON arguments,
/// and the unit tests below build it directly.
#[derive(Debug, Clone, Default)]
pub struct RgOptions {
    /// Search patterns: positional pattern plus every `-e/--regexp` value,
    /// ORed together.
    pub patterns: Vec<String>,
    /// Roots to search. Empty means the current directory.
    pub paths: Vec<PathBuf>,
    /// List every file that would be searched instead of searching.
    pub files: bool,
    pub fixed_strings: bool,
    pub ignore_case: bool,
    pub smart_case: bool,
    pub case_sensitive: bool,
    pub word: bool,
    pub line_regexp: bool,
    pub invert_match: bool,
    /// `-g/--glob` filters in gitignore syntax.
    pub globs: Vec<String>,
    pub hidden: bool,
    pub no_ignore: bool,
    pub follow: bool,
    pub max_depth: Option<usize>,
    /// `-u` count: once disables ignore files, twice also shows hidden files.
    pub unrestricted: u8,
    pub files_with_matches: bool,
    pub count: bool,
    pub quiet: bool,
    /// Stop each file after this many matching lines.
    pub max_count: Option<u64>,
    pub before: usize,
    pub after: usize,
    pub line_number: bool,
    pub column: bool,
    pub vimgrep: bool,
    pub with_filename: bool,
    pub no_filename: bool,
    /// NUL separator for the list modes (`--files`, `-l`, `--count`).
    pub null: bool,
    /// Stop emitting after this many total output lines (model bounding).
    pub max_lines: Option<u64>,
    /// Stop emitting after this many total output bytes (model bounding).
    pub max_bytes: Option<u64>,
    /// Truncate printed content lines to this many characters.
    pub max_columns: Option<usize>,
}

/// Totals for one `run_rg` call, used to derive the process exit code.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RgStats {
    /// Files with at least one match (or files listed in `--files` mode).
    pub files_with_hits: u64,
    /// Matching lines printed or counted across all files.
    pub matching_lines: u64,
    /// True when a bound cut the output, so the counts are lower bounds.
    pub truncated: bool,
}

impl RgStats {
    pub fn matched(&self) -> bool {
        self.files_with_hits > 0
    }
}

enum Root {
    File(PathBuf),
    Dir(PathBuf),
}

fn effective_hidden(opts: &RgOptions) -> bool {
    opts.hidden || opts.unrestricted >= 2
}

fn effective_no_ignore(opts: &RgOptions) -> bool {
    opts.no_ignore || opts.unrestricted >= 1
}

fn effective_insensitive(opts: &RgOptions) -> bool {
    if opts.case_sensitive {
        return false;
    }
    if opts.ignore_case {
        return true;
    }
    if opts.smart_case {
        return !opts
            .patterns
            .iter()
            .any(|p| p.chars().any(char::is_uppercase));
    }
    false
}

fn build_regex(opts: &RgOptions) -> Result<regex::Regex> {
    let mut alternates = Vec::with_capacity(opts.patterns.len());
    for pattern in &opts.patterns {
        let atom = if opts.fixed_strings {
            regex::escape(pattern)
        } else {
            pattern.clone()
        };
        alternates.push(format!("(?:{atom})"));
    }
    let mut source = alternates.join("|");
    if opts.word {
        source = format!(r"\b(?:{source})\b");
    }
    if opts.line_regexp {
        source = format!(r"^(?:{source})$");
    }
    regex::RegexBuilder::new(&source)
        .case_insensitive(effective_insensitive(opts))
        .build()
        .with_context(|| format!("invalid pattern: {}", opts.patterns.join(", ")))
}

/// Gitignore semantics for `-g/--glob`: patterns without a slash match the
/// file name, later patterns override earlier ones, `!` negates. Returns the
/// matcher plus whether any pattern is non-negated (a lone `!` list still
/// searches everything else).
fn build_glob_matcher(
    root: &Path,
    globs: &[String],
) -> Result<Option<(ignore::gitignore::Gitignore, bool)>> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    let mut has_positive = false;
    for glob in globs {
        if !glob.starts_with('!') {
            has_positive = true;
        }
        builder
            .add_line(None, glob)
            .with_context(|| format!("invalid glob: {glob}"))?;
    }
    let matcher = builder.build().context("failed to build glob matcher")?;
    Ok(Some((matcher, has_positive)))
}

fn glob_passes(matcher: &Option<(ignore::gitignore::Gitignore, bool)>, path: &Path) -> bool {
    let Some((matcher, has_positive)) = matcher else {
        return true;
    };
    // The gitignore matcher reports how the filter list treats the path:
    // `Ignore` hit a positive `-g` glob (search it), `Whitelist` hit a `!`
    // negation (skip it).
    match matcher.matched(path, false) {
        ignore::Match::None => !has_positive,
        ignore::Match::Ignore(_) => true,
        ignore::Match::Whitelist(_) => false,
    }
}

/// Bounded output sink: at most `lines` emitted lines and `bytes` bytes.
/// Lines are atomic, a line is either fully written or dropped, never torn
/// mid-line. Dropping is sticky and marks `truncated`, so callers stop
/// scanning and report partial results honestly.
struct LineWriter<'a> {
    inner: &'a mut dyn Write,
    lines_left: Option<u64>,
    bytes_left: Option<u64>,
    dropped: bool,
    truncated: bool,
}

impl<'a> LineWriter<'a> {
    fn new(out: &'a mut dyn Write, lines: Option<u64>, bytes: Option<u64>) -> Self {
        Self {
            inner: out,
            lines_left: lines,
            bytes_left: bytes,
            dropped: false,
            truncated: false,
        }
    }

    /// True once a line had to be dropped: every later line drops too, so
    /// the search should stop.
    fn spent(&self) -> bool {
        self.dropped
    }

    /// Emit one logical line. Returns false when the line was dropped by a
    /// bound (real I/O failures still return `Err`).
    fn emit(&mut self, line: &str, term: &str) -> io::Result<bool> {
        if self.dropped {
            return Ok(false);
        }
        if self.lines_left == Some(0) {
            self.dropped = true;
            self.truncated = true;
            return Ok(false);
        }
        let bytes = format!("{line}{term}");
        if self
            .bytes_left
            .is_some_and(|left| bytes.len() as u64 > left)
        {
            self.dropped = true;
            self.truncated = true;
            return Ok(false);
        }
        self.inner.write_all(bytes.as_bytes())?;
        if let Some(left) = self.lines_left.as_mut() {
            *left -= 1;
        }
        if let Some(left) = self.bytes_left.as_mut() {
            *left -= bytes.len() as u64;
        }
        Ok(true)
    }
}

struct Display {
    show_filename: bool,
    line_number: bool,
    column: bool,
    vimgrep: bool,
    invert: bool,
    terminator: &'static str,
    columns: Option<usize>,
}

/// Prints one file's results, handling `-B/-A/-C` context with `--`
/// separators between non-contiguous groups, like `rg`.
struct FilePrinter<'o, 'w, 'x> {
    path: &'o str,
    display: &'o Display,
    writer: &'w mut LineWriter<'x>,
    before_cap: usize,
    after_cap: usize,
    before: VecDeque<(u64, String)>,
    after_left: usize,
    last_printed: Option<u64>,
    started: bool,
}

impl FilePrinter<'_, '_, '_> {
    fn match_prefix(&self, lineno: u64, col: Option<usize>) -> String {
        if self.display.vimgrep {
            // `--vimgrep` implies column, so a column is always present.
            return format!("{}:{lineno}:{}:", self.path, col.unwrap_or(1));
        }
        let mut prefix = String::new();
        if self.display.show_filename {
            prefix.push_str(self.path);
            prefix.push(':');
        }
        if self.display.line_number {
            prefix.push_str(&lineno.to_string());
            prefix.push(':');
        }
        if self.display.column
            && !self.display.invert
            && let Some(col) = col
        {
            prefix.push_str(&col.to_string());
            prefix.push(':');
        }
        prefix
    }

    fn context_prefix(&self, lineno: u64) -> String {
        if self.display.vimgrep {
            return format!("{}:{lineno}:", self.path);
        }
        let mut prefix = String::new();
        if self.display.show_filename {
            prefix.push_str(self.path);
            prefix.push('-');
        }
        if self.display.line_number {
            prefix.push_str(&lineno.to_string());
            prefix.push('-');
        }
        prefix
    }

    /// Long content lines are cut at a character boundary. Paths, counts,
    /// and separators bypass this: truncating them would corrupt the output.
    fn fit_content(&mut self, content: &str) -> String {
        let Some(max) = self.display.columns else {
            return content.to_string();
        };
        if content.chars().count() <= max {
            return content.to_string();
        }
        let kept: String = content.chars().take(max).collect();
        self.writer.truncated = true;
        format!("{kept}…")
    }

    fn separator_if_gap(&mut self, group_start: u64) -> io::Result<()> {
        // Without -B/-A/-C every match is its own group and no separator
        // is printed, like `rg`.
        if (self.before_cap > 0 || self.after_cap > 0)
            && self.started
            && let Some(last) = self.last_printed
            && group_start > last + 1
        {
            self.writer.emit("--", "\n")?;
        }
        Ok(())
    }

    /// Returns false when the match line itself was dropped by a bound, so
    /// the caller stops counting and scanning.
    fn print_match(&mut self, lineno: u64, col: Option<usize>, line: &str) -> io::Result<bool> {
        if self.writer.spent() {
            return Ok(false);
        }
        let group_start = self.before.front().map_or(lineno, |(n, _)| *n);
        self.separator_if_gap(group_start)?;
        while let Some((n, text)) = self.before.pop_front() {
            let shown = self.fit_content(&text);
            let prefix = self.context_prefix(n);
            self.writer.emit(&format!("{prefix}{shown}"), "\n")?;
            self.last_printed = Some(n);
        }
        let shown = self.fit_content(line);
        let prefix = self.match_prefix(lineno, col);
        let wrote = self.writer.emit(&format!("{prefix}{shown}"), "\n")?;
        self.last_printed = Some(lineno);
        self.after_left = self.after_cap;
        self.started = true;
        Ok(wrote)
    }

    fn print_context(&mut self, lineno: u64, line: &str) -> io::Result<bool> {
        if self.writer.spent() {
            return Ok(false);
        }
        // After-context lines are consecutive with the match group, so no
        // separator check is needed here.
        let shown = self.fit_content(line);
        let prefix = self.context_prefix(lineno);
        let wrote = self.writer.emit(&format!("{prefix}{shown}"), "\n")?;
        self.last_printed = Some(lineno);
        self.started = true;
        Ok(wrote)
    }

    fn push_before(&mut self, lineno: u64, line: String) {
        if self.before_cap == 0 {
            return;
        }
        if self.before.len() == self.before_cap {
            self.before.pop_front();
        }
        self.before.push_back((lineno, line));
    }

    fn consume_after(&mut self) -> bool {
        if self.after_left > 0 {
            self.after_left -= 1;
            true
        } else {
            false
        }
    }

    fn spent(&self) -> bool {
        self.writer.spent()
    }
}

fn strip_newline(bytes: &[u8]) -> &[u8] {
    let mut line = bytes;
    if line.last() == Some(&b'\n') {
        line = &line[..line.len() - 1];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
    }
    line
}

struct FileOutcome {
    /// True for `--files`, `-l`, `--count`, `-q`, and normal matches alike.
    hit: bool,
    matching_lines: u64,
}

/// Searches one already opened file. Explicitly listed files bypass the
/// hidden/ignore/glob filters, matching `rg`.
fn search_reader(
    opts: &RgOptions,
    regex: &regex::Regex,
    display: &Display,
    path_display: &str,
    reader: &mut dyn BufRead,
    writer: &mut LineWriter,
) -> io::Result<FileOutcome> {
    if opts.files {
        let wrote = writer.emit(path_display, display.terminator)?;
        return Ok(FileOutcome {
            hit: wrote,
            matching_lines: 0,
        });
    }
    // The list and quiet modes stop at the first match; `--count` and normal
    // mode scan the whole file.
    let one_pass = opts.files_with_matches || opts.quiet;
    if one_pass {
        return search_until_first_match(opts, regex, display, path_display, reader, writer);
    }

    let mut printer = FilePrinter {
        path: path_display,
        display,
        writer,
        before_cap: opts.before,
        after_cap: opts.after,
        before: VecDeque::new(),
        after_left: 0,
        last_printed: None,
        started: false,
    };
    let mut chunk = Vec::new();
    let mut lineno: u64 = 0;
    let mut matching_lines: u64 = 0;
    let max = opts.max_count.unwrap_or(u64::MAX);
    loop {
        if printer.spent() {
            break;
        }
        chunk.clear();
        let read = reader.read_until(b'\n', &mut chunk)?;
        if read == 0 {
            break;
        }
        if chunk.contains(&0) {
            // Binary files are skipped, see the module docs.
            return Ok(FileOutcome {
                hit: false,
                matching_lines: 0,
            });
        }
        lineno += 1;
        let Ok(line) = std::str::from_utf8(strip_newline(&chunk)) else {
            // Non UTF-8 lines are skipped, like the agent search tool does.
            continue;
        };
        let raw_match = regex.is_match(line);
        let is_match = raw_match != opts.invert_match;
        if is_match {
            let col = if raw_match {
                regex.find(line).map(|m| m.start() + 1)
            } else {
                None
            };
            if opts.count {
                matching_lines += 1;
                if matching_lines >= max {
                    break;
                }
                continue;
            }
            if printer.print_match(lineno, col, line)? {
                matching_lines += 1;
                if matching_lines >= max {
                    break;
                }
            } else {
                break;
            }
        } else if printer.consume_after() {
            if !printer.print_context(lineno, line)? {
                break;
            }
        } else {
            printer.push_before(lineno, line.to_string());
        }
    }
    if opts.count {
        if matching_lines == 0 {
            return Ok(FileOutcome {
                hit: false,
                matching_lines: 0,
            });
        }
        let line = if display.show_filename {
            format!("{path_display}:{matching_lines}")
        } else {
            matching_lines.to_string()
        };
        let wrote = printer.writer.emit(&line, display.terminator)?;
        return Ok(FileOutcome {
            hit: wrote,
            matching_lines,
        });
    }
    Ok(FileOutcome {
        hit: matching_lines > 0,
        matching_lines,
    })
}

fn search_until_first_match(
    opts: &RgOptions,
    regex: &regex::Regex,
    display: &Display,
    path_display: &str,
    reader: &mut dyn BufRead,
    writer: &mut LineWriter,
) -> io::Result<FileOutcome> {
    let mut chunk = Vec::new();
    loop {
        if writer.spent() {
            return Ok(FileOutcome {
                hit: false,
                matching_lines: 0,
            });
        }
        chunk.clear();
        let read = reader.read_until(b'\n', &mut chunk)?;
        if read == 0 {
            return Ok(FileOutcome {
                hit: false,
                matching_lines: 0,
            });
        }
        if chunk.contains(&0) {
            return Ok(FileOutcome {
                hit: false,
                matching_lines: 0,
            });
        }
        let Ok(line) = std::str::from_utf8(strip_newline(&chunk)) else {
            continue;
        };
        if regex.is_match(line) != opts.invert_match {
            if opts.quiet {
                return Ok(FileOutcome {
                    hit: true,
                    matching_lines: 1,
                });
            }
            // `--files-with-matches`.
            let wrote = writer.emit(path_display, display.terminator)?;
            return Ok(FileOutcome {
                hit: wrote,
                matching_lines: 1,
            });
        }
    }
}

/// Runs the search, writing `rg` style output to `out`.
pub fn run_rg(opts: &RgOptions, out: &mut dyn Write) -> Result<RgStats> {
    if opts.patterns.is_empty() && !opts.files {
        anyhow::bail!("no pattern given (pass a PATTERN or --files)");
    }
    let regex = if opts.files {
        None
    } else {
        Some(build_regex(opts)?)
    };

    let raw_paths: Vec<PathBuf> = if opts.paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        opts.paths.clone()
    };
    let mut roots = Vec::with_capacity(raw_paths.len());
    for raw in &raw_paths {
        if !raw.exists() {
            anyhow::bail!("{}: no such file or directory", raw.display());
        }
        if raw.is_file() {
            roots.push(Root::File(raw.clone()));
        } else {
            roots.push(Root::Dir(raw.clone()));
        }
    }

    let line_number = opts.vimgrep || opts.line_number;
    let show_filename = opts.with_filename
        || opts.vimgrep
        || (!opts.no_filename
            && (roots.len() > 1 || roots.iter().any(|r| matches!(r, Root::Dir(_)))));
    // `--null` only changes the list modes, like `rg`.
    let list_mode = opts.files || opts.files_with_matches || opts.count;
    let display = Display {
        show_filename,
        line_number,
        column: opts.column || opts.vimgrep,
        vimgrep: opts.vimgrep,
        invert: opts.invert_match,
        terminator: if opts.null && list_mode { "\0" } else { "\n" },
        columns: opts.max_columns,
    };

    let mut writer = LineWriter::new(out, opts.max_lines, opts.max_bytes);
    let mut stats = RgStats::default();
    for root in &roots {
        if writer.spent() {
            break;
        }
        match root {
            Root::File(path) => {
                let shown = path.display().to_string();
                let file = std::fs::File::open(path)
                    .with_context(|| format!("cannot open {}", path.display()))?;
                let mut reader = BufReader::new(file);
                let outcome = search_reader(
                    opts,
                    regex.as_ref().unwrap_or(&empty_regex()),
                    &display,
                    &shown,
                    &mut reader,
                    &mut writer,
                )
                .with_context(|| format!("cannot read {}", path.display()))?;
                if outcome.hit {
                    stats.files_with_hits += 1;
                    stats.matching_lines += outcome.matching_lines;
                }
            }
            Root::Dir(root) => {
                search_dir(
                    opts,
                    regex.as_ref(),
                    &display,
                    root,
                    &mut writer,
                    &mut stats,
                )?;
            }
        }
    }
    stats.truncated = writer.truncated;
    Ok(stats)
}

/// Placeholder used only in `--files` mode, where no match is ever attempted.
fn empty_regex() -> regex::Regex {
    regex::Regex::new("$^").expect("static pattern compiles")
}

fn search_dir(
    opts: &RgOptions,
    regex: Option<&regex::Regex>,
    display: &Display,
    root: &Path,
    writer: &mut LineWriter,
    stats: &mut RgStats,
) -> Result<()> {
    // Canonicalize so the glob matcher and the walked paths share one
    // prefix even when the root hangs off a symlink (macOS tempdirs).
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let glob_matcher = build_glob_matcher(&canonical, &opts.globs)?;
    let mut builder = ignore::WalkBuilder::new(&canonical);
    builder
        .hidden(!effective_hidden(opts))
        .git_ignore(!effective_no_ignore(opts))
        .git_global(!effective_no_ignore(opts))
        .git_exclude(!effective_no_ignore(opts))
        .parents(!effective_no_ignore(opts))
        .ignore(!effective_no_ignore(opts))
        .follow_links(opts.follow)
        .sort_by_file_path(|a, b| a.cmp(b));
    if let Some(depth) = opts.max_depth {
        builder.max_depth(Some(depth));
    }
    // The serial iterator keeps output deterministic across runs.
    for entry in builder.build() {
        if writer.spent() {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if !glob_passes(&glob_matcher, path) {
            continue;
        }
        let shown = path.display().to_string();
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let mut reader = BufReader::new(file);
        match search_reader(
            opts,
            regex.unwrap_or(&empty_regex()),
            display,
            &shown,
            &mut reader,
            writer,
        ) {
            Ok(outcome) => {
                if outcome.hit {
                    stats.files_with_hits += 1;
                    stats.matching_lines += outcome.matching_lines;
                }
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn options(pattern: &str) -> RgOptions {
        RgOptions {
            patterns: vec![pattern.to_string()],
            line_number: true,
            ..RgOptions::default()
        }
    }

    /// Two text files plus a hidden file and an ignored file.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("a.txt"),
            "hello world\nfoo bar\nhello again\n",
        )
        .expect("write a.txt");
        fs::write(dir.path().join("b.log"), "nothing here\nstill nothing\n").expect("write b.log");
        fs::write(dir.path().join(".hidden.txt"), "hello hidden\n").expect("write hidden");
        fs::write(dir.path().join("skip.me"), "hello skipped\n").expect("write skip.me");
        fs::write(dir.path().join(".ignore"), "skip.me\n").expect("write .ignore");
        fs::create_dir(dir.path().join("sub")).expect("mkdir sub");
        fs::write(dir.path().join("sub/c.txt"), "prefix hello suffix\n").expect("write c.txt");
        dir
    }

    fn run_in(dir: &Path, opts: &RgOptions) -> (RgStats, String) {
        let mut opts = opts.clone();
        opts.paths = vec![dir.to_path_buf()];
        let mut buf = Vec::new();
        let stats = run_rg(&opts, &mut buf).expect("run_rg");
        (stats, String::from_utf8(buf).expect("utf8 output"))
    }

    #[test]
    fn prints_path_line_and_content() {
        let dir = fixture();
        let (stats, out) = run_in(dir.path(), &options("hello"));
        assert!(stats.matched());
        assert!(!stats.truncated);
        assert!(out.contains("a.txt:1:hello world"), "got:\n{out}");
        assert!(out.contains("a.txt:3:hello again"), "got:\n{out}");
        assert!(out.contains("c.txt:1:prefix hello suffix"), "got:\n{out}");
        assert!(!out.contains("b.log"), "got:\n{out}");
    }

    #[test]
    fn single_explicit_file_hides_the_filename() {
        let dir = fixture();
        let mut opts = options("hello");
        opts.paths = vec![dir.path().join("a.txt")];
        let mut buf = Vec::new();
        let stats = run_rg(&opts, &mut buf).expect("run_rg");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(stats.matched());
        assert!(out.contains("1:hello world"), "got:\n{out}");
        assert!(!out.contains("a.txt"), "got:\n{out}");
    }

    #[test]
    fn fixed_strings_escapes_regex_meta() {
        let dir = fixture();
        let mut opts = options("o w.rld");
        opts.fixed_strings = true;
        let (stats, out) = run_in(dir.path(), &opts);
        assert!(!stats.matched());
        assert!(out.is_empty());
    }

    #[test]
    fn ignore_case_matches_uppercase() {
        let dir = fixture();
        fs::write(dir.path().join("upper.txt"), "HELLO THERE\n").expect("write");
        let mut opts = options("hello");
        opts.ignore_case = true;
        let (_, out) = run_in(dir.path(), &opts);
        assert!(out.contains("upper.txt:1:HELLO THERE"), "got:\n{out}");
    }

    #[test]
    fn smart_case_folds_lowercase_patterns_only() {
        let dir = fixture();
        fs::write(dir.path().join("upper.txt"), "HELLO THERE\n").expect("write");
        let mut opts = options("hello");
        opts.smart_case = true;
        let (_, lower) = run_in(dir.path(), &opts);
        assert!(lower.contains("upper.txt"), "got:\n{lower}");
        let mut opts = options("Hello");
        opts.smart_case = true;
        let (_, mixed) = run_in(dir.path(), &opts);
        assert!(!mixed.contains("upper.txt"), "got:\n{mixed}");
    }

    #[test]
    fn word_regexp_rejects_partial_words() {
        let dir = fixture();
        let mut opts = options("hell");
        opts.word = true;
        let (stats, _) = run_in(dir.path(), &opts);
        assert!(!stats.matched());
    }

    #[test]
    fn glob_limits_to_matching_extensions() {
        let dir = fixture();
        let mut opts = options("nothing");
        opts.globs = vec!["*.log".to_string()];
        let (_, out) = run_in(dir.path(), &opts);
        assert!(out.contains("b.log"), "got:\n{out}");
        let mut opts = options("hello");
        opts.globs = vec!["*.log".to_string()];
        let (stats, _) = run_in(dir.path(), &opts);
        assert!(!stats.matched());
    }

    #[test]
    fn negated_only_glob_searches_everything_else() {
        let dir = fixture();
        let mut opts = options("hello");
        opts.globs = vec!["!*.txt".to_string()];
        let (_, out) = run_in(dir.path(), &opts);
        assert!(!out.contains("a.txt"), "got:\n{out}");
    }

    #[test]
    fn ignore_file_is_respected_unless_no_ignore() {
        let dir = fixture();
        let (_, out) = run_in(dir.path(), &options("hello"));
        assert!(!out.contains("skip.me"), "got:\n{out}");
        let mut opts = options("hello");
        opts.no_ignore = true;
        let (_, out) = run_in(dir.path(), &opts);
        assert!(out.contains("skip.me:1:hello skipped"), "got:\n{out}");
    }

    #[test]
    fn hidden_files_need_the_hidden_flag() {
        let dir = fixture();
        let (_, out) = run_in(dir.path(), &options("hello"));
        assert!(!out.contains(".hidden.txt"), "got:\n{out}");
        let mut opts = options("hello");
        opts.hidden = true;
        let (_, out) = run_in(dir.path(), &opts);
        assert!(out.contains(".hidden.txt:1:hello hidden"), "got:\n{out}");
    }

    #[test]
    fn files_with_matches_lists_each_path_once() {
        let dir = fixture();
        let mut opts = options("hello");
        opts.files_with_matches = true;
        let (stats, out) = run_in(dir.path(), &opts);
        assert!(stats.matched());
        assert_eq!(out.lines().filter(|l| l.contains("a.txt")).count(), 1);
        assert!(out.contains("c.txt"), "got:\n{out}");
    }

    #[test]
    fn count_mode_prints_per_file_totals() {
        let dir = fixture();
        let mut opts = options("hello");
        opts.count = true;
        let (stats, out) = run_in(dir.path(), &opts);
        assert!(stats.matched());
        assert!(out.contains("a.txt:2"), "got:\n{out}");
        assert!(!out.contains("b.log"), "got:\n{out}");
    }

    #[test]
    fn max_count_caps_lines_per_file() {
        let dir = fixture();
        let mut opts = options("hello");
        opts.max_count = Some(1);
        let (_, out) = run_in(dir.path(), &opts);
        assert!(out.contains("a.txt:1:hello world"), "got:\n{out}");
        assert!(!out.contains("a.txt:3:"), "got:\n{out}");
    }

    #[test]
    fn context_prints_markers_and_separators() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("ctx.txt"),
            "one\ntwo\nmatch one\nfour\nfive\nsix\nseven\nmatch two\nnine\n",
        )
        .expect("write");
        let mut opts = options("match");
        opts.paths = vec![dir.path().join("ctx.txt")];
        opts.before = 1;
        opts.after = 1;
        let mut buf = Vec::new();
        run_rg(&opts, &mut buf).expect("run_rg");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("2-two"), "got:\n{out}");
        assert!(out.contains("3:match one"), "got:\n{out}");
        assert!(out.contains("--"), "got:\n{out}");
    }

    #[test]
    fn vimgrep_always_shows_file_line_and_column() {
        let dir = fixture();
        let mut opts = options("world");
        opts.vimgrep = true;
        let (_, out) = run_in(dir.path(), &opts);
        assert!(out.contains("a.txt:1:7:hello world"), "got:\n{out}");
    }

    #[test]
    fn invert_match_reports_non_matching_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("inv.txt"), "keep\nskip me\nkeep too\n").expect("write");
        let mut opts = options("skip");
        opts.paths = vec![dir.path().join("inv.txt")];
        opts.invert_match = true;
        let mut buf = Vec::new();
        let stats = run_rg(&opts, &mut buf).expect("run_rg");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(stats.matched());
        assert!(out.contains("1:keep"), "got:\n{out}");
        assert!(!out.contains("skip me"), "got:\n{out}");
    }

    #[test]
    fn files_mode_lists_searchable_files() {
        let dir = fixture();
        let opts = RgOptions {
            files: true,
            ..RgOptions::default()
        };
        let (stats, out) = run_in(dir.path(), &opts);
        assert!(stats.matched());
        assert!(out.contains("a.txt"), "got:\n{out}");
        assert!(!out.contains("skip.me"), "got:\n{out}");
        assert!(!out.contains(".hidden.txt"), "got:\n{out}");
    }

    #[test]
    fn no_match_reports_cleanly() {
        let dir = fixture();
        let (stats, out) = run_in(dir.path(), &options("definitely absent pattern"));
        assert!(!stats.matched());
        assert!(!stats.truncated);
        assert!(out.is_empty());
    }

    #[test]
    fn missing_root_is_an_error() {
        let mut opts = options("hello");
        opts.paths = vec![PathBuf::from("no-such-dir-2b9f1c")];
        let mut buf = Vec::new();
        assert!(run_rg(&opts, &mut buf).is_err());
    }

    #[test]
    fn invalid_pattern_is_an_error() {
        let mut opts = options("(unclosed");
        opts.paths = vec![PathBuf::from(".")];
        let mut buf = Vec::new();
        assert!(run_rg(&opts, &mut buf).is_err());
    }

    #[test]
    fn max_lines_stops_the_scan_and_marks_truncated() {
        let dir = fixture();
        let mut opts = options("hello");
        opts.max_lines = Some(2);
        let (stats, out) = run_in(dir.path(), &opts);
        assert!(stats.matched());
        assert!(stats.truncated);
        assert_eq!(out.lines().count(), 2, "got:\n{out}");
        assert_eq!(stats.matching_lines, 2);
    }

    #[test]
    fn max_bytes_keeps_whole_lines_only() {
        let dir = fixture();
        // Walked paths are canonical absolute paths: budget exactly the
        // first match line so the second drops whole instead of tearing.
        let first = format!(
            "{}:1:hello world\n",
            dir.path()
                .canonicalize()
                .expect("canonicalize")
                .join("a.txt")
                .display()
        );
        let mut opts = options("hello");
        opts.max_bytes = Some(first.len() as u64);
        let (stats, out) = run_in(dir.path(), &opts);
        assert!(stats.matched());
        assert!(stats.truncated);
        assert_eq!(out, first, "got:\n{out}");
        assert_eq!(stats.matching_lines, 1);
    }

    #[test]
    fn max_columns_truncates_long_content_and_marks_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("long.txt"), "hello abcdefghij\n").expect("write");
        let mut opts = options("hello");
        opts.paths = vec![dir.path().join("long.txt")];
        opts.max_columns = Some(8);
        let mut buf = Vec::new();
        let stats = run_rg(&opts, &mut buf).expect("run_rg");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(stats.matched());
        assert!(stats.truncated);
        assert!(out.contains("hello ab…"), "got:\n{out}");
        assert!(!out.contains("abcdefghij"), "got:\n{out}");
    }
}
