//! Property-based tests for the layout solver and overlay resolver.
//!
//! Example-based tests pin down specific cases; these assert *invariants* that
//! must hold for every input, which is how the flexbox solver's arithmetic
//! edge cases get shaken out (the out-of-bounds-placement bug fixed alongside
//! the resize suite is exactly the kind proptest finds automatically).

use proptest::prelude::*;
use ratatui_core::layout::Rect;

use super::geometry::{Axis, Padding, Size};
use super::layout::{Align, Dimension, Direction, Item, Justify, LayoutStyle, solve};
use super::overlay::{Anchor, Extent, OverlaySpec};

fn dimension_strategy() -> impl Strategy<Value = Dimension> {
    prop_oneof![
        Just(Dimension::Auto),
        (0u16..60).prop_map(Dimension::Fixed),
        (0u16..=100).prop_map(Dimension::Percent),
        (1u16..6).prop_map(Dimension::Flex),
    ]
}

fn item_strategy() -> impl Strategy<Value = Item> {
    (dimension_strategy(), 0u16..60, 0u16..30).prop_map(|(d, w, h)| Item::new(d, Size::new(w, h)))
}

fn align_strategy() -> impl Strategy<Value = Align> {
    prop_oneof![
        Just(Align::Start),
        Just(Align::Center),
        Just(Align::End),
        Just(Align::Stretch),
    ]
}

fn justify_strategy() -> impl Strategy<Value = Justify> {
    prop_oneof![
        Just(Justify::Start),
        Just(Justify::Center),
        Just(Justify::End),
        Just(Justify::SpaceBetween),
    ]
}

fn direction_strategy() -> impl Strategy<Value = Direction> {
    prop_oneof![Just(Direction::Row), Just(Direction::Column)]
}

proptest! {
    /// For ANY area, item set, gap, padding, alignment and justification: every
    /// child rect stays inside the area, the rect count matches, and children
    /// are placed in non-decreasing main-axis order (never overlapping backward).
    #[test]
    fn solved_children_stay_within_area(
        w in 0u16..300,
        h in 0u16..120,
        items in proptest::collection::vec(item_strategy(), 1..8),
        gap in 0u16..30,
        pad in 0u16..6,
        dir in direction_strategy(),
        align in align_strategy(),
        justify in justify_strategy(),
    ) {
        let area = Rect::new(0, 0, w, h);
        let style = LayoutStyle {
            direction: dir,
            padding: Padding::all(pad),
            gap,
            align_items: align,
            justify,
        };
        let rects = solve(area, &style, &items);
        prop_assert_eq!(rects.len(), items.len());

        let axis = dir.axis();
        let mut last_main = 0u16;
        for r in &rects {
            prop_assert!(
                r.x >= area.x && r.right() <= area.right(),
                "x out of bounds: {:?} in {:?}",
                r,
                area
            );
            prop_assert!(
                r.y >= area.y && r.bottom() <= area.bottom(),
                "y out of bounds: {:?} in {:?}",
                r,
                area
            );
            let main = match axis {
                Axis::Horizontal => r.x,
                Axis::Vertical => r.y,
            };
            prop_assert!(main >= last_main, "children placed out of order: {:?}", rects);
            last_main = main;
        }
    }

    /// N equal-weight flex children with no gap or padding fill the main axis
    /// exactly — no cell lost to rounding, none double-counted.
    #[test]
    fn flex_children_fill_main_axis_exactly(w in 0u16..500, n in 1usize..8) {
        let items: Vec<Item> = (0..n)
            .map(|_| Item::new(Dimension::Flex(1), Size::ZERO))
            .collect();
        let rects = solve(Rect::new(0, 0, w, 1), &LayoutStyle::row(), &items);
        let total: u16 = rects.iter().map(|r| r.width).fold(0, u16::saturating_add);
        prop_assert_eq!(total, w, "flex children must fill width exactly");
    }

    /// An overlay always lands inside its screen and never exceeds its max
    /// clamp — for any anchor, size request, and margin (including a margin
    /// larger than the screen).
    #[test]
    fn overlay_stays_within_screen(
        w in 0u16..300,
        h in 0u16..120,
        pct_w in 0u16..=100,
        pct_h in 0u16..=100,
        margin in 0u16..12,
        anchor_i in 0usize..9,
        max_w in 1u16..300,
        max_h in 1u16..120,
    ) {
        let anchors = [
            Anchor::Center,
            Anchor::Top,
            Anchor::Bottom,
            Anchor::Left,
            Anchor::Right,
            Anchor::TopLeft,
            Anchor::TopRight,
            Anchor::BottomLeft,
            Anchor::BottomRight,
        ];
        let screen = Rect::new(0, 0, w, h);
        let spec = OverlaySpec {
            anchor: anchors[anchor_i],
            width: Extent::Percent(pct_w),
            height: Extent::Percent(pct_h),
            min_width: 0,
            min_height: 0,
            max_width: max_w,
            max_height: max_h,
            margin,
        };
        let r = spec.resolve(screen);
        prop_assert!(
            r.x >= screen.x && r.right() <= screen.right(),
            "overlay x out of bounds: {:?} in {:?}",
            r,
            screen
        );
        prop_assert!(
            r.y >= screen.y && r.bottom() <= screen.bottom(),
            "overlay y out of bounds: {:?} in {:?}",
            r,
            screen
        );
        prop_assert!(r.width <= max_w, "width exceeds max");
        prop_assert!(r.height <= max_h, "height exceeds max");
    }
}
