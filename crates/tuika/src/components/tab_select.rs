//! [`TabSelect`] — a horizontal segmented control that *selects a value*.
//!
//! Where [`Tabs`](crate::Tabs) is navigation chrome (a strip you move a focus
//! ring across, the host deciding what a switch means), `TabSelect` is an input:
//! moving the cursor changes the selected value immediately, and Enter/Space
//! activates it. Its [`handle`](TabSelectState::handle) returns a
//! [`TabSelectOutcome`] so the host can react to a change versus an activation —
//! the value-select analog of OpenTUI's `TabSelect`.

use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::Line;

use crate::components::text::line_width;
use crate::event::{Event, KeyCode};
use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// The result of feeding a key to a [`TabSelectState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabSelectOutcome {
    /// Navigation moved the selection to this index (its value changed).
    Changed(usize),
    /// The current index was activated (Enter/Space) without moving.
    Activated(usize),
    /// The key was not handled; let it bubble.
    Ignored,
}

/// Host-owned selected-option state for a [`TabSelect`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TabSelectState {
    selected: usize,
}

impl TabSelectState {
    /// A state with `index` selected.
    pub fn with_selected(index: usize) -> Self {
        Self { selected: index }
    }

    /// The selected option index.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Select `index`, clamped to `len` options.
    pub fn select(&mut self, index: usize, len: usize) {
        self.selected = index.min(len.saturating_sub(1));
    }

    /// Apply a key over `len` options: Left/BackTab and Right/Tab move the
    /// selection (wrapping), Enter/Space activates the current option. Wrapping
    /// navigation reports [`TabSelectOutcome::Changed`]; activation reports
    /// [`TabSelectOutcome::Activated`]; anything else is
    /// [`TabSelectOutcome::Ignored`].
    pub fn handle(&mut self, event: &Event, len: usize) -> TabSelectOutcome {
        if len == 0 {
            return TabSelectOutcome::Ignored;
        }
        let Event::Key(key) = event else {
            return TabSelectOutcome::Ignored;
        };
        if !key.plain() {
            return TabSelectOutcome::Ignored;
        }
        // Keep the selection in range even if `len` shrank since it was set.
        self.selected = self.selected.min(len - 1);
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.selected = self.selected.checked_sub(1).unwrap_or(len - 1);
                TabSelectOutcome::Changed(self.selected)
            }
            KeyCode::Right | KeyCode::Tab => {
                self.selected = (self.selected + 1) % len;
                TabSelectOutcome::Changed(self.selected)
            }
            KeyCode::Enter | KeyCode::Char(' ') => TabSelectOutcome::Activated(self.selected),
            _ => TabSelectOutcome::Ignored,
        }
    }
}

/// A one-row segmented control derived from a [`TabSelectState`]: each option is
/// a padded pill, the selected one drawn in the theme's selection style.
///
/// ```
/// use tuika::{Event, Key, KeyCode, TabSelectOutcome, TabSelectState};
/// let mut state = TabSelectState::default();
/// let right = Event::Key(Key::new(KeyCode::Right));
/// assert_eq!(state.handle(&right, 3), TabSelectOutcome::Changed(1));
/// let enter = Event::Key(Key::new(KeyCode::Enter));
/// assert_eq!(state.handle(&enter, 3), TabSelectOutcome::Activated(1));
/// ```
pub struct TabSelect {
    labels: Vec<Line<'static>>,
    selected: usize,
}

impl TabSelect {
    /// Build the control, snapshotting the selected index for this frame.
    pub fn new(labels: Vec<Line<'static>>, state: &TabSelectState) -> Self {
        Self {
            labels,
            selected: state.selected(),
        }
    }
}

/// One cell of horizontal padding on each side of a pill's label.
const PILL_PAD: u16 = 1;

impl View for TabSelect {
    fn measure(&self, available: Size) -> Size {
        // Each pill is label width plus a cell of padding on each side; pills abut
        // with no separator.
        let width = self
            .labels
            .iter()
            .map(|l| line_width(l).saturating_add(PILL_PAD * 2))
            .fold(0u16, u16::saturating_add);
        Size::new(width.min(available.width), u16::from(available.height > 0))
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.is_empty() {
            return;
        }
        let mut x = area.x;
        for (index, label) in self.labels.iter().enumerate() {
            if x >= area.right() {
                break;
            }
            let selected = index == self.selected;
            let base = if selected {
                ctx.theme.selection_style()
            } else {
                Style::default().fg(ctx.theme.muted)
            };
            // Leading pad.
            x = surface.set_string(x, area.y, " ", base);
            for span in &label.spans {
                if x >= area.right() {
                    break;
                }
                let style = if selected {
                    base.add_modifier(Modifier::BOLD)
                } else {
                    span.style.patch(base)
                };
                x = surface.set_string(x, area.y, span.content.as_ref(), style);
            }
            // Trailing pad.
            x = surface.set_string(x, area.y, " ", base);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, Key, KeyCode};
    use crate::style::Theme;
    use crate::test_support::{rainbow_theme, row};
    use ratatui_core::text::Line;

    fn key(code: KeyCode) -> Event {
        Event::Key(Key::new(code))
    }

    #[test]
    fn navigation_changes_value_and_wraps() {
        let mut s = TabSelectState::default();
        assert_eq!(
            s.handle(&key(KeyCode::Right), 3),
            TabSelectOutcome::Changed(1)
        );
        assert_eq!(
            s.handle(&key(KeyCode::Right), 3),
            TabSelectOutcome::Changed(2)
        );
        // Wraps forward past the end...
        assert_eq!(
            s.handle(&key(KeyCode::Right), 3),
            TabSelectOutcome::Changed(0)
        );
        // ...and backward past the start.
        assert_eq!(
            s.handle(&key(KeyCode::Left), 3),
            TabSelectOutcome::Changed(2)
        );
    }

    #[test]
    fn enter_and_space_activate_without_moving() {
        let mut s = TabSelectState::with_selected(1);
        assert_eq!(
            s.handle(&key(KeyCode::Enter), 3),
            TabSelectOutcome::Activated(1)
        );
        assert_eq!(
            s.handle(&key(KeyCode::Char(' ')), 3),
            TabSelectOutcome::Activated(1)
        );
        assert_eq!(s.selected(), 1);
        // Unhandled keys bubble.
        assert_eq!(
            s.handle(&key(KeyCode::Char('x')), 3),
            TabSelectOutcome::Ignored
        );
        assert_eq!(s.handle(&key(KeyCode::Up), 0), TabSelectOutcome::Ignored);
    }

    #[test]
    fn renders_pills_with_selected_highlight() {
        let t = rainbow_theme();
        let state = TabSelectState::with_selected(1);
        let view = TabSelect::new(
            vec![Line::from("low"), Line::from("mid"), Line::from("high")],
            &state,
        );
        let buf = crate::testing::render(&view, 30, 1, &t);
        let line = row(&buf, 0);
        assert!(line.contains("low"));
        assert!(line.contains("mid"));
        assert!(line.contains("high"));
        // The selected pill ("mid") paints the selection background somewhere.
        let has_selection_bg = (0..buf.area.width).any(|x| buf[(x, 0)].bg == t.selection_bg);
        assert!(has_selection_bg, "selected pill uses selection bg");
    }

    #[test]
    fn empty_and_narrow_do_not_panic() {
        let theme = Theme::default();
        let state = TabSelectState::default();
        let _ = crate::testing::render(&TabSelect::new(vec![], &state), 10, 1, &theme);
        let view = TabSelect::new(vec![Line::from("wide-label")], &state);
        let _ = crate::testing::render(&view, 3, 1, &theme);
    }
}
