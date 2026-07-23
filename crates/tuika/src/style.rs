//! Styling and theming.
//!
//! `tuika` reuses ratatui's [`Style`], [`Color`], and [`Modifier`] rather than
//! reinventing cell attributes, but components never hard-code colors: they
//! pull from a [`Theme`] passed through the render context. Swapping the theme
//! restyles every component at once, and downstream libraries can ship their
//! own palette without touching component code.

use ratatui::style::{Color, Modifier, Style};

use crate::geometry::Padding;

/// Line-drawing style for bordered components.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BorderStyle {
    /// Rounded corners (`╭╮╰╯`) with light edges.
    #[default]
    Rounded,
    /// Square corners (`┌┐└┘`) with light edges.
    Plain,
    /// Heavy corners and edges (`┏┓┗┛━┃`).
    Thick,
    /// No border; drawn as blank spaces.
    None,
}

impl BorderStyle {
    /// The six line-drawing glyphs (corners, horizontal, vertical) for this style.
    pub fn glyphs(self) -> BorderGlyphs {
        match self {
            BorderStyle::Rounded => BorderGlyphs::new('╭', '╮', '╰', '╯', '─', '│'),
            BorderStyle::Plain => BorderGlyphs::new('┌', '┐', '└', '┘', '─', '│'),
            BorderStyle::Thick => BorderGlyphs::new('┏', '┓', '┗', '┛', '━', '┃'),
            BorderStyle::None => BorderGlyphs::new(' ', ' ', ' ', ' ', ' ', ' '),
        }
    }
}

/// The resolved corner and edge glyphs for a [`BorderStyle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BorderGlyphs {
    /// Top-left corner glyph.
    pub top_left: char,
    /// Top-right corner glyph.
    pub top_right: char,
    /// Bottom-left corner glyph.
    pub bottom_left: char,
    /// Bottom-right corner glyph.
    pub bottom_right: char,
    /// Horizontal edge glyph (top and bottom sides).
    pub horizontal: char,
    /// Vertical edge glyph (left and right sides).
    pub vertical: char,
}

impl BorderGlyphs {
    const fn new(
        top_left: char,
        top_right: char,
        bottom_left: char,
        bottom_right: char,
        horizontal: char,
        vertical: char,
    ) -> Self {
        Self {
            top_left,
            top_right,
            bottom_left,
            bottom_right,
            horizontal,
            vertical,
        }
    }
}

/// A named palette every component styles itself from.
///
/// The default mirrors yolop's existing inline palette so the full-screen
/// renderer looks like the same product. A different host can construct its
/// own `Theme` and the whole component tree follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Base fill behind the whole screen.
    pub background: Color,
    /// Raised fill for panels and overlays that sit above the background.
    pub surface: Color,
    /// Primary foreground for body text.
    pub text: Color,
    /// De-emphasized foreground for secondary text, hints, and scrollbar thumbs.
    pub muted: Color,
    /// Faintest foreground, dimmer than `muted`, for inactive tracks and rules.
    pub dim: Color,
    /// Primary highlight for active/emphasized elements (progress fill, spinner).
    pub accent: Color,
    /// Secondary accent for complementary highlights distinct from `accent`.
    pub accent_alt: Color,
    /// Border color for unfocused bordered components.
    pub border: Color,
    /// Border color for the focused component.
    pub border_focused: Color,
    /// Background of the selected item in lists and menus.
    pub selection_bg: Color,
    /// Foreground of the selected item in lists and menus.
    pub selection_fg: Color,
    /// Markdown and code-block role colors, consumed by [`Markdown`] and
    /// [`CodeBlock`] (and any [`Highlighter`] the host plugs in).
    ///
    /// [`Markdown`]: crate::components::Markdown
    /// [`CodeBlock`]: crate::components::CodeBlock
    /// [`Highlighter`]: crate::highlight::Highlighter
    pub code: CodeTheme,
}

