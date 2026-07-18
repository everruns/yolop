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
//!   Attrs: `title = e`, `border = e`, `padding = e`, `background = e`.
//! - `text( expr )` — a `Text::raw` line. `spacer()` — a `Spacer`.
//! - `grow(n) { node }` / `fixed(n) { node }` — set a child's main-axis size
//!   (default is auto).
//! - `node( expr )` — **escape hatch**: splice any `impl View`, including a
//!   component defined in another crate. This is how third-party components
//!   plug into the DSL today.
//!
//! Cross-crate note: the macro emits `$crate::…` paths, so it works
//! anywhere in this binary without imports. When `tuika` graduates to its own
//! crate, this becomes a `#[macro_export]` in that crate and a proc-macro can
//! add first-class `MyWidget { … }` syntax for external components.

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
    (@boxattrs $b:expr; border = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@boxattrs $b.border($e); $($($rest)*)?)
    };
    (@boxattrs $b:expr; padding = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@boxattrs $b.padding($e); $($($rest)*)?)
    };
    (@boxattrs $b:expr; background = $e:expr $(, $($rest:tt)*)?) => {
        $crate::view!(@boxattrs $b.background($e); $($($rest)*)?)
    };
}
