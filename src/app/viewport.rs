//! Inline-viewport anchoring helpers. Ratatui anchors inline viewports to the
//! cursor row at init and keeps that row fixed across vertical grows; a
//! horizontal shrink resets the origin to the top. Yolop wants the composer
//! pinned to the terminal bottom, so we re-insert blank scrollback lines when
//! the viewport drifts away from that target.

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::Backend;

/// Blank lines to insert above the inline viewport so its bottom edge sits on
/// the terminal bottom.
pub(crate) fn blank_lines_after_inline_viewport(
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
    let terminal_height = terminal.size()?.height;
    let viewport_area = terminal.get_frame().area();
    let blank_lines =
        blank_lines_after_inline_viewport(viewport_area.y, viewport_area.height, terminal_height);
    if blank_lines == 0 {
        return Ok(());
    }
    terminal.insert_before(blank_lines, |_| {})?;
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
        assert!(
            viewport_bottom(&mut terminal) < 40,
            "ratatui keeps the viewport row fixed across a grow"
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
