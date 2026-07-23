//! Columned, selectable table — the multi-column peer of [`SelectList`].
//!
//! The defining widget of repo/branch/worktree browsers, process and container
//! lists, and file explorers: a header row, per-column width policies, a
//! full-row selection highlight, a caret gutter, and windowed scrolling. Column
//! widths are resolved by the same flexbox [`solve`](crate::solve) the rest of
//! the toolkit uses — a column is [`Column::fixed`], [`Column::auto`] (sizes to
//! its widest cell), or [`Column::flex`] (shares leftover width by weight) — so
//! a table lays out consistently with every other container.
//!
//! Selection and navigation are the same [`SelectState`] a [`SelectList`] uses
//! (`handle` for arrows/Enter/Esc, `select` to drive it from a host model), so a
//! single-column list and a table share one state type.
//!
//! [`SelectList`]: crate::SelectList

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::geometry::Size;
use crate::layout::{Dimension, Item, LayoutStyle, solve};
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

use super::select::SelectState;
use super::text::line_width;

/// One table column: a header cell and a main-axis width policy.
pub struct Column {
    header: Line<'static>,
    width: Dimension,
}

impl Column {
    /// A column with an explicit [`Dimension`] width policy.
    pub fn new(header: impl Into<Line<'static>>, width: Dimension) -> Self {
        Self {
            header: header.into(),
            width,
        }
    }

    /// A column that sizes to its widest cell (header included).
    pub fn auto(header: impl Into<Line<'static>>) -> Self {
        Self::new(header, Dimension::Auto)
    }

    /// A column of exactly `cells` columns wide.
    pub fn fixed(header: impl Into<Line<'static>>, cells: u16) -> Self {
        Self::new(header, Dimension::Fixed(cells))
    }

    /// A column that grows to share leftover width, weighted by `weight`.
    pub fn flex(header: impl Into<Line<'static>>, weight: u16) -> Self {
        Self::new(header, Dimension::Flex(weight))
    }
}

/// A multi-column selectable list.
///
/// Pairs with a host-held [`SelectState`] (the row selection) exactly as
/// [`SelectList`](crate::SelectList) does. Each row is a `Vec` of cell
/// [`Line`]s, one per column; a short row is padded with blanks, extra cells are
/// ignored.
///
/// # Example
///
/// ```
/// use ratatui::text::Line;
/// use tuika::{Column, Table, SelectState, Theme};
/// use tuika::testing::{grid, render};
///
/// let cols = vec![Column::auto("name"), Column::auto("kind")];
/// let rows = vec![
///     vec![Line::from("main"), Line::from("branch")],
///     vec![Line::from("wip"), Line::from("branch")],
/// ];
/// let state = SelectState::new(); // first row selected
/// let table = Table::new(cols, rows, &state);
///
/// let g = grid(&render(&table, 20, 3, &Theme::default()));
/// let lines: Vec<&str> = g.lines().collect();
/// assert!(lines[0].contains("name") && lines[0].contains("kind")); // header row
/// assert!(lines[1].starts_with("› main"));                         // caret on selection
/// assert!(lines[1].contains("branch") && lines[2].contains("wip"));
/// ```
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<Line<'static>>>,
    selected: usize,
    viewport: Option<u16>,
    scrollbar: bool,
    gutter: bool,
    show_header: bool,
    gap: u16,
}

impl Table {
    /// A table of `columns` and `rows`, with the row from `state` selected.
    pub fn new(columns: Vec<Column>, rows: Vec<Vec<Line<'static>>>, state: &SelectState) -> Self {
        Self {
            columns,
            rows,
            selected: state.selected(),
            viewport: None,
            scrollbar: true,
            gutter: true,
            show_header: true,
            gap: 2,
        }
    }

    /// Cap the visible data rows to `rows`, windowing a longer table around the
    /// selection so the selected row stays on screen.
    pub fn viewport(mut self, rows: u16) -> Self {
        self.viewport = Some(rows.max(1));
        self
    }

    /// Show the overflow scrollbar (default true; only drawn when windowed).
    pub fn scrollbar(mut self, show: bool) -> Self {
        self.scrollbar = show;
        self
    }

