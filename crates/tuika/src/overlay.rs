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
    /// Centered horizontally and vertically.
    #[default]
    Center,
    /// Centered horizontally, flush to the top edge.
    Top,
    /// Centered horizontally, flush to the bottom edge.
    Bottom,
    /// Centered vertically, flush to the left edge.
    Left,
    /// Centered vertically, flush to the right edge.
    Right,
    /// Flush to the top-left corner.
    TopLeft,
    /// Flush to the top-right corner.
    TopRight,
    /// Flush to the bottom-left corner.
    BottomLeft,
    /// Flush to the bottom-right corner.
    BottomRight,
}

/// A size request: an absolute cell count or a percentage of the screen extent,
/// with optional min/max clamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extent {
    /// An absolute size in terminal cells.
    Cells(u16),
    /// A percentage (0–100) of the available screen extent.
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
/// Resolve a [`Rect`] for the current screen, then paint a view into it last
/// (see [`paint`](crate::paint)); the host routes input to whichever layer owns
/// focus.
///
/// # Examples
///
/// ```
/// use tuika::OverlaySpec;
/// use ratatui::layout::Rect;
///
/// // A centered dialog at 50% × 40% of a 100 × 40 screen, never smaller than 34 × 7.
/// let rect = OverlaySpec::centered(50, 40).min_size(34, 7).resolve(Rect::new(0, 0, 100, 40));
/// assert_eq!((rect.width, rect.height), (50, 16));
/// assert_eq!((rect.x, rect.y), (25, 12));
/// ```
#[derive(Clone, Copy, Debug)]
pub struct OverlaySpec {
    /// Where the resolved rect sits within the screen.
    pub anchor: Anchor,
    /// Requested width, absolute or a percentage of the screen width.
    pub width: Extent,
    /// Requested height, absolute or a percentage of the screen height.
    pub height: Extent,
    /// Lower clamp on the resolved width, in cells.
    pub min_width: u16,
    /// Lower clamp on the resolved height, in cells.
    pub min_height: u16,
    /// Upper clamp on the resolved width, in cells.
    pub max_width: u16,
    /// Upper clamp on the resolved height, in cells.
    pub max_height: u16,
    /// Gap in cells kept between the overlay and every screen edge.
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

    /// Set the anchor that positions the overlay within the screen.
    pub fn anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    /// Set the minimum resolved size in cells.
    pub fn min_size(mut self, width: u16, height: u16) -> Self {
        self.min_width = width;
        self.min_height = height;
        self
    }

    /// Set the maximum resolved size in cells.
    pub fn max_size(mut self, width: u16, height: u16) -> Self {
        self.max_width = width;
        self.max_height = height;
        self
    }

    /// Set the gap in cells kept between the overlay and every screen edge.
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn overlay_centered_percentage() {
        let screen = Rect::new(0, 0, 100, 40);
        let spec = OverlaySpec::centered(50, 50);
        let rect = spec.resolve(screen);
        assert_eq!(rect.width, 50);
        assert_eq!(rect.height, 20);
        assert_eq!(rect.x, 25);
        assert_eq!(rect.y, 10);
    }

    #[test]
    fn overlay_anchors_to_corner_with_margin() {
        let screen = Rect::new(0, 0, 100, 40);
        let spec = OverlaySpec {
            anchor: Anchor::BottomRight,
            width: Extent::Cells(20),
            height: Extent::Cells(10),
            min_width: 0,
            min_height: 0,
            max_width: u16::MAX,
            max_height: u16::MAX,
            margin: 2,
        };
        let rect = spec.resolve(screen);
        assert_eq!(rect.width, 20);
        assert_eq!(rect.height, 10);
        // Bottom-right inside a 2-cell margin: right edge at 98, bottom at 38.
        assert_eq!(rect.right(), 98);
        assert_eq!(rect.bottom(), 38);
    }

    #[test]
    fn overlay_clamps_to_max() {
        let screen = Rect::new(0, 0, 100, 40);
        let spec = OverlaySpec::centered(90, 90).max_size(40, 20);
        let rect = spec.resolve(screen);
        assert_eq!(rect.width, 40);
        assert_eq!(rect.height, 20);
    }
}
