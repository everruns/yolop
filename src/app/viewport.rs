//! Inline-viewport anchoring helpers. Ratatui anchors inline viewports to the
//! cursor row at init and keeps that row fixed across vertical grows; a
//! horizontal shrink resets the origin to the top. Yolop pins the composer to
//! the terminal bottom by moving the backend cursor there and recomputing the
//! inline origin instead of inserting blank scrollback lines.

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Position, Rect};

/// How many rows remain between the inline viewport bottom edge and the
/// terminal bottom. Zero means the composer is flush with the terminal bottom.
pub(crate) fn rows_below_inline_viewport(
    viewport_top: u16,
    viewport_height: u16,
    terminal_height: u16,
) -> u16 {
    terminal_height.saturating_sub(viewport_top.saturating_add(viewport_height))
}

/// Push the inline viewport to the terminal bottom when it has drifted upward.
/// No-op when already flush with the bottom edge.
pub(crate) fn maybe_reanchor_inline_viewport<B>(terminal: &mut Terminal<B>) -> Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let terminal_size = terminal.size()?;
    let viewport_area = terminal.get_frame().area();
    let rows_below =
        rows_below_inline_viewport(viewport_area.y, viewport_area.height, terminal_size.height);
    if rows_below == 0 {
        return Ok(());
    }

    // Wait until crossterm has observed a PTY resize before recomputing the
    // inline origin from the terminal bottom.
    let viewport_bottom = viewport_area.y.saturating_add(viewport_area.height);
    if viewport_bottom > terminal_size.height {
        return Ok(());
    }

    // Recompute the inline origin from a bottom-row cursor. Blank scrollback
    // lines would leave a visible gap above the composer.
    let bottom_row = terminal_size.height.saturating_sub(1);
    terminal.backend_mut().set_cursor_position(Position {
        x: 0,
        y: bottom_row,
    })?;
    terminal.resize(Rect::new(0, 0, terminal_size.width, terminal_size.height))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::TerminalOptions;
    use ratatui::Viewport;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;

    fn viewport_bottom<B: Backend>(terminal: &mut Terminal<B>) -> u16 {
        let area = terminal.get_frame().area();
        area.y.saturating_add(area.height)
    }

    fn blank_rows_before_viewport(terminal: &mut Terminal<TestBackend>) -> usize {
        let viewport_top = terminal.get_frame().area().y;
        let buffer = terminal.backend().buffer();
        (0..viewport_top)
            .filter(|y| {
                (0..buffer.area.width).all(|x| {
                    buffer[(x, *y)]
                        .symbol()
                        .chars()
                        .all(|ch| ch.is_whitespace())
                })
            })
            .count()
    }

    #[test]
    fn reanchor_from_top_pins_viewport_without_large_blank_band() {
        let height = 60;
        let mut backend = TestBackend::new(80, height);
        backend
            .set_cursor_position(Position { x: 0, y: 1 })
            .unwrap();
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(18),
            },
        )
        .unwrap();

        maybe_reanchor_inline_viewport(&mut terminal).expect("initial anchor");

        assert_eq!(viewport_bottom(&mut terminal), height);
        assert!(
            terminal.backend().scrollback().area.height < 25,
            "cursor+resize anchoring should not push a tall blank band into scrollback"
        );
    }

    #[test]
    fn reanchor_after_grow_keeps_scrollback_band_bounded() {
        let mut backend = TestBackend::new(80, 24);
        backend
            .set_cursor_position(Position { x: 0, y: 1 })
            .unwrap();
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(18),
            },
        )
        .unwrap();

        maybe_reanchor_inline_viewport(&mut terminal).expect("initial anchor");
        assert!(
            terminal.backend().scrollback().area.height < 25,
            "initial anchor should not push a tall blank band into scrollback"
        );

        terminal.backend_mut().resize(80, 40);
        terminal.autoresize().expect("autoresize grow");
        maybe_reanchor_inline_viewport(&mut terminal).expect("re-anchor after grow");

        assert_eq!(viewport_bottom(&mut terminal), 40);
        assert!(
            terminal.backend().scrollback().area.height < 25,
            "re-anchor after grow should not push a tall blank band into scrollback"
        );
    }

    /// Documents the pre-0.5.0 anchoring mechanism: blank `insert_before` rows
    /// fill the band above the inline viewport. On Crossterm that band becomes
    /// visible scrollback in tall terminals; PTY integration tests cover that.
    #[test]
    fn insert_before_blank_line_anchor_fills_rows_above_viewport() {
        let height = 60;
        let viewport_height = 18;
        let mut backend = TestBackend::new(80, height);
        backend
            .set_cursor_position(Position { x: 0, y: 1 })
            .unwrap();
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(viewport_height),
            },
        )
        .unwrap();

        let viewport_area = terminal.get_frame().area();
        let blank_lines = rows_below_inline_viewport(viewport_area.y, viewport_area.height, height);
        assert!(
            blank_lines >= 40,
            "fixture should mimic a tall terminal with the cursor near the top"
        );

        terminal
            .insert_before(blank_lines, |_| {})
            .expect("blank-line anchor");

        assert_eq!(viewport_bottom(&mut terminal), height);
        assert!(
            blank_rows_before_viewport(&mut terminal) >= 40,
            "blank insert_before anchoring fills the rows above the viewport"
        );
    }

    #[test]
    fn reanchor_after_terminal_grow_moves_viewport_to_bottom() {
        let mut backend = TestBackend::new(80, 24);
        backend
            .set_cursor_position(Position { x: 0, y: 1 })
            .unwrap();
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(18),
            },
        )
        .unwrap();

        maybe_reanchor_inline_viewport(&mut terminal).expect("initial anchor");
        assert_eq!(viewport_bottom(&mut terminal), 24);

        terminal.backend_mut().resize(80, 40);
        terminal.autoresize().expect("autoresize grow");
        let bottom_before_reanchor = viewport_bottom(&mut terminal);
        assert!(
            bottom_before_reanchor <= 40,
            "inline viewport should stay within the grown terminal before re-anchor"
        );

        maybe_reanchor_inline_viewport(&mut terminal).expect("re-anchor after grow");
        assert_eq!(viewport_bottom(&mut terminal), 40);
    }

    #[test]
    fn reanchor_after_horizontal_shrink_moves_viewport_back_to_bottom() {
        let mut backend = TestBackend::new(80, 24);
        backend
            .set_cursor_position(Position { x: 0, y: 1 })
            .unwrap();
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(18),
            },
        )
        .unwrap();

        maybe_reanchor_inline_viewport(&mut terminal).expect("initial anchor");
        assert_eq!(viewport_bottom(&mut terminal), 24);

        terminal.backend_mut().resize(60, 24);
        terminal.autoresize().expect("autoresize shrink");
        assert_eq!(
            terminal.get_frame().area().y,
            0,
            "ratatui resets inline origin to the top on horizontal shrink"
        );

        maybe_reanchor_inline_viewport(&mut terminal).expect("re-anchor after shrink");
        assert_eq!(viewport_bottom(&mut terminal), 24);
    }
}
