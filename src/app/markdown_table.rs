//! Markdown pipe-table parsing and terminal layout via `comfy-table`.

use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, CellAlignment, ContentArrangement, Table};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{TEXT_DIM, TEXT_PRIMARY, push_markdown_line};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkdownTable {
    pub headers: Vec<String>,
    pub alignments: Vec<CellAlignment>,
    pub rows: Vec<Vec<String>>,
}

/// Returns `(table, next_line_index)` when `lines[start]` begins a GFM table.
pub(crate) fn try_parse_table_at(lines: &[&str], start: usize) -> Option<(MarkdownTable, usize)> {
    if start + 1 >= lines.len() {
        return None;
    }
    if !is_table_row(lines[start]) || !is_table_separator(lines[start + 1]) {
        return None;
    }

    let headers = parse_cells(lines[start]);
    if headers.is_empty() {
        return None;
    }

    let alignments = parse_alignments(lines[start + 1], headers.len());
    let mut rows = Vec::new();
    let mut index = start + 2;
    while index < lines.len() && is_table_row(lines[index]) && !is_table_separator(lines[index]) {
        let mut row = parse_cells(lines[index]);
        row.resize(headers.len(), String::new());
        row.truncate(headers.len());
        rows.push(row);
        index += 1;
    }

    Some((
        MarkdownTable {
            headers,
            alignments,
            rows,
        },
        index,
    ))
}

pub(crate) fn append_markdown_table_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    first_prefix: &str,
    prefix_style: Style,
    table: &MarkdownTable,
    inner_width: usize,
    first: &mut bool,
) -> bool {
    let prefix_len = first_prefix.chars().count();
    let wrap_width = inner_width.saturating_sub(prefix_len).max(1);
    let rendered = render_table(table, wrap_width);
    if !table_render_fits(&rendered, inner_width, prefix_len) {
        return false;
    }

    let continuation = " ".repeat(prefix_len);
    for rendered_line in rendered {
        let spans = style_table_line(&rendered_line);
        push_markdown_line(
            lines,
            first_prefix,
            &continuation,
            prefix_style,
            first,
            spans,
        );
    }
    true
}

fn table_render_fits(rendered: &[String], inner_width: usize, prefix_len: usize) -> bool {
    rendered.iter().all(|line| {
        prefix_len.saturating_add(line.chars().count()) <= inner_width
    })
}

pub(crate) fn render_table(table: &MarkdownTable, width: usize) -> Vec<String> {
    let mut comfy = Table::new();
    comfy
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(width.max(1) as u16);

    let header_cells = table
        .headers
        .iter()
        .map(|header| Cell::new(header.as_str()))
        .collect::<Vec<_>>();
    comfy.set_header(header_cells);

    for row in &table.rows {
        let cells = row
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let cell = Cell::new(value.as_str());
                if let Some(alignment) = table.alignments.get(index) {
                    cell.set_alignment(*alignment)
                } else {
                    cell
                }
            })
            .collect::<Vec<_>>();
        comfy.add_row(cells);
    }

    comfy.to_string().lines().map(str::to_owned).collect()
}

fn style_table_line(line: &str) -> Vec<Span<'static>> {
    let has_cell_delimiters = line.contains('│')
        || line
            .chars()
            .filter(|ch| *ch == '|')
            .count()
            >= 2;
    if !has_cell_delimiters || is_border_only_line(line) {
        return vec![Span::styled(
            line.to_string(),
            Style::default().fg(TEXT_DIM),
        )];
    }

    let border_style = Style::default().fg(TEXT_DIM);
    let content_style = if is_header_content_line(line) {
        Style::default()
            .fg(TEXT_PRIMARY)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT_PRIMARY)
    };

    let mut spans = Vec::new();
    let mut cell = String::new();
    for ch in line.chars() {
        if ch == '│' || ch == '|' {
            if !cell.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut cell), content_style));
            }
            spans.push(Span::styled(ch.to_string(), border_style));
        } else {
            cell.push(ch);
        }
    }
    if !cell.is_empty() {
        spans.push(Span::styled(cell, content_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(line.to_string(), content_style));
    }
    spans
}

fn is_header_content_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with(['│', '|']) && !trimmed.contains(['═', '=']) && !is_border_only_line(line)
}

fn is_border_only_line(line: &str) -> bool {
    line.trim().chars().all(|ch| {
        matches!(
            ch,
            '│' | '┃'
                | '├'
                | '┤'
                | '┼'
                | '─'
                | '━'
                | '┴'
                | '┬'
                | '╭'
                | '╮'
                | '╰'
                | '╯'
                | '╞'
                | '╡'
                | '╪'
                | '╌'
                | '┆'
                | '+'
                | '-'
                | '='
                | '|'
                | ' '
        )
    })
}

