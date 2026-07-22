//! Overlay positioning (Pi's overlay model).
//!
//! An overlay is a view drawn on top of the base tree at an anchored, sized
//! rect. Because the full-screen renderer owns the whole alternate-screen
//! buffer, an overlay is just "paint this last, into this rect, after clearing
//! it" — none of the inline-viewport chrome-bleed the inline path fights. The
//! [`OverlaySpec`] resolves a target [`Rect`] from an [`Anchor`] plus size
//! constraints relative to the screen.

use ratatui::layout::Rect;

use super::geometry::Size;

/// Where an overlay sits relative to the screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Anchor {
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// A size request: an absolute cell count or a percentage of the screen extent,
/// with optional min/max clamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extent {
    Cells(u16),
    Percent(u16),
}

impl Extent {
    fn resolve(self, available: u16) -> u16 {
        match self {
            Extent::Cells(n) => n,
            Extent::Percent(p) => ((available as u32 * p.min(100) as u32) / 100) as u16,
        }
    }
}

/// Anchored, sized overlay placement relative to a screen rect.
///
/// ![overlay demo](https://raw.githubusercontent.com/everruns/yolop/main/crates/tuika/docs/demos/overlay.gif)
#[derive(Clone, Copy, Debug)]
pub struct OverlaySpec {
    pub anchor: Anchor,
    pub width: Extent,
    pub height: Extent,
    pub min_width: u16,
    pub min_height: u16,
    pub max_width: u16,
    pub max_height: u16,
    pub margin: u16,
}

impl OverlaySpec {
    /// A centered overlay sized as a percentage of the screen.
    pub fn centered(width_pct: u16, height_pct: u16) -> Self {
        Self {
            anchor: Anchor::Center,
            width: Extent::Percent(width_pct),
            height: Extent::Percent(height_pct),
            min_width: 0,
            min_height: 0,
            max_width: u16::MAX,
            max_height: u16::MAX,
            margin: 0,
        }
    }

    pub fn anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    pub fn min_size(mut self, width: u16, height: u16) -> Self {
        self.min_width = width;
        self.min_height = height;
        self
    }

    pub fn max_size(mut self, width: u16, height: u16) -> Self {
        self.max_width = width;
        self.max_height = height;
        self
    }

    pub fn margin(mut self, margin: u16) -> Self {
        self.margin = margin;
        self
    }

    /// Resolve the overlay rect within `screen`.
    pub fn resolve(&self, screen: Rect) -> Rect {
        let avail = Size::new(
            screen.width.saturating_sub(self.margin.saturating_mul(2)),
            screen.height.saturating_sub(self.margin.saturating_mul(2)),
        );
        let w = self.width.resolve(avail.width).clamp(
            self.min_width.min(avail.width),
            self.max_width.min(avail.width),
        );
        let h = self.height.resolve(avail.height).clamp(
            self.min_height.min(avail.height),
            self.max_height.min(avail.height),
        );

        let inner = Rect {
            x: screen.x.saturating_add(self.margin),
            y: screen.y.saturating_add(self.margin),
            width: avail.width,
            height: avail.height,
        };
        let (x, y) = self.position(inner, w, h);
        // Clamp into the screen: a margin wider than the screen would otherwise
        // place `inner` (and the overlay) past the edge.
        let x = x.min(screen.right().saturating_sub(w)).max(screen.x);
        let y = y.min(screen.bottom().saturating_sub(h)).max(screen.y);
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn position(&self, inner: Rect, w: u16, h: u16) -> (u16, u16) {
        let free_x = inner.width.saturating_sub(w);
        let free_y = inner.height.saturating_sub(h);
        let (left, mid_x, right) = (inner.x, inner.x + free_x / 2, inner.x + free_x);
        let (top, mid_y, bottom) = (inner.y, inner.y + free_y / 2, inner.y + free_y);
        match self.anchor {
            Anchor::Center => (mid_x, mid_y),
            Anchor::Top => (mid_x, top),
            Anchor::Bottom => (mid_x, bottom),
            Anchor::Left => (left, mid_y),
            Anchor::Right => (right, mid_y),
            Anchor::TopLeft => (left, top),
            Anchor::TopRight => (right, top),
            Anchor::BottomLeft => (left, bottom),
            Anchor::BottomRight => (right, bottom),
        }
    }
}
