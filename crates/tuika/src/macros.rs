//! `view!` — a declarative macro layer over the `tuika` builder API.
//!
//! `view!` is pure sugar: every rule expands to the same `Flex`/`Boxed`/
//! `element(...)` calls you would write by hand, so there is no runtime cost,
//! no reconciler, and nothing new to learn about the model. It exists to make
//! nested layouts read top-down instead of inside-out.
//!
//! ```ignore
//! let root = view! {
//!     col(gap = 1, padding = tuika::Padding::all(1)) {
//!         boxed(title = " body ") { text("hello") }
//!         grow(1) { spacer() }
//!         node(my_status_bar)      // any expression that is `impl View`
//!     }
//! };
//! ```
//!
//! Grammar (each keyword consumes exactly one node):
//! - `col( attrs ) { children }` / `row( attrs ) { children }` — flex containers.
//!   Attrs: `gap = e`, `padding = e`, `align = e`, `justify = e` (comma-separated,
//!   all optional; the whole `( … )` may be omitted).
//! - `boxed( attrs ) { child }` — bordered container of one child.
//!   Attrs: `title = e`, `title_bottom = e`, `border = e`, `border_color = e`,
//!   `padding = e`, `background = e`.
//! - `text( expr )` — a `Text::raw` line. `spacer()` — a `Spacer`.
//! - `grow(n) { node }` / `fixed(n) { node }` — set a child's main-axis size
//!   (default is auto).
//! - `node( expr )` — **escape hatch**: splice any `impl View`, including a
//!   component defined in another crate. This is how third-party components
//!   plug into the DSL today.
//!
//! Cross-crate note: the macro emits `$crate::…` paths and is
//! `#[macro_export]`ed, so downstream crates use it as `tuika::view!` without
//! importing the referenced types. A future proc-macro could add first-class
//! `MyWidget { … }` syntax for external components; the `node(expr)` escape
//! hatch covers that case today.

/// Build a tuika [`Element`](crate::view::Element) tree from a declarative,
/// top-down layout description.
///
/// Each keyword consumes exactly one node: `col`/`row` open flex containers
/// (with optional `gap`/`padding`/`align`/`justify`/`background` attrs and a
/// `{ children }` block), `boxed` wraps a single child in a border (with
/// `title`/`title_bottom`/`border`/`border_color`/`padding`/`background` attrs), `text(expr)` and `spacer()`
/// emit leaves, `grow(n)`/`fixed(n)` set a child's main-axis size, and
/// `node(expr)` splices any `impl View`. Expands to plain builder calls with no
/// runtime cost.
#[macro_export]
macro_rules! view {
    // ---- public entry: a single node -> Element -----------------------------
    (col $($rest:tt)*) => { $crate::view!(@one col $($rest)*) };
    (row $($rest:tt)*) => { $crate::view!(@one row $($rest)*) };
    (boxed $($rest:tt)*) => { $crate::view!(@one boxed $($rest)*) };
    (text $($rest:tt)*) => { $crate::view!(@one text $($rest)*) };
    (spacer $($rest:tt)*) => { $crate::view!(@one spacer $($rest)*) };
    (node $($rest:tt)*) => { $crate::view!(@one node $($rest)*) };

    // ---- @one: parse one node into an Element -------------------------------
    (@one col $(( $($attr:tt)* ))? { $($kids:tt)* }) => {{
        let __flex = $crate::Flex::column();
        let __flex = $crate::view!(@attrs __flex; $($($attr)*)?);
        let __flex = $crate::view!(@kids __flex; $($kids)*);
        $crate::element(__flex)
    }};
    (@one row $(( $($attr:tt)* ))? { $($kids:tt)* }) => {{
        let __flex = $crate::Flex::row();
        let __flex = $crate::view!(@attrs __flex; $($($attr)*)?);
        let __flex = $crate::view!(@kids __flex; $($kids)*);
        $crate::element(__flex)
    }};
    (@one boxed $(( $($attr:tt)* ))? { $($kids:tt)* }) => {{
        let __boxed = $crate::Boxed::new($crate::view!(@one $($kids)*));
        let __boxed = $crate::view!(@boxattrs __boxed; $($($attr)*)?);
        $crate::element(__boxed)
    }};
    (@one text ( $e:expr )) => {
        $crate::element($crate::Text::raw($e))
    };
    (@one spacer ()) => { $crate::element($crate::Spacer) };
    (@one node ( $e:expr )) => { $crate::element($e) };

    // ---- @kids: fold children onto a Flex builder ---------------------------
    (@kids $b:expr; ) => { $b };
    (@kids $b:expr; grow ( $n:expr ) { $($node:tt)* } $($rest:tt)*) => {
        $crate::view!(@kids $b.grow($n, $crate::view!(@one $($node)*)); $($rest)*)
    };
    (@kids $b:expr; fixed ( $n:expr ) { $($node:tt)* } $($rest:tt)*) => {
        $crate::view!(@kids $b.fixed($n, $crate::view!(@one $($node)*)); $($rest)*)
    };
    (@kids $b:expr; col $(( $($a:tt)* ))? { $($k:tt)* } $($rest:tt)*) => {
        $crate::view!(@kids $b.auto($crate::view!(@one col $(($($a)*))? { $($k)* })); $($rest)*)
    };
    (@kids $b:expr; row $(( $($a:tt)* ))? { $($k:tt)* } $($rest:tt)*) => {
        $crate::view!(@kids $b.auto($crate::view!(@one row $(($($a)*))? { $($k)* })); $($rest)*)
    };
    (@kids $b:expr; boxed $(( $($a:tt)* ))? { $($k:tt)* } $($rest:tt)*) => {
        $crate::view!(@kids $b.auto($crate::view!(@one boxed $(($($a)*))? { $($k)* })); $($rest)*)
    };
    (@kids $b:expr; text ( $e:expr ) $($rest:tt)*) => {
        $crate::view!(@kids $b.auto($crate::element($crate::Text::raw($e))); $($rest)*)
    };
    (@kids $b:expr; spacer () $($rest:tt)*) => {
        $crate::view!(@kids $b.auto($crate::element($crate::Spacer)); $($rest)*)
    };
    (@kids $b:expr; node ( $e:expr ) $($rest:tt)*) => {
        $crate::view!(@kids $b.auto($crate::element($e)); $($rest)*)
    };

    // ---- @attrs: fold flex layout attributes --------------------------------
    (@attrs $b:expr; ) => { $b };
    (@attrs $b:expr; gap = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@attrs $b.gap($e); $($($rest)*)?)
    };
    (@attrs $b:expr; padding = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@attrs $b.padding($e); $($($rest)*)?)
    };
    (@attrs $b:expr; align = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@attrs $b.align($e); $($($rest)*)?)
    };
    (@attrs $b:expr; justify = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@attrs $b.justify($e); $($($rest)*)?)
    };
    (@attrs $b:expr; background = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@attrs $b.background($e); $($($rest)*)?)
    };

    // ---- @boxattrs: fold boxed attributes -----------------------------------
    (@boxattrs $b:expr; ) => { $b };
    (@boxattrs $b:expr; title = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@boxattrs $b.title($e); $($($rest)*)?)
    };
    (@boxattrs $b:expr; title_bottom = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@boxattrs $b.title_bottom($e); $($($rest)*)?)
    };
    (@boxattrs $b:expr; border = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@boxattrs $b.border($e); $($($rest)*)?)
    };
    (@boxattrs $b:expr; border_color = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@boxattrs $b.border_color($e); $($($rest)*)?)
    };
    (@boxattrs $b:expr; padding = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@boxattrs $b.padding($e); $($($rest)*)?)
    };
    (@boxattrs $b:expr; background = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@boxattrs $b.background($e); $($($rest)*)?)
    };
}

