//! Geometry helpers shared across `tuika`.
//!
//! `tuika` reuses ratatui's [`Rect`] as its area type so the compositor can
//! draw straight into a ratatui [`Buffer`](ratatui_core::buffer::Buffer), but the
//! layout solver reasons about a direction-agnostic main/cross axis. The
//! helpers here bridge the two: an [`Axis`] projects a `Rect` onto its main or
//! cross extent so the flex solver can be written once for rows and columns.

use ratatui_core::layout::Rect;

/// An intrinsic size in terminal cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    /// Width in terminal cells.
    pub width: u16,
    /// Height in terminal cells.
    pub height: u16,
}

impl Size {
    /// A zero-by-zero size.
    pub const ZERO: Size = Size {
        width: 0,
        height: 0,
    };

    /// A size of `width` by `height` cells.
    pub fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    /// Clamp both extents so the size fits inside `bounds`.
    pub fn clamp_to(self, bounds: Size) -> Size {
        Size {
            width: self.width.min(bounds.width),
            height: self.height.min(bounds.height),
        }
    }
}

impl From<Rect> for Size {
    fn from(rect: Rect) -> Self {
        Size {
            width: rect.width,
            height: rect.height,
        }
    }
}

/// The layout axis a flex container distributes children along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Main axis runs left-to-right; children stack in a row.
    Horizontal,
    /// Main axis runs top-to-bottom; children stack in a column.
    Vertical,
}

impl Axis {
    /// Extent of `size` along this axis (the "main" extent).
    pub fn main(self, size: Size) -> u16 {
        match self {
            Axis::Horizontal => size.width,
            Axis::Vertical => size.height,
        }
    }

    /// Extent of `size` across this axis (the "cross" extent).
    pub fn cross(self, size: Size) -> u16 {
        match self {
            Axis::Horizontal => size.height,
            Axis::Vertical => size.width,
        }
    }

    /// Build a `Size` from main/cross extents on this axis.
    pub fn size(self, main: u16, cross: u16) -> Size {
        match self {
            Axis::Horizontal => Size::new(main, cross),
            Axis::Vertical => Size::new(cross, main),
        }
    }

    /// Place a child rect at `main`/`cross` offsets with `main_len`/`cross_len`
    /// extents, relative to `origin`.
    pub fn place(self, origin: Rect, main: u16, cross: u16, main_len: u16, cross_len: u16) -> Rect {
        match self {
            Axis::Horizontal => Rect {
                x: origin.x.saturating_add(main),
                y: origin.y.saturating_add(cross),
                width: main_len,
                height: cross_len,
            },
            Axis::Vertical => Rect {
                x: origin.x.saturating_add(cross),
                y: origin.y.saturating_add(main),
                width: cross_len,
                height: main_len,
            },
        }
    }
}

/// Symmetric-per-edge padding, in cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Padding {
    /// Cells inset from the left edge.
    pub left: u16,
    /// Cells inset from the right edge.
    pub right: u16,
    /// Cells inset from the top edge.
    pub top: u16,
    /// Cells inset from the bottom edge.
    pub bottom: u16,
}

impl Padding {
    /// No padding on any edge.
    pub const ZERO: Padding = Padding {
        left: 0,
        right: 0,
        top: 0,
        bottom: 0,
    };

    /// Uniform padding on every edge.
    pub fn all(value: u16) -> Self {
        Self {
            left: value,
            right: value,
            top: value,
            bottom: value,
        }
    }

    /// Independent horizontal and vertical padding.
    pub fn symmetric(horizontal: u16, vertical: u16) -> Self {
        Self {
            left: horizontal,
            right: horizontal,
            top: vertical,
            bottom: vertical,
        }
    }

    /// Combined left + right padding, in cells.
    pub fn horizontal(self) -> u16 {
        self.left.saturating_add(self.right)
    }

    /// Combined top + bottom padding, in cells.
    pub fn vertical(self) -> u16 {
        self.top.saturating_add(self.bottom)
    }

    /// Shrink `area` by this padding, saturating so it never inverts. The
    /// origin offset is clamped to the area's extent so an area smaller than
    /// its padding stays inside `area` (a zero-size box at the edge) instead of
    /// pushing the inner rect past the right/bottom edge.
    pub fn inner(self, area: Rect) -> Rect {
        let width = area.width.saturating_sub(self.horizontal());
        let height = area.height.saturating_sub(self.vertical());
        Rect {
            x: area.x.saturating_add(self.left.min(area.width)),
            y: area.y.saturating_add(self.top.min(area.height)),
            width,
            height,
        }
    }
}
