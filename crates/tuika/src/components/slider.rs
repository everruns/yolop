//! [`Slider`] — a one-row value picker over a numeric range.
//!
//! Like the other interactive widgets, state and view are split:
//! [`SliderState`] owns the value, range, and step and applies edits (via
//! [`SliderState::handle`] or the explicit methods); [`Slider`] borrows it and
//! renders a filled track with a thumb. This is the `StatefulWidget` idiom — the
//! value survives across frames in the host, the view is rebuilt each frame.

use ratatui_core::layout::Rect;
use ratatui_core::style::{Modifier, Style};

use crate::event::{Event, EventFlow, KeyCode};
use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// The value, range, and step of a [`Slider`], persisted by the host.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SliderState {
    value: f32,
    min: f32,
    max: f32,
    step: f32,
}

impl Default for SliderState {
    /// A `0.0..=1.0` slider stepping by `0.1`, starting at `0.0`.
    fn default() -> Self {
        Self::new(0.0, 1.0, 0.0)
    }
}

impl SliderState {
    /// A slider over `min..=max` starting at `value` (clamped into range). The
    /// step defaults to 1/100 of the range; override with [`step`](Self::step).
    /// If `max < min` the two are swapped so the range is always well-formed.
    pub fn new(min: f32, max: f32, value: f32) -> Self {
        let (min, max) = if max < min { (max, min) } else { (min, max) };
        let step = ((max - min) / 100.0).max(f32::MIN_POSITIVE);
        Self {
            value: value.clamp(min, max),
            min,
            max,
            step,
        }
    }

    /// Set the increment applied by [`increment`](Self::increment) /
    /// [`decrement`](Self::decrement) and the arrow keys. Non-positive steps are
    /// ignored (the default step is kept).
    pub fn step(mut self, step: f32) -> Self {
        if step > 0.0 {
            self.step = step;
        }
        self
    }

    /// The current value.
    pub fn value(&self) -> f32 {
        self.value
    }

    /// The range lower bound.
    pub fn min(&self) -> f32 {
        self.min
    }

    /// The range upper bound.
    pub fn max(&self) -> f32 {
        self.max
    }

    /// Position within the range as `0.0..=1.0` (0 when the range is degenerate).
    pub fn ratio(&self) -> f32 {
        let span = self.max - self.min;
        if span <= 0.0 {
            0.0
        } else {
            ((self.value - self.min) / span).clamp(0.0, 1.0)
        }
    }

    /// Set the value directly, clamped into range.
    pub fn set_value(&mut self, value: f32) {
        self.value = value.clamp(self.min, self.max);
    }

    /// Set the value from a `0.0..=1.0` position within the range — the seam a
    /// host uses to map a click/drag on the track to a value.
    pub fn set_ratio(&mut self, ratio: f32) {
        let r = ratio.clamp(0.0, 1.0);
        self.set_value(self.min + r * (self.max - self.min));
    }

    /// Nudge the value up by one step (clamped to `max`).
    pub fn increment(&mut self) {
        self.set_value(self.value + self.step);
    }

    /// Nudge the value down by one step (clamped to `min`).
    pub fn decrement(&mut self) {
        self.set_value(self.value - self.step);
    }

    /// Apply a key: Right/Up step up, Left/Down step down, PageUp/PageDown jump
    /// ten steps, Home/End snap to the bounds. Returns whether the key was
    /// consumed. Modified keys and everything else are ignored so they can bubble.
    pub fn handle(&mut self, event: &Event) -> EventFlow {
        let Event::Key(key) = event else {
            return EventFlow::Ignored;
        };
        if !key.plain() {
            return EventFlow::Ignored;
        }
        match key.code {
            KeyCode::Right | KeyCode::Up => self.increment(),
            KeyCode::Left | KeyCode::Down => self.decrement(),
            KeyCode::PageUp => self.set_value(self.value + self.step * 10.0),
            KeyCode::PageDown => self.set_value(self.value - self.step * 10.0),
            KeyCode::Home => self.value = self.min,
            KeyCode::End => self.value = self.max,
            _ => return EventFlow::Ignored,
        }
        EventFlow::Consumed
    }
}

/// A one-row slider rendered from a [`SliderState`]: a filled track with a thumb,
/// and an optional trailing value label.
///
/// The filled portion and thumb use the theme accent, the remaining track the
/// theme's dim color — the same palette split as [`ProgressBar`](crate::ProgressBar).
/// Like a progress bar it fills the width it's given, so place it in a
/// [`fixed`](crate::Flex) row or let a flex child grow.
pub struct Slider {
    ratio: f32,
    label: Option<String>,
}

impl Slider {
    /// A slider view snapshotting `state` for this frame.
    pub fn new(state: &SliderState) -> Self {
        Self {
            ratio: state.ratio(),
            label: None,
        }
    }

    /// Append a right-aligned value label (e.g. ` 0.42`), formatted from `state`.
    /// Whole numbers render without a fractional part.
    pub fn label(mut self, state: &SliderState) -> Self {
        self.label = Some(format_value(state.value()));
        self
    }

