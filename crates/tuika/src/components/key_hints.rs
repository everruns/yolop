//! Compact, responsive key/action hints.

use ratatui_core::layout::Rect;
use ratatui_core::style::{Color, Style};

use crate::width::str_cols;
use crate::{RenderCtx, Size, Surface, View};

/// A one-line sequence of styled key and action labels.
pub struct KeyHints {
    hints: Vec<(String, String)>,
}

impl KeyHints {
    /// Build hints from `(key, action)` pairs.
    pub fn new<K, L>(hints: impl IntoIterator<Item = (K, L)>) -> Self
    where
        K: Into<String>,
        L: Into<String>,
    {
        Self {
            hints: hints
                .into_iter()
                .map(|(key, label)| (key.into(), label.into()))
                .collect(),
        }
    }
}

impl View for KeyHints {
    fn measure(&self, available: Size) -> Size {
        let width = self
            .hints
            .iter()
            .map(|(key, label)| (str_cols(key) + str_cols(label)) as usize + 3)
            .sum::<usize>()
            .min(available.width as usize) as u16;
        Size::new(width, u16::from(available.height > 0))
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.is_empty() {
            return;
        }
        let mut x = area.x;
        for (index, (key, label)) in self.hints.iter().enumerate() {
            if index > 0 {
                x = surface.set_string(x, area.y, "  ", ctx.theme.muted_style());
            }
            x = surface.set_string(
                x,
                area.y,
                &format!(" {key} "),
                Style::default().fg(Color::Black).bg(ctx.theme.accent),
            );
            x = surface.set_string(x, area.y, " ", ctx.theme.muted_style());
            x = surface.set_string(x, area.y, label, ctx.theme.muted_style());
            if x >= area.right() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Size;
    use crate::view::View;

    #[test]
    fn key_hints_measure_unicode_by_terminal_width() {
        let hints = KeyHints::new([("⌘", "開く")]);
        assert_eq!(hints.measure(Size::new(20, 1)), Size::new(8, 1));
    }
}
