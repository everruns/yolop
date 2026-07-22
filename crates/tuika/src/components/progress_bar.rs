//! Progress bar — determinate (sub-cell precision) or indeterminate (marquee).
//!
//! Determinate bars fill by eighths using partial block glyphs, so a 20-cell
//! bar shows 160 distinct levels. Indeterminate bars slide a bright segment
//! across a dim track, driven by the host frame counter (see [`crate::anim`]).

use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::anim;
use crate::geometry::Size;
use crate::surface::Surface;
use crate::view::{RenderCtx, View};

/// Left-to-right eighth-block fill glyphs, index 0 = empty .. 8 = full.
const EIGHTHS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// A single-row progress bar.
///
/// ![progress_bar demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/progress_bar.gif)
pub struct ProgressBar {
    /// `Some(0.0..=1.0)` for a determinate bar; `None` for indeterminate.
    fraction: Option<f32>,
    /// Host frame counter, used to animate the indeterminate marquee.
    frame: u64,
    /// Append a right-aligned `NN%` label to a determinate bar.
    show_percent: bool,
    filled: Option<ratatui::style::Color>,
    track: Option<ratatui::style::Color>,
}

impl ProgressBar {
    /// A determinate bar filled to `fraction` (clamped to `0.0..=1.0`).
    pub fn determinate(fraction: f32) -> Self {
        Self {
            fraction: Some(fraction.clamp(0.0, 1.0)),
            frame: 0,
            show_percent: false,
            filled: None,
            track: None,
        }
    }

    /// An indeterminate bar whose marquee position follows `frame`.
    pub fn indeterminate(frame: u64) -> Self {
        Self {
            fraction: None,
            frame,
            show_percent: false,
            filled: None,
            track: None,
        }
    }

    /// Show a trailing `NN%` (determinate bars only).
    pub fn percent(mut self, show: bool) -> Self {
        self.show_percent = show;
        self
    }

    pub fn colors(mut self, filled: ratatui::style::Color, track: ratatui::style::Color) -> Self {
        self.filled = Some(filled);
        self.track = Some(track);
        self
    }

    /// Percent integer for the current fraction, or `None` when indeterminate.
    pub fn percent_value(&self) -> Option<u8> {
        self.fraction.map(|f| (f * 100.0).round() as u8)
    }