    /// Append an explicit trailing label instead of the formatted value.
    pub fn label_text(mut self, text: impl Into<String>) -> Self {
        self.label = Some(text.into());
        self
    }
}

/// Format a slider value compactly: an integer when it has no fractional part,
/// otherwise two decimals.
fn format_value(value: f32) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value:.2}")
    }
}

impl View for Slider {
    fn measure(&self, available: Size) -> Size {
        Size::new(available.width, 1)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let accent = Style::default().fg(ctx.theme.accent);
        let track = Style::default().fg(ctx.theme.dim);

        // Reserve a " label" suffix (with a leading space) when present.
        let suffix = self
            .label
            .as_ref()
            .map(|l| format!(" {l}"))
            .filter(|_| area.width > 4);
        let suffix_w = suffix
            .as_ref()
            .map(|s| crate::width::str_cols(s))
            .unwrap_or(0);
        let bar_w = area.width.saturating_sub(suffix_w);
        if bar_w == 0 {
            return;
        }

        // Thumb sits at the cell nearest the ratio across the track.
        let last = bar_w.saturating_sub(1);
        let thumb = (self.ratio * last as f32).round() as u16;
        for i in 0..bar_w {
            let x = area.x + i;
            if i == thumb {
                surface.set(x, area.y, '●', accent.add_modifier(Modifier::BOLD));
            } else if i < thumb {
                surface.set(x, area.y, '━', accent);
            } else {
                surface.set(x, area.y, '─', track);
            }
        }

        if let Some(suffix) = suffix {
            surface.set_string(
                area.x + bar_w,
                area.y,
                &suffix,
                Style::default().fg(ctx.theme.muted),
            );
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

    fn key(code: KeyCode) -> Event {
        Event::Key(Key::new(code))
    }

    #[test]
    fn state_clamps_and_reports_ratio() {
        let mut s = SliderState::new(0.0, 10.0, 5.0).step(1.0);
        assert!((s.ratio() - 0.5).abs() < 1e-6);
        s.set_value(100.0);
        assert_eq!(s.value(), 10.0);
        s.set_value(-100.0);
        assert_eq!(s.value(), 0.0);
        // A reversed range is normalized.
        let r = SliderState::new(10.0, 0.0, 3.0);
        assert_eq!(r.min(), 0.0);
        assert_eq!(r.max(), 10.0);
    }

    #[test]
    fn set_ratio_maps_into_range() {
        let mut s = SliderState::new(0.0, 200.0, 0.0);
        s.set_ratio(0.25);
        assert_eq!(s.value(), 50.0);
        s.set_ratio(2.0); // clamped
        assert_eq!(s.value(), 200.0);
    }

    #[test]
    fn keys_step_page_and_snap() {
        let mut s = SliderState::new(0.0, 100.0, 50.0).step(1.0);
        assert_eq!(s.handle(&key(KeyCode::Right)), EventFlow::Consumed);
        assert_eq!(s.value(), 51.0);
        s.handle(&key(KeyCode::Left));
        s.handle(&key(KeyCode::Left));
        assert_eq!(s.value(), 49.0);
        s.handle(&key(KeyCode::PageUp));
        assert_eq!(s.value(), 59.0); // ten steps
        s.handle(&key(KeyCode::Home));
        assert_eq!(s.value(), 0.0);
        s.handle(&key(KeyCode::End));
        assert_eq!(s.value(), 100.0);
        // Unrelated keys bubble.
        assert_eq!(s.handle(&key(KeyCode::Char('x'))), EventFlow::Ignored);
    }

    #[test]
    fn renders_filled_track_thumb_and_label() {
        let theme = Theme::default();
        let state = SliderState::new(0.0, 1.0, 0.5);
        let view = Slider::new(&state).label(&state);
        let buf = crate::testing::render(&view, 20, 1, &theme);
        let line = row(&buf, 0);
        assert!(line.contains('●'), "thumb present: {line:?}");
        assert!(line.contains('━'), "filled portion present: {line:?}");
        assert!(line.contains('─'), "empty track present: {line:?}");
        assert!(line.contains("0.5"), "value label present: {line:?}");
    }

    #[test]
    fn thumb_and_fill_use_theme_accent() {
        let t = rainbow_theme();
        let ctx = RenderCtx::new(&t);
        let state = SliderState::new(0.0, 1.0, 1.0); // full → thumb at the end
        let mut buf = buffer(10, 1);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        Slider::new(&state).render(area, &mut surface, &ctx);
        // The first cell is filled track in accent; last is the thumb in accent.
        assert_eq!(buf[(0, 0)].fg, t.accent);
        assert_eq!(buf[(9, 0)].symbol(), "●");
        assert_eq!(buf[(9, 0)].fg, t.accent);
    }

    #[test]
    fn degenerate_widths_do_not_panic() {
        let theme = Theme::default();
        let state = SliderState::new(0.0, 1.0, 0.5);
        for w in [0u16, 1, 2] {
            let _ = crate::testing::render(&Slider::new(&state).label(&state), w, 1, &theme);
        }
    }
}
