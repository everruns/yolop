//! Flexbox-style layout solver.
//!
//! This is the piece ratatui does not give you: a CSS-flexbox-shaped solver
//! (borrowing OpenTUI's Yoga model and quench's declarative structure, but
//! implemented as plain Rust — no reconciler). A [`Flex`](crate::tuika::components::Flex)
//! container owns a [`LayoutStyle`] and children; [`solve`] resolves child
//! rects for one axis. It is written once against a direction-agnostic
//! [`Axis`] so rows and columns share the same code path.

use ratatui::layout::Rect;

use super::geometry::{Axis, Padding, Size};

/// How a child sizes itself along the container's main axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Dimension {
    /// Size to the child's intrinsic (measured) main extent.
    #[default]
    Auto,
    /// Exactly this many cells.
    Fixed(u16),
    /// This percent (0–100) of the container's available main extent.
    Percent(u16),
    /// Grow to share leftover main space, weighted by this factor.
    Flex(u16),
}

/// Cross-axis alignment of children within the container.
///
/// Defaults to [`Align::Stretch`], matching CSS flexbox and the common TUI case
/// where a column's children should fill its width (and a row's its height).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
    /// Fill the full cross extent.
    #[default]
    Stretch,
}

/// Main-axis distribution of leftover space when no child is [`Dimension::Flex`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    /// Even gaps between children, none at the ends.
    SpaceBetween,
}

/// Direction a flex container stacks its children.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    Row,
    #[default]
    Column,
}

impl Direction {
    pub fn axis(self) -> Axis {
        match self {
            Direction::Row => Axis::Horizontal,
            Direction::Column => Axis::Vertical,
        }
    }
}

/// Declarative layout properties for a flex container.
#[derive(Clone, Copy, Debug, Default)]
pub struct LayoutStyle {
    pub direction: Direction,
    pub padding: Padding,
    pub gap: u16,
    pub align_items: Align,
    pub justify: Justify,
}

impl LayoutStyle {
    pub fn row() -> Self {
        Self {
            direction: Direction::Row,
            ..Self::default()
        }
    }

    pub fn column() -> Self {
        Self {
            direction: Direction::Column,
            ..Self::default()
        }
    }

    pub fn gap(mut self, gap: u16) -> Self {
        self.gap = gap;
        self
    }

    pub fn padding(mut self, padding: Padding) -> Self {
        self.padding = padding;
        self
    }

    pub fn align(mut self, align: Align) -> Self {
        self.align_items = align;
        self
    }

    pub fn justify(mut self, justify: Justify) -> Self {
        self.justify = justify;
        self
    }
}

/// A child's layout request: its main-axis sizing rule and its intrinsic size.
///
/// `intrinsic` is what the child's `measure` reported for the content box; the
/// solver uses it for [`Dimension::Auto`] main sizing and for non-stretch
/// cross alignment.
#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub dimension: Dimension,
    pub intrinsic: Size,
}

impl Item {
    pub fn new(dimension: Dimension, intrinsic: Size) -> Self {
        Self {
            dimension,
            intrinsic,
        }
    }
}

/// Resolve child rects inside `area` for a container with `style` and `items`.
///
/// Returns one `Rect` per item, in order. The algorithm mirrors flexbox on a
/// single line: resolve fixed/percent/auto main sizes, distribute remaining
/// main space to flex children (or to `justify` when there are none), then
/// align each child on the cross axis.
pub fn solve(area: Rect, style: &LayoutStyle, items: &[Item]) -> Vec<Rect> {
    if items.is_empty() {
        return Vec::new();
    }

    let axis = style.direction.axis();
    let inner = style.padding.inner(area);
    let inner_size = Size::from(inner);
    let main_avail = axis.main(inner_size);
    let cross_avail = axis.cross(inner_size);

    let total_gap = style
        .gap
        .saturating_mul(items.len().saturating_sub(1) as u16);
    let space_for_children = main_avail.saturating_sub(total_gap);

    // Pass 1: resolve the main extent of every non-flex child and tally the
    // flex weights.
    let mut main_sizes: Vec<u16> = Vec::with_capacity(items.len());
    let mut flex_weight_total: u32 = 0;
    let mut consumed: u16 = 0;
    for item in items {
        let size = match item.dimension {
            Dimension::Auto => axis.main(item.intrinsic).min(space_for_children),
            Dimension::Fixed(n) => n.min(space_for_children),
            Dimension::Percent(p) => {
                let p = p.min(100) as u32;
                ((space_for_children as u32 * p) / 100) as u16
            }
            Dimension::Flex(weight) => {
                flex_weight_total += weight.max(1) as u32;
                0 // resolved in pass 2
            }
        };
        main_sizes.push(size);
        consumed = consumed.saturating_add(size);
    }

    // Pass 2: distribute leftover main space to flex children by weight,
    // handing the rounding remainder to the last flex child so the row fills
    // exactly.
    let leftover = space_for_children.saturating_sub(consumed);
    if flex_weight_total > 0 {
        let mut distributed: u16 = 0;
        let flex_indices: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| matches!(it.dimension, Dimension::Flex(_)))
            .map(|(i, _)| i)
            .collect();
        for (nth, &i) in flex_indices.iter().enumerate() {
            let weight = match items[i].dimension {
                Dimension::Flex(w) => w.max(1) as u32,
                _ => unreachable!(),
            };
            let size = if nth + 1 == flex_indices.len() {
                leftover.saturating_sub(distributed)
            } else {
                (leftover as u32 * weight)
                    .checked_div(flex_weight_total)
                    .unwrap_or(0) as u16
            };
            distributed = distributed.saturating_add(size);
            main_sizes[i] = size;
        }
    }

    // Main-axis start offset from `justify` (only meaningful when there is
    // leftover space and no flex child soaked it up).
    let used_main: u16 = main_sizes.iter().copied().fold(0, u16::saturating_add);
    let free = space_for_children.saturating_sub(used_main);
    let (mut cursor, between_extra) = match style.justify {
        Justify::Start => (0, 0),
        Justify::Center => (free / 2, 0),
        Justify::End => (free, 0),
        Justify::SpaceBetween if items.len() > 1 => (0, free / (items.len() as u16 - 1)),
        Justify::SpaceBetween => (0, 0),
    };

    // Place each child, aligning it on the cross axis.
    let mut rects = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let main_len = main_sizes[i];
        let cross_len = match style.align_items {
            Align::Stretch => cross_avail,
            _ => axis.cross(item.intrinsic).min(cross_avail),
        };
        let cross_off = match style.align_items {
            Align::Start | Align::Stretch => 0,
            Align::Center => cross_avail.saturating_sub(cross_len) / 2,
            Align::End => cross_avail.saturating_sub(cross_len),
        };
        // Clamp placement to the inner box. Normally children fit exactly, so
        // this is a no-op; it only matters in degenerate cases (e.g. a gap
        // wider than the container) where the running cursor would otherwise
        // push a child past the edge and return an out-of-bounds rect.
        let main_start = cursor.min(main_avail);
        let main_len = main_len.min(main_avail.saturating_sub(main_start));
        rects.push(axis.place(inner, main_start, cross_off, main_len, cross_len));
        cursor = cursor
            .saturating_add(main_sizes[i])
            .saturating_add(style.gap)
            .saturating_add(between_extra);
    }
    rects
}