#[cfg(test)]
mod tests {
    use crate::components::{Boxed, Flex, Spacer, Text};
    use crate::test_support::render_el;
    use crate::view::{Element, RenderCtx, View, element};
    use crate::{Size, Surface};
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn view_macro_matches_builder() {
        // Hand-written builder tree.
        let built: Element = element(
            Flex::column()
                .gap(1)
                .auto(element(Boxed::new(element(Text::raw("hi"))).title(" t ")))
                .grow(1, element(Spacer)),
        );

        // The same tree via the declarative macro.
        let macroed: Element = crate::view! {
            col(gap = 1) {
                boxed(title = " t ") { text("hi") }
                grow(1) { spacer() }
            }
        };

        let a = render_el(&built, 20, 5);
        let b = render_el(&macroed, 20, 5);
        assert_eq!(a, b, "view! must render identically to the builder form");
        assert!(b.iter().any(|l| l.contains('t')), "{b:?}");
    }

    #[test]
    fn view_macro_forwards_title_bottom() {
        // `title_bottom =` folds to `Boxed::title_bottom` and renders on the
        // bottom border, flush-right by default.
        let built: Element = element(
            Boxed::new(element(Text::raw("hi")))
                .title(" top ")
                .title_bottom(" 1/3 "),
        );
        let macroed: Element = crate::view! {
            boxed(title = " top ", title_bottom = " 1/3 ") { text("hi") }
        };
        let a = render_el(&built, 12, 3);
        let b = render_el(&macroed, 12, 3);
        assert_eq!(a, b, "title_bottom must render identically to the builder");
        assert!(
            b.last().is_some_and(|l| l.contains("1/3")),
            "bottom border carries the secondary title: {b:?}"
        );
    }

    #[test]
    fn view_macro_forwards_border_color() {
        use ratatui::style::Color;
        // `border_color =` folds to `Boxed::border_color`, rendering identically
        // to the builder form.
        let built: Element =
            element(Boxed::new(element(Text::raw("hi"))).border_color(Color::Indexed(201)));
        let macroed: Element = crate::view! {
            boxed(border_color = Color::Indexed(201)) { text("hi") }
        };
        assert_eq!(
            render_el(&built, 10, 3),
            render_el(&macroed, 10, 3),
            "border_color must render identically to the builder"
        );
    }

    /// A `View` standing in for a component defined in some other crate.
    struct Star;
    impl View for Star {
        fn measure(&self, _available: Size) -> Size {
            Size::new(1, 1)
        }
        fn render(&self, area: Rect, surface: &mut Surface, _ctx: &RenderCtx) {
            surface.set(area.x, area.y, '★', Style::default());
        }
    }

    #[test]
    fn view_macro_accepts_foreign_view_via_node() {
        // `node(expr)` splices any `impl View` — this is how a component from
        // another crate participates in the DSL.
        let tree: Element = crate::view! {
            col {
                node(Star)
            }
        };
        let out = render_el(&tree, 5, 2);
        assert!(out[0].contains('★'), "foreign view should render: {out:?}");
    }
}