    /// Show the caret gutter that marks the selected row (default true).
    pub fn gutter(mut self, show: bool) -> Self {
        self.gutter = show;
        self
    }

    /// Show the header row (default true).
    pub fn header(mut self, show: bool) -> Self {
        self.show_header = show;
        self
    }

    /// Columns of blank between adjacent columns (default 2).
    pub fn gap(mut self, gap: u16) -> Self {
        self.gap = gap;
        self
    }

    /// Cells the caret gutter occupies (caret + space), or 0 when hidden.
    fn gutter_width(&self) -> u16 {
        if self.gutter { 2 } else { 0 }
    }

    /// Rows the header occupies (0 or 1).
    fn header_rows(&self) -> u16 {
        u16::from(self.show_header)
    }

    /// The widest content in column `c` — its header and every cell — used as
    /// the intrinsic size the solver gives an [`Dimension::Auto`] column.
    fn column_intrinsic(&self, c: usize) -> u16 {
        let header_w = if self.show_header {
            line_width(&self.columns[c].header)
        } else {
            0
        };
        let cells_w = self
            .rows
            .iter()
            .filter_map(|row| row.get(c))
            .map(line_width)
            .max()
            .unwrap_or(0);
        header_w.max(cells_w)
    }

    /// Resolve each column's rect inside `cols_area` via the flexbox solver.
    fn solve_columns(&self, cols_area: Rect) -> Vec<Rect> {
        let items: Vec<Item> = self
            .columns
            .iter()
            .enumerate()
            .map(|(c, col)| Item::new(col.width, Size::new(self.column_intrinsic(c), 1)))
            .collect();
        solve(cols_area, &LayoutStyle::row().gap(self.gap), &items)
    }

    /// The `(start, visible_rows)` data-row window: the whole table unless a
    /// [`viewport`](Self::viewport) smaller than the row count is set, in which
    /// case a slice centered on the selection and clamped to the ends.
    fn window(&self) -> (usize, usize) {
        let total = self.rows.len();
        match self.viewport {
            Some(v) if total > v as usize => {
                let v = (v as usize).max(1);
                let start = self.selected.saturating_sub(v / 2).min(total - v);
                (start, v)
            }
            _ => (0, total),
        }
    }