/// The palette [`Markdown`](crate::components::Markdown) and
/// [`CodeBlock`](crate::components::CodeBlock) style themselves from.
///
/// It carries two related things: a few markdown-prose roles (`heading`,
/// `link`, the inline/`block` code background) and the syntax roles a
/// [`Highlighter`](crate::highlight::Highlighter) maps token classes onto so
/// highlighted code follows the host theme instead of a hard-coded palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeTheme {
    /// Heading text (`# …`).
    pub heading: Color,
    /// Link text and bare URLs.
    pub link: Color,
    /// Background behind inline code and fenced blocks.
    pub background: Color,
    /// Default code text: unclassified tokens and the plain fallback.
    pub text: Color,
    /// Language-tag label above a fenced block.
    pub label: Color,
    // Syntax roles a highlighter maps token classes onto.
    /// Keywords and language reserved words.
    pub keyword: Color,
    /// Function and method names.
    pub function: Color,
    /// Type names, traits, and other type-level identifiers.
    pub type_name: Color,
    /// Constants, literals, and enum-like values.
    pub constant: Color,
    /// String and character literals.
    pub string: Color,
    /// Comments.
    pub comment: Color,
    /// Punctuation, operators, and delimiters.
    pub punctuation: Color,
}

impl Default for Theme {
    fn default() -> Self {
        // tuika's own toolkit identity — a warm red-on-dark palette. This is a
        // neutral default for any app built on tuika; it is deliberately NOT any
        // one host's brand. A host with its own look builds its own `Theme` (see
        // yolop's `fullscreen::yolop_theme`) rather than relying on this.
        Theme {
            background: Color::Rgb(20, 18, 20),
            surface: Color::Rgb(34, 28, 30),
            text: Color::Rgb(235, 230, 230),
            muted: Color::Rgb(150, 140, 142),
            dim: Color::Rgb(90, 74, 78),
            accent: Color::Rgb(200, 60, 70),
            accent_alt: Color::Rgb(230, 140, 90),
            border: Color::Rgb(90, 74, 78),
            border_focused: Color::Rgb(200, 60, 70),
            selection_bg: Color::Rgb(120, 30, 40),
            selection_fg: Color::Rgb(240, 235, 235),
            code: CodeTheme::default(),
        }
    }
}

impl Default for CodeTheme {
    fn default() -> Self {
        // A warm, low-contrast syntax palette that sits alongside tuika's
        // default red-on-dark identity. Hosts with their own look build their
        // own `CodeTheme` (see yolop's `yolop_theme`).
        CodeTheme {
            heading: Color::Rgb(235, 230, 230),
            link: Color::Rgb(120, 160, 210),
            background: Color::Rgb(28, 24, 26),
            text: Color::Rgb(210, 205, 205),
            label: Color::Rgb(150, 140, 142),
            keyword: Color::Rgb(210, 120, 90),
            function: Color::Rgb(120, 160, 210),
            type_name: Color::Rgb(126, 170, 176),
            constant: Color::Rgb(200, 160, 120),
            string: Color::Rgb(150, 180, 140),
            comment: Color::Rgb(120, 110, 112),
            punctuation: Color::Rgb(150, 140, 142),
        }
    }
}

impl Theme {
    /// Style for primary body text (`text` foreground).
    pub fn text_style(&self) -> Style {
        Style::default().fg(self.text)
    }

    /// Style for de-emphasized text (`muted` foreground).
    pub fn muted_style(&self) -> Style {
        Style::default().fg(self.muted)
    }

    /// Bold style in the primary `accent` color.
    pub fn accent_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// Border color for a component, `border_focused` when `focused` else `border`.
    pub fn border_color(&self, focused: bool) -> Color {
        if focused {
            self.border_focused
        } else {
            self.border
        }
    }

    /// Bold style for a selected item (`selection_fg` on `selection_bg`).
    pub fn selection_style(&self) -> Style {
        Style::default()
            .bg(self.selection_bg)
            .fg(self.selection_fg)
            .add_modifier(Modifier::BOLD)
    }
}

