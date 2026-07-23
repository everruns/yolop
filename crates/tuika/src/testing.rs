//! Public helpers for hermetic view and resize tests.

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::{Theme, View, paint};

/// Render `view` into a zero-origin in-memory buffer.
pub fn render(view: &dyn View, width: u16, height: u16, theme: &Theme) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    paint(&mut buffer, area, theme, view, &[]);
    buffer
}

/// Convert a buffer to a stable glyph grid suitable for snapshot assertions.
pub fn grid(buffer: &Buffer) -> String {
    let mut output = String::new();
    for y in buffer.area.y..buffer.area.bottom() {
        for x in buffer.area.x..buffer.area.right() {
            output.push_str(buffer[(x, y)].symbol());
        }
        if y + 1 < buffer.area.bottom() {
            output.push('\n');
        }
    }
    output
}

/// Render the same view at several sizes, useful for resize and degenerate-size tests.
pub fn render_sizes(
    view: &dyn View,
    sizes: impl IntoIterator<Item = (u16, u16)>,
    theme: &Theme,
) -> Vec<Buffer> {
    sizes
        .into_iter()
        .map(|(width, height)| render(view, width, height, theme))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Text;
    use crate::style::Theme;

    #[test]
    fn public_testing_grid_is_stable_and_rectangular() {
        let theme = Theme::default();
        let rendered = render(&Text::raw("hi"), 3, 2, &theme);
        assert_eq!(grid(&rendered), "hi \n   ");
    }
}
