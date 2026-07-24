//! Focus-independent tab navigation with host-owned selection state.

use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::Line;

use crate::{Event, EventFlow, KeyCode, RenderCtx, Size, Surface, View};

use super::text::line_width;

#[derive(Clone, Debug, Default)]
/// Host-owned selected-tab state.
pub struct TabsState {
    selected: usize,
}

impl TabsState {
    /// Current selected tab index.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Select an index, clamped to the available tab count.
    pub fn select(&mut self, index: usize, len: usize) {
        self.selected = index.min(len.saturating_sub(1));
    }

    /// Handle left/right and forward/backward tab navigation.
    pub fn handle(&mut self, event: &Event, len: usize) -> EventFlow {
        if len == 0 {
            return EventFlow::Ignored;
        }
        let Event::Key(key) = event else {
            return EventFlow::Ignored;
        };
        if !key.plain() {
            return EventFlow::Ignored;
        }
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.selected = self.selected.checked_sub(1).unwrap_or(len - 1);
                EventFlow::Consumed
            }
            KeyCode::Right | KeyCode::Tab => {
                self.selected = (self.selected + 1) % len;
                EventFlow::Consumed
            }
            _ => EventFlow::Ignored,
        }
    }
}

/// A one-line tab strip derived from [`TabsState`].
///
/// ![tabs demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/tabs.gif)
pub struct Tabs {
    labels: Vec<Line<'static>>,
    selected: usize,
}

impl Tabs {
    /// Create a tab strip, snapshotting the selected index for this frame.
    pub fn new(labels: Vec<Line<'static>>, state: &TabsState) -> Self {
        Self {
            labels,
            selected: state.selected(),
        }
    }
}

impl View for Tabs {
    fn measure(&self, available: Size) -> Size {
        let labels = self
            .labels
            .iter()
            .map(line_width)
            .fold(0, u16::saturating_add);
        let gaps = (self.labels.len().saturating_sub(1) as u16).saturating_mul(2);
        Size::new(
            labels.saturating_add(gaps).min(available.width),
            u16::from(available.height > 0),
        )
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.is_empty() {
            return;
        }
        let mut x = area.x;
        for (index, label) in self.labels.iter().enumerate() {
            if index > 0 {
                x = surface.set_string(x, area.y, "  ", ctx.theme.muted_style());
            }
            let selected = index == self.selected;
            for span in &label.spans {
                let style = if selected {
                    span.style.patch(
                        ctx.theme
                            .accent_style()
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    )
                } else {
                    span.style.patch(Style::default().fg(ctx.theme.muted))
                };
                x = surface.set_string(x, area.y, span.content.as_ref(), style);
            }
            if x >= area.right() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventFlow, Key, KeyCode};
    use crate::style::Theme;
    use crate::test_support::row;
    use ratatui_core::text::Line;

    #[test]
    fn tabs_state_wraps_and_tabs_render_selection() {
        let mut state = TabsState::default();
        assert_eq!(
            state.handle(&Event::Key(Key::new(KeyCode::Left)), 3),
            EventFlow::Consumed
        );
        assert_eq!(state.selected(), 2);

        let tabs = Tabs::new(
            vec![Line::from("one"), Line::from("two"), Line::from("three")],
            &state,
        );
        let theme = Theme::default();
        let rendered = crate::testing::render(&tabs, 20, 1, &theme);
        assert!(row(&rendered, 0).contains("one  two  three"));
        assert!(
            rendered[(10, 0)]
                .modifier
                .contains(ratatui_core::style::Modifier::UNDERLINED)
        );
    }
}