    /// Draw one row's cells into their column rects at row `y`, patching each
    /// cell's style with `row_style` (the selection style on the selected row).
    fn draw_cells(
        &self,
        row: &[Line<'static>],
        col_rects: &[Rect],
        y: u16,
        row_style: Option<Style>,
        surface: &mut Surface,
    ) {
        for (c, rect) in col_rects.iter().enumerate() {
            let Some(cell) = row.get(c) else {
                continue;
            };
            let mut x = rect.x;
            let right = rect.x.saturating_add(rect.width);
            for span in &cell.spans {
                if x >= right {
                    break;
                }
                let style = match row_style {
                    Some(sel) => span.style.patch(sel),
                    None => span.style,
                };
                x = surface.set_string(x, y, span.content.as_ref(), style);
            }
        }
    }
}

impl View for Table {
    fn measure(&self, available: Size) -> Size {
        let (_, rows) = self.window();
        let cols_w: u16 = (0..self.columns.len())
            .map(|c| self.column_intrinsic(c))
            .fold(0, u16::saturating_add);
        let gaps = self
            .gap
            .saturating_mul(self.columns.len().saturating_sub(1) as u16);
        let width = self
            .gutter_width()
            .saturating_add(cols_w)
            .saturating_add(gaps);
        let height = self.header_rows().saturating_add(rows as u16);
        Size::new(width.min(available.width), height)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 || self.columns.is_empty() {
            return;
        }
        let (start, win_rows) = self.window();
        let overflow = self.rows.len() > win_rows;
        let gutter_w = self.gutter_width();
        let scrollbar_w = u16::from(overflow && self.scrollbar);

        // The band the columns lay out in: full width minus the gutter and the
        // reserved scrollbar column.
        let cols_area = Rect::new(
            area.x.saturating_add(gutter_w),
            area.y,
            area.width
                .saturating_sub(gutter_w)
                .saturating_sub(scrollbar_w),
            area.height,
        );
        let col_rects = self.solve_columns(cols_area);

        // Header row.
        if self.show_header {
            let headers: Vec<Line<'static>> =
                self.columns.iter().map(|c| c.header.clone()).collect();
            self.draw_cells(
                &headers,
                &col_rects,
                area.y,
                Some(ctx.theme.accent_style()),
                surface,
            );
        }

        // Data rows, windowed around the selection.
        let body_top = area.y.saturating_add(self.header_rows());
        let row_span_w = area.width.saturating_sub(scrollbar_w);
        for i in 0..win_rows {
            let idx = start + i;
            let Some(row) = self.rows.get(idx) else {
                break;
            };
            let y = body_top.saturating_add(i as u16);
            if y >= area.bottom() {
                break;
            }
            let selected = idx == self.selected;
            let row_style = if selected {
                let sel = ctx.theme.selection_style();
                // Highlight spans the whole row: gutter, columns, and gaps.
                let mut band = surface.child(Rect::new(area.x, y, row_span_w, 1));
                band.fill(sel);
                Some(sel)
            } else {
                None
            };
            if self.gutter {
                let caret = if selected { '›' } else { ' ' };
                let caret_style = if selected {
                    ctx.theme.selection_style()
                } else {
                    ctx.theme.muted_style()
                };
                surface.set(area.x, y, caret, caret_style);
            }
            self.draw_cells(row, &col_rects, y, row_style, surface);
        }

        if scrollbar_w == 1 {
            self.draw_scrollbar(area, start, win_rows, surface, ctx);
        }
    }
}

impl Table {
    /// A right-edge scrollbar whose thumb tracks the data-row window, mirroring
    /// [`SelectList`](crate::SelectList)'s. Drawn over the body rows, below the
    /// header.
    fn draw_scrollbar(
        &self,
        area: Rect,
        start: usize,
        rows: usize,
        surface: &mut Surface,
        ctx: &RenderCtx,
    ) {
        let total = self.rows.len();
        let track_x = area.right() - 1;
        let track_top = area.y.saturating_add(self.header_rows());
        let track_h = rows as u16;
        let max_start = total.saturating_sub(rows).max(1) as u32;
        let thumb_h = (((rows * rows) / total).max(1) as u16).min(track_h);
        let travel = track_h.saturating_sub(thumb_h);
        let thumb_y = track_top + ((start as u32 * travel as u32) / max_start) as u16;
        let track_style = Style::default().fg(ctx.theme.dim);
        let thumb_style = Style::default().fg(ctx.theme.muted);
        for r in 0..track_h {
            let y = track_top + r;
            if y >= area.bottom() {
                break;
            }
            let within = y >= thumb_y && y < thumb_y.saturating_add(thumb_h);
            let (glyph, style) = if within {
                ('█', thumb_style)
            } else {
                ('│', track_style)
            };
            surface.set(track_x, y, glyph, style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Key, KeyCode};
    use crate::style::Theme;
    use crate::test_support::{buffer, rainbow_theme, row};
    use crate::view::{RenderCtx, View};
    use crate::{Size, Surface};
    use ratatui::text::Line;

    fn sample() -> (Vec<Column>, Vec<Vec<Line<'static>>>) {
        let cols = vec![Column::auto("name"), Column::auto("kind")];
        let rows = vec![
            vec![Line::from("main"), Line::from("branch")],
            vec![Line::from("wip"), Line::from("branch")],
            vec![Line::from("hotfix"), Line::from("branch")],
        ];
        (cols, rows)
    }

    #[test]
    fn table_renders_header_and_marks_selection() {
        let (cols, rows) = sample();
        let mut state = SelectState::new();
        state.select(1);
        let table = Table::new(cols, rows, &state);
        let mut buf = buffer(20, 4);
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        table.render(area, &mut surface, &ctx);
        // Header on row 0, data below.
        assert!(row(&buf, 0).contains("name") && row(&buf, 0).contains("kind"));
        assert!(row(&buf, 1).contains("main"));
        assert!(row(&buf, 2).contains("wip"));
        // Caret on the selected (second) data row, at column 0.
        assert_eq!(buf[(0, 2)].symbol(), "›");
        assert_ne!(buf[(0, 1)].symbol(), "›", "unselected row has no caret");
    }

    #[test]
    fn table_selection_highlights_full_row() {
        let (cols, rows) = sample();
        let t = rainbow_theme();
        let mut state = SelectState::new();
        state.select(0);
        let table = Table::new(cols, rows, &state);
        let mut buf = buffer(20, 4);
        let area = buf.area;
        let ctx = RenderCtx::new(&t);
        let mut surface = Surface::new(&mut buf, area);
        table.render(area, &mut surface, &ctx);
        // The whole selected row (row 1) carries the selection background, gutter
        // through the last content column, not just the cell text.
        let y = 1;
        for x in [0u16, 2, 8, 15] {
            assert_eq!(buf[(x, y)].bg, t.selection_bg, "row bg at col {x}");
        }
        // A non-selected data row is not highlighted.
        assert_ne!(buf[(0, 2)].bg, t.selection_bg);
    }

    #[test]
    fn table_columns_use_the_flex_solver() {
        // A fixed first column and a flex second: the flex column grows to fill
        // the leftover width. Verify via the resolved column rects.
        let cols = vec![Column::fixed("id", 4), Column::flex("desc", 1)];
        let rows = vec![vec![Line::from("1"), Line::from("x")]];
        let state = SelectState::new();
        let table = Table::new(cols, rows, &state).gutter(false).gap(1);
        // cols_area spans the full width (no gutter, no overflow scrollbar).
        let rects = table.solve_columns(Rect::new(0, 0, 20, 1));
        assert_eq!(rects[0].width, 4, "fixed column keeps its width");
        // 20 - 4 (fixed) - 1 (gap) = 15 for the flex column.
        assert_eq!(rects[1].width, 15, "flex column takes the leftover");
        assert_eq!(rects[1].x, 5, "second column starts after col1 + gap");
    }

    #[test]
    fn table_windows_long_body_and_keeps_selection_visible() {
        let cols = vec![Column::auto("n")];
        let rows: Vec<Vec<Line>> = (0..20)
            .map(|i| vec![Line::from(format!("row{i}"))])
            .collect();
        let mut state = SelectState::new();
        state.select(15);
        let table = Table::new(cols, rows, &state).viewport(4);
        let theme = Theme::default();
        // Height = header (1) + viewport (4).
        assert_eq!(table.measure(Size::new(30, 40)).height, 5);
        let text = crate::testing::grid(&crate::testing::render(&table, 30, 5, &theme));
        assert!(text.contains("row15"), "selection visible:\n{text}");
        assert!(!text.contains("row0\n"), "far rows windowed out:\n{text}");
        // Scrollbar occupies the last column on a body row.
        let buf = crate::testing::render(&table, 30, 5, &theme);
        let has_bar = (1..5).any(|y| matches!(buf[(29, y)].symbol(), "█" | "│"));
        assert!(has_bar, "overflowing table draws a scrollbar:\n{text}");
    }

    #[test]
    fn table_navigates_with_select_state() {
        // The table shares SelectState with SelectList — arrow keys move the row.
        let (_, rows) = sample();
        let mut state = SelectState::new();
        state.handle(&Event::Key(Key::new(KeyCode::Down)), rows.len());
        assert_eq!(state.selected(), 1);
    }

    #[test]
    fn table_pads_short_rows() {
        // A row with fewer cells than columns simply leaves the missing cells
        // blank rather than panicking.
        let cols = vec![Column::auto("a"), Column::auto("b")];
        let rows = vec![vec![Line::from("only")]]; // missing second cell
        let state = SelectState::new();
        let table = Table::new(cols, rows, &state);
        let theme = Theme::default();
        let text = crate::testing::grid(&crate::testing::render(&table, 20, 2, &theme));
        assert!(text.contains("only"));
    }
}