pub(crate) fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed[1..].contains('|')
}

pub(crate) fn is_table_separator(line: &str) -> bool {
    if !is_table_row(line) {
        return false;
    }
    parse_cells(line)
        .iter()
        .all(|cell| cell_contains_only_alignment_markers(cell))
}

fn cell_contains_only_alignment_markers(cell: &str) -> bool {
    let trimmed = cell.trim();
    !trimmed.is_empty() && trimmed.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
}

pub(crate) fn parse_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(str::trim)
        .map(str::to_owned)
        .collect()
}

fn parse_alignments(separator: &str, column_count: usize) -> Vec<CellAlignment> {
    let mut alignments = parse_cells(separator)
        .into_iter()
        .map(|cell| parse_alignment(&cell))
        .collect::<Vec<_>>();
    alignments.resize(column_count, CellAlignment::Left);
    alignments.truncate(column_count);
    alignments
}

fn parse_alignment(cell: &str) -> CellAlignment {
    let trimmed = cell.trim();
    let left = trimmed.starts_with(':');
    let right = trimmed.ends_with(':');
    match (left, right) {
        (true, true) => CellAlignment::Center,
        (_, true) => CellAlignment::Right,
        _ => CellAlignment::Left,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_table_block() {
        let lines = vec![
            "| Name | Value |",
            "| --- | --- |",
            "| foo | bar |",
            "plain text",
        ];
        let (table, next) = try_parse_table_at(&lines, 0).expect("table");
        assert_eq!(table.headers, vec!["Name", "Value"]);
        assert_eq!(table.rows, vec![vec!["foo", "bar"]]);
        assert_eq!(next, 3);
    }

    #[test]
    fn parses_column_alignments() {
        let lines = vec![
            "| Left | Center | Right |",
            "|:---|:---:|---:|",
            "| a | b | c |",
        ];
        let (table, _) = try_parse_table_at(&lines, 0).expect("table");
        assert_eq!(
            table.alignments,
            vec![
                CellAlignment::Left,
                CellAlignment::Center,
                CellAlignment::Right,
            ]
        );
    }

    #[test]
    fn render_table_respects_width() {
        let table = MarkdownTable {
            headers: vec!["Command".into(), "Description".into()],
            alignments: vec![CellAlignment::Left, CellAlignment::Left],
            rows: vec![vec![
                "cargo test --all-features".into(),
                "Run the full test suite before shipping".into(),
            ]],
        };

        for width in [24, 40, 80] {
            let rendered = render_table(&table, width);
            assert!(
                !rendered.is_empty(),
                "expected table output at width {width}"
            );
            assert!(
                rendered.iter().all(|line| line.chars().count() <= width),
                "table line exceeded width {width}: {rendered:?}"
            );
        }
    }

    #[test]
    fn render_table_changes_layout_when_width_changes() {
        let table = MarkdownTable {
            headers: vec!["Key".into(), "Value".into()],
            alignments: vec![CellAlignment::Left, CellAlignment::Left],
            rows: vec![vec![
                "description".into(),
                "a deliberately long value that should wrap".into(),
            ]],
        };

        let narrow = render_table(&table, 28);
        let wide = render_table(&table, 80);
        assert_ne!(
            narrow, wide,
            "dynamic layout should reflow when width changes: narrow={narrow:?} wide={wide:?}"
        );
    }

    #[test]
    fn separator_detection_rejects_plain_pipe_text() {
        assert!(!is_table_separator("| just | one | row |"));
    }

    #[test]
    fn table_line_styles_use_dim_borders() {
        let spans = style_table_line("╭──────┬──────╮");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, Some(TEXT_DIM));
    }

    #[test]
    fn table_line_styles_bold_header_cells() {
        let spans = style_table_line("│ Name     │ Value    │");
        assert!(
            spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD)),
            "header row should include bold spans: {spans:?}"
        );
    }

    #[test]
    fn table_line_styles_dim_header_separator() {
        let spans = style_table_line("╞═══╪═══╡");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, Some(TEXT_DIM));
    }

    #[test]
    fn append_table_falls_back_when_width_is_too_narrow() {
        let table = MarkdownTable {
            headers: vec!["Col A".into(), "Col B".into()],
            alignments: vec![CellAlignment::Left, CellAlignment::Left],
            rows: vec![vec!["alpha".into(), "beta".into()]],
        };
        let mut lines = Vec::new();
        let mut first = true;
        let fitted = append_markdown_table_lines(
            &mut lines,
            "agent › ",
            Style::default(),
            &table,
            12,
            &mut first,
        );
        assert!(!fitted, "narrow terminals should reject boxed tables");
        assert!(lines.is_empty());
    }
}