/// A resolved, partial style for one semantic [`Role`].
///
/// This is the "declaration block" of tuika's stylesheet model: the `Theme` is
/// the token layer (named *colors*), and a `StyleBundle` is what a role maps
/// those tokens onto — a foreground/background plus text modifiers, and, for
/// container roles, a border glyph style and padding. Every field is optional so
/// a bundle can *contribute* (add a modifier, override just the foreground)
/// rather than replace a whole [`Style`]: [`apply`](Self::apply) overlays only
/// the fields it sets onto a base style, and adds its modifiers.
///
/// Only paint-time attributes belong here. A component's [`measure`] runs
/// without a [`RenderCtx`], so layout-affecting choices (whether a border
/// exists, how much padding) stay instance-level; the stylesheet owns color,
/// background, modifiers, and — where they do not change a cell's footprint —
/// border glyphs.
///
/// [`measure`]: crate::View::measure
/// [`RenderCtx`]: crate::RenderCtx
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StyleBundle {
    /// Foreground color, when the role sets one.
    pub fg: Option<Color>,
    /// Background color, when the role sets one.
    pub bg: Option<Color>,
    /// Text modifiers the role adds (bold, italic, underline, …).
    pub add_modifier: Modifier,
    /// Border glyph style for container roles (see the note above: this must not
    /// change a component's footprint, so it selects the glyph set, not whether
    /// a border exists).
    pub border: Option<BorderStyle>,
    /// Padding for container roles, when the role sets one.
    pub padding: Option<Padding>,
}

impl StyleBundle {
    /// An empty bundle that overrides nothing and adds no modifiers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the foreground color.
    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    /// Set the background color.
    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    /// Add text modifiers (accumulates with any already set).
    pub fn modifier(mut self, m: Modifier) -> Self {
        self.add_modifier |= m;
        self
    }

    /// Add the bold modifier.
    pub fn bold(self) -> Self {
        self.modifier(Modifier::BOLD)
    }

    /// Add the italic modifier.
    pub fn italic(self) -> Self {
        self.modifier(Modifier::ITALIC)
    }

    /// Add the underline modifier.
    pub fn underlined(self) -> Self {
        self.modifier(Modifier::UNDERLINED)
    }

    /// Add the crossed-out (strikethrough) modifier.
    pub fn crossed_out(self) -> Self {
        self.modifier(Modifier::CROSSED_OUT)
    }

    /// Set the border glyph style (container roles).
    pub fn border(mut self, border: BorderStyle) -> Self {
        self.border = Some(border);
        self
    }

    /// Set the padding (container roles).
    pub fn padding(mut self, padding: Padding) -> Self {
        self.padding = Some(padding);
        self
    }

    /// Overlay this bundle onto `base`: `fg`/`bg` replace the base's when set,
    /// and this bundle's modifiers are added. Unset fields leave `base` as-is, so
    /// a modifier-only bundle (e.g. emphasis) keeps the inherited color.
    pub fn apply(&self, base: Style) -> Style {
        let mut style = base;
        if let Some(fg) = self.fg {
            style = style.fg(fg);
        }
        if let Some(bg) = self.bg {
            style = style.bg(bg);
        }
        style.add_modifier(self.add_modifier)
    }

    /// Resolve this bundle to a standalone [`Style`] over the default.
    pub fn to_style(&self) -> Style {
        self.apply(Style::default())
    }
}

/// A semantic element a component styles itself as. The stylesheet maps each
/// role to a [`StyleBundle`]; this is the "selector" half of the model, kept a
/// closed enum (rather than open string classes) so a typo is a compile error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A bordered panel / framed container ([`Boxed`](crate::Boxed)).
    Panel,
    /// A markdown heading (`# …`).
    Heading,
    /// A hyperlink or bare URL.
    Link,
    /// Inline `code` spans.
    InlineCode,
    /// Emphasized (`*italic*`) text.
    Emphasis,
    /// Strong (`**bold**`) text.
    Strong,
    /// Struck-through (`~~text~~`) text.
    Strikethrough,
    /// A list item's bullet or number marker.
    ListMarker,
    /// A task-list checkbox marker.
    TaskMarker,
    /// A thematic break / horizontal rule.
    Rule,
    /// The glyph marking an inline image placeholder.
    ImageMarker,
}