    fn render_determinate(
        &self,
        area: Rect,
        surface: &mut Surface,
        ctx: &RenderCtx,
        fraction: f32,
    ) {
        let filled = self.filled.unwrap_or(ctx.theme.accent);
        let track = self.track.unwrap_or(ctx.theme.dim);

        // Reserve space for a " 100%" suffix when requested.
        let suffix = if self.show_percent {
            Some(format!(" {:>3}%", (fraction * 100.0).round() as u16))
        } else {
            None
        };
        let suffix_w = suffix.as_ref().map(|s| s.len() as u16).unwrap_or(0);
        let bar_w = area.width.saturating_sub(suffix_w);
        if bar_w == 0 {
            return;
        }

        // Total fill measured in eighths of a cell across the bar width.
        let total_eighths = (fraction * bar_w as f32 * 8.0).round() as u32;
        let full = (total_eighths / 8) as u16;
        let remainder = (total_eighths % 8) as usize;

        for i in 0..bar_w {
            let x = area.x + i;
            if i < full {
                surface.set(x, area.y, '█', Style::default().fg(filled));
            } else if i == full && remainder > 0 {
                // Partial cell: the fractional glyph is the "filled" color on
                // the track background so it reads as a leading edge.
                surface.set(
                    x,
                    area.y,
                    EIGHTHS[remainder],
                    Style::default().fg(filled).bg(track),
                );
            } else {
                surface.set(x, area.y, ' ', Style::default().bg(track));
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

    fn render_indeterminate(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        let filled = self.filled.unwrap_or(ctx.theme.accent);
        let track = self.track.unwrap_or(ctx.theme.dim);
        let w = area.width;
        // A segment roughly one-third of the bar bounces back and forth.
        let seg = (w / 3).max(1);
        let travel = w.saturating_sub(seg);
        let pos = (anim::ping_pong(self.frame, 60) * travel as f32).round() as u16;

        for i in 0..w {
            let x = area.x + i;
            let within = i >= pos && i < pos.saturating_add(seg);
            if within {
                surface.set(x, area.y, '█', Style::default().fg(filled));
            } else {
                surface.set(x, area.y, '░', Style::default().fg(track));
            }
        }
    }
}

impl View for ProgressBar {
    fn measure(&self, available: Size) -> Size {
        Size::new(available.width, 1)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let row = Rect::new(area.x, area.y, area.width, 1);
        match self.fraction {
            Some(fraction) => self.render_determinate(row, surface, ctx, fraction),
            None => self.render_indeterminate(row, surface, ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Surface;
    use crate::style::Theme;
    use crate::test_support::{buffer, rainbow_theme, row};
    use crate::view::{RenderCtx, View};
    use ratatui::style::Color;

    #[test]
    fn progress_bar_determinate_fills_by_fraction() {
        let bar = ProgressBar::determinate(0.5);
        assert_eq!(bar.percent_value(), Some(50));
        let mut buf = buffer(10, 1);
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        bar.render(area, &mut surface, &ctx);
        // Half of 10 cells fully filled.
        let full = (0..10).filter(|&x| buf[(x, 0)].symbol() == "█").count();
        assert_eq!(full, 5);
    }

    #[test]
    fn progress_bar_full_and_percent_label() {
        let bar = ProgressBar::determinate(1.0).percent(true);
        let mut buf = buffer(20, 1);
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        bar.render(area, &mut surface, &ctx);
        assert!(row(&buf, 0).contains("100%"));
        // Bar area (minus the " 100%" suffix = 5 cols) is fully filled.
        let full = (0..15).filter(|&x| buf[(x, 0)].symbol() == "█").count();
        assert_eq!(full, 15);
    }

    #[test]
    fn progress_bar_indeterminate_has_segment_and_track() {
        let bar = ProgressBar::indeterminate(0);
        let mut buf = buffer(12, 1);
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        bar.render(area, &mut surface, &ctx);
        let seg = (0..12).filter(|&x| buf[(x, 0)].symbol() == "█").count();
        let track = (0..12).filter(|&x| buf[(x, 0)].symbol() == "░").count();
        assert!(seg > 0, "expected a bright segment");
        assert!(track > 0, "expected a dim track");
        assert_eq!(seg + track, 12);
    }

    #[test]
    fn progress_bar_is_responsive_to_width() {
        let theme = Theme::default();
        let ctx = RenderCtx::new(&theme);
        let filled = |w: u16| {
            let mut buf = buffer(w, 1);
            let area = buf.area;
            let mut surface = Surface::new(&mut buf, area);
            ProgressBar::determinate(0.5).render(area, &mut surface, &ctx);
            (0..w).filter(|&x| buf[(x, 0)].symbol() == "█").count()
        };
        assert_eq!(filled(4), 2, "half of 4");
        assert_eq!(filled(40), 20, "half of 40");
        assert!(filled(4) < filled(40), "wider bar fills more cells");
        // Degenerate widths must not panic.
        let _ = filled(0);
        let _ = filled(1);
    }

    #[test]
    fn progress_bar_default_colors_come_from_theme() {
        let t = rainbow_theme();
        let ctx = RenderCtx::new(&t);

        // Determinate: filled fg = accent, empty bg = dim.
        let bar = ProgressBar::determinate(0.5);
        let mut buf = buffer(10, 1);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        bar.render(area, &mut surface, &ctx);
        assert_eq!(buf[(0, 0)].fg, t.accent, "filled cell fg");
        assert_eq!(buf[(9, 0)].bg, t.dim, "empty cell bg");

        // Indeterminate: bright segment fg = accent, track fg = dim.
        let bar = ProgressBar::indeterminate(0);
        let mut buf = buffer(12, 1);
        let area = buf.area;
        let mut surface = Surface::new(&mut buf, area);
        bar.render(area, &mut surface, &ctx);
        let fgs: Vec<Color> = (0..12).map(|x| buf[(x, 0)].fg).collect();
        assert!(fgs.contains(&t.accent), "segment fg accent: {fgs:?}");
        assert!(fgs.contains(&t.dim), "track fg dim: {fgs:?}");
    }
}
