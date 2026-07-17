//! Loader — spinner + message + optional cancel hint on one row.
//!
//! Pi's `BorderedLoader` analog. Kept borderless so callers compose it with
//! [`Boxed`](super::Boxed) when they want a frame; on its own it fits inline in
//! a status row or overlay.

use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::tuika::geometry::Size;
use crate::tuika::surface::Surface;
use crate::tuika::view::{RenderCtx, View};

use super::spinner::{Spinner, SpinnerStyle};

/// An animated loading row.
pub struct Loader {
    frame: u64,
    message: String,
    hint: Option<String>,
    spinner_style: SpinnerStyle,
}

impl Loader {
    pub fn new(frame: u64, message: impl Into<String>) -> Self {
        Self {
            frame,
            message: message.into(),
            hint: None,
            spinner_style: SpinnerStyle::default(),
        }
    }

    /// A trailing dim hint, e.g. "esc to cancel".
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn spinner_style(mut self, style: SpinnerStyle) -> Self {
        self.spinner_style = style;
        self
    }
}

impl View for Loader {
    fn measure(&self, available: Size) -> Size {
        Size::new(available.width, 1)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // Spinner in the first cell.
        Spinner::new(self.frame).style(self.spinner_style).render(
            Rect::new(area.x, area.y, 1, 1),
            surface,
            ctx,
        );

        let mut x = area.x.saturating_add(2);
        x = surface.set_string(x, area.y, &self.message, ctx.theme.text_style());

        if let Some(hint) = &self.hint {
            let hint = format!("  {hint}");
            let hint_w = hint.len() as u16;
            // Right-align the hint if it fits past the message.
            let hx = area.right().saturating_sub(hint_w);
            if hx > x {
                surface.set_string(hx, area.y, &hint, ctx.theme.muted_style());
            }
        }
    }
}