/// The rule layer of tuika's styling model: a mapping from every [`Role`] to the
/// [`StyleBundle`] a component resolves for it.
///
/// Where [`Theme`] centralizes the *colors* a whole component tree draws from,
/// `StyleSheet` centralizes the *choice of style per role* — so a host can, in
/// one place, make links green-and-bold, panels thick-bordered, or every panel
/// share a surface fill, without touching component code or the raw colors used
/// elsewhere. Like [`Theme`] it is a flat `Copy` struct of named slots, built
/// once (usually with [`from_theme`](Self::from_theme)) and threaded through
/// [`RenderCtx`](crate::RenderCtx).
///
/// Override a rule with struct-update syntax:
///
/// ```
/// use tuika::{StyleBundle, StyleSheet, Theme};
/// use ratatui::style::Color;
///
/// let theme = Theme::default();
/// let sheet = StyleSheet {
///     link: StyleBundle::new().fg(Color::Green).bold().underlined(),
///     ..StyleSheet::from_theme(&theme)
/// };
/// assert_eq!(sheet.link.fg, Some(Color::Green));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyleSheet {
    /// Border color/background for a [`Boxed`](crate::Boxed) panel.
    pub panel: StyleBundle,
    /// Markdown heading text.
    pub heading: StyleBundle,
    /// Links and bare URLs.
    pub link: StyleBundle,
    /// Inline code spans.
    pub inline_code: StyleBundle,
    /// Emphasized text (modifier-only, keeps the inherited color).
    pub emphasis: StyleBundle,
    /// Strong text (modifier-only).
    pub strong: StyleBundle,
    /// Struck-through text (modifier-only).
    pub strikethrough: StyleBundle,
    /// List bullet / number markers.
    pub list_marker: StyleBundle,
    /// Task-list checkbox markers.
    pub task_marker: StyleBundle,
    /// Thematic-break rules.
    pub rule: StyleBundle,
    /// Inline-image placeholder marker glyph.
    pub image_marker: StyleBundle,
}

impl StyleSheet {
    /// The default stylesheet for `theme`: every role mapped to the same style
    /// tuika's components used before the stylesheet existed, so swapping in
    /// `from_theme(theme)` is a no-op visually. Start from this and override the
    /// roles you want to restyle.
    pub fn from_theme(theme: &Theme) -> Self {
        let code = &theme.code;
        StyleSheet {
            // Panel background/border color default to unset so a plain `Boxed`
            // keeps its no-fill, theme-bordered look; a host opts panels into a
            // shared fill by setting `panel.bg`.
            panel: StyleBundle::new(),
            heading: StyleBundle::new().fg(code.heading).bold(),
            link: StyleBundle::new().fg(code.link).underlined(),
            inline_code: StyleBundle::new().fg(code.text).bg(code.background),
            emphasis: StyleBundle::new().italic(),
            strong: StyleBundle::new().bold(),
            strikethrough: StyleBundle::new().crossed_out(),
            list_marker: StyleBundle::new().fg(theme.accent_alt),
            task_marker: StyleBundle::new().fg(theme.accent),
            rule: StyleBundle::new().fg(theme.dim),
            image_marker: StyleBundle::new().fg(theme.accent),
        }
    }

    /// The [`StyleBundle`] mapped to `role`.
    pub fn resolve(&self, role: Role) -> StyleBundle {
        match role {
            Role::Panel => self.panel,
            Role::Heading => self.heading,
            Role::Link => self.link,
            Role::InlineCode => self.inline_code,
            Role::Emphasis => self.emphasis,
            Role::Strong => self.strong,
            Role::Strikethrough => self.strikethrough,
            Role::ListMarker => self.list_marker,
            Role::TaskMarker => self.task_marker,
            Role::Rule => self.rule,
            Role::ImageMarker => self.image_marker,
        }
    }
}

impl Default for StyleSheet {
    /// The stylesheet for the default [`Theme`].
    fn default() -> Self {
        Self::from_theme(&Theme::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::rainbow_theme;
    use ratatui::style::Modifier;

    #[test]
    fn theme_helper_styles_map_to_slots() {
        let t = rainbow_theme();
        assert_eq!(t.text_style().fg, Some(t.text));
        assert_eq!(t.muted_style().fg, Some(t.muted));
        assert_eq!(t.accent_style().fg, Some(t.accent));
        assert!(t.accent_style().add_modifier.contains(Modifier::BOLD));
        assert_eq!(t.border_color(false), t.border);
        assert_eq!(t.border_color(true), t.border_focused);
        let sel = t.selection_style();
        assert_eq!(sel.bg, Some(t.selection_bg));
        assert_eq!(sel.fg, Some(t.selection_fg));
        assert!(sel.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn default_theme_is_the_toolkit_identity_not_a_host_brand() {
        use ratatui::style::Color;
        // tuika's own look is warm red-on-dark. It is deliberately a neutral toolkit
        // identity, not any host's brand — a host with its own palette builds its own
        // `Theme` (e.g. yolop's `fullscreen::yolop_theme`) instead of inheriting this.
        let t = Theme::default();
        assert_eq!(t.accent, Color::Rgb(200, 60, 70));
        assert_eq!(t.selection_bg, Color::Rgb(120, 30, 40));
        // Not yolop's accent blue.
        assert_ne!(t.accent, Color::Rgb(45, 91, 158));
    }

    #[test]
    fn bundle_apply_overrides_color_but_only_adds_modifiers() {
        use ratatui::style::Color;
        // A modifier-only bundle keeps the base's color and adds its modifier.
        let base = Style::default().fg(Color::Red);
        let emphasized = StyleBundle::new().italic().apply(base);
        assert_eq!(emphasized.fg, Some(Color::Red), "color inherited");
        assert!(emphasized.add_modifier.contains(Modifier::ITALIC));

        // A bundle that sets fg replaces the base's fg.
        let recolored = StyleBundle::new().fg(Color::Green).bold().apply(base);
        assert_eq!(recolored.fg, Some(Color::Green));
        assert!(recolored.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn from_theme_reproduces_the_pre_stylesheet_styles() {
        let t = rainbow_theme();
        let s = StyleSheet::from_theme(&t);
        // Heading = code.heading + bold; link = code.link + underline; these are
        // exactly what the components hard-coded before the stylesheet existed.
        assert_eq!(s.heading.fg, Some(t.code.heading));
        assert!(s.heading.add_modifier.contains(Modifier::BOLD));
        assert_eq!(s.link.fg, Some(t.code.link));
        assert!(s.link.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(s.inline_code.fg, Some(t.code.text));
        assert_eq!(s.inline_code.bg, Some(t.code.background));
        // Emphasis is modifier-only so it keeps the surrounding prose color.
        assert_eq!(s.emphasis.fg, None);
        assert!(s.emphasis.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn resolve_matches_named_fields() {
        let s = StyleSheet::from_theme(&rainbow_theme());
        assert_eq!(s.resolve(Role::Heading), s.heading);
        assert_eq!(s.resolve(Role::Link), s.link);
        assert_eq!(s.resolve(Role::Panel), s.panel);
    }

    #[test]
    fn overriding_one_role_leaves_the_rest_at_theme_defaults() {
        use ratatui::style::Color;
        let t = rainbow_theme();
        let sheet = StyleSheet {
            link: StyleBundle::new().fg(Color::Green).bold(),
            ..StyleSheet::from_theme(&t)
        };
        assert_eq!(sheet.link.fg, Some(Color::Green));
        assert!(sheet.link.add_modifier.contains(Modifier::BOLD));
        // Everything else still tracks the theme.
        assert_eq!(sheet.heading.fg, Some(t.code.heading));
    }
}
