//! A host-agnostic keymap engine: declarative key bindings resolved to named
//! commands, with layered scoping, mode gating, and multi-stroke sequences.
//!
//! This is `tuika`'s answer to the same problem [OpenTUI's keymap][opentui]
//! solves for the browser/terminal: decouple *what keys do* from the widget
//! that reacts, so an application can declare its shortcuts once, gate them by
//! mode, discover them for help UIs, and dispatch through a single seam. It
//! follows OpenTUI's **register → dispatch → query** shape, adapted to idiomatic
//! Rust and to `tuika`'s own [`Key`] events (so it needs no terminal and stays
//! unit-testable).
//!
//! [opentui]: https://opentui.com/docs/keymap/overview/
//!
//! # Model
//!
//! - A [`Chord`] is one key press plus its modifiers (`ctrl+r`, `enter`, `?`).
//! - A [`KeySequence`] is one or more chords typed in order (`g g`, `ctrl+x s`).
//! - A [`Binding`] maps a sequence to a command value `C` (usually an app enum).
//! - A [`Layer`] groups bindings under a name and priority, optionally *gated*
//!   on runtime data so it is only active in a given mode.
//! - A [`Keymap`] holds the layers plus a small runtime-data store and the
//!   pending multi-stroke state, and turns incoming [`Key`]s into [`Dispatch`].
//!
//! # Example
//!
//! ```
//! use tuika::event::{Key, KeyCode};
//! use tuika::keymap::{Dispatch, Keymap, Layer};
//!
//! #[derive(Clone, Debug, PartialEq)]
//! enum Action { Search, Quit, Top }
//!
//! let mut keymap = Keymap::new().layer(
//!     Layer::new("global")
//!         .bind("ctrl+r", Action::Search)
//!         .bind("ctrl+d", Action::Quit)
//!         // A two-stroke sequence, vim-style.
//!         .bind("g g", Action::Top),
//! );
//!
//! // A single chord resolves immediately.
//! let ctrl_r = Key { code: KeyCode::Char('r'), ctrl: true, alt: false, shift: false };
//! assert_eq!(keymap.dispatch(ctrl_r), Dispatch::Command(Action::Search));
//!
//! // A sequence stays pending until it completes.
//! let g = Key::new(KeyCode::Char('g'));
//! assert_eq!(keymap.dispatch(g), Dispatch::Pending);
//! assert_eq!(keymap.dispatch(g), Dispatch::Command(Action::Top));
//! ```

use std::collections::HashMap;
use std::fmt;

use crate::event::{Key, KeyCode};

/// One key press plus its modifier state, normalized for binding lookups.
///
/// Modifier state is explicit, but with one normalization that mirrors how
/// terminals report input: for a [`KeyCode::Char`] the Shift flag is *dropped*,
/// because the shifted character is already folded into the `char` itself (`?`
/// arrives as `Char('?')`, not `Shift`+`Char('/')`). Shift is kept meaningful
/// for non-character keys, where it is a distinct chord (`Shift`+`Enter`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    /// The key that was pressed.
    pub code: KeyCode,
    /// Ctrl held during the press.
    pub ctrl: bool,
    /// Alt held during the press.
    pub alt: bool,
    /// Shift held during the press (only meaningful for non-character keys).
    pub shift: bool,
}

impl Chord {
    /// A chord for `code` with no modifiers held.
    pub fn new(code: KeyCode) -> Self {
        Self {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// Build a chord from a translated [`Key`] event, applying the character
    /// Shift-normalization described on [`Chord`].
    pub fn from_key(key: Key) -> Self {
        let shift = if matches!(key.code, KeyCode::Char(_)) {
            false
        } else {
            key.shift
        };
        Self {
            code: key.code,
            ctrl: key.ctrl,
            alt: key.alt,
            shift,
        }
    }

    /// Parse a single chord like `"ctrl+r"`, `"alt+shift+tab"`, `"enter"`, or
    /// `"?"`. Modifiers (`ctrl`/`control`, `alt`/`option`, `shift`) are joined to
    /// the key with `+`; the final segment is the key. A trailing `+` is the
    /// literal plus key (`"ctrl++"`).
    pub fn parse(spec: &str) -> Result<Self, KeyParseError> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(KeyParseError::new(spec));
        }
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;

        // Split on '+' while treating a trailing '+' as the literal key. Walk the
        // segments; every segment but the last must be a known modifier.
        let mut key_token: Option<&str> = None;
        let segments = split_plus(spec);
        let last = segments.len().saturating_sub(1);
        for (index, segment) in segments.iter().enumerate() {
            if index == last {
                key_token = Some(segment);
                break;
            }
            match segment.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" | "option" | "opt" | "meta" => alt = true,
                "shift" => shift = true,
                _ => return Err(KeyParseError::new(spec)),
            }
        }

        let key_token = key_token.ok_or_else(|| KeyParseError::new(spec))?;
        let mut code = parse_key_code(key_token).ok_or_else(|| KeyParseError::new(spec))?;
        // A character key folds Shift into the char, matching `from_key`.
        if matches!(code, KeyCode::Char(_)) {
            shift = false;
        }
        // Terminals report Shift+Tab as a distinct `BackTab` key (as does
        // `from_key`), so fold the spec to match what an event will carry.
        if code == KeyCode::Tab && shift {
            code = KeyCode::BackTab;
            shift = false;
        }
        Ok(Self {
            code,
            ctrl,
            alt,
            shift,
        })
    }

    /// A human-readable label such as `"Ctrl+R"`, `"Shift+Tab"`, or `"Space"`,
    /// suitable for a [`KeyHints`](crate::KeyHints) row or a help overlay.
    pub fn display(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl+");
        }
        if self.alt {
            out.push_str("Alt+");
        }
        if self.shift {
            out.push_str("Shift+");
        }
        out.push_str(&key_code_label(self.code));
        out
    }
}

/// Split a chord spec on `+`, but treat a trailing `+` as a literal segment so
/// `"ctrl++"` parses as `["ctrl", "+"]` rather than `["ctrl", "", ""]`.
fn split_plus(spec: &str) -> Vec<&str> {
    if spec == "+" {
        return vec!["+"];
    }
    if let Some(prefix) = spec.strip_suffix('+') {
        // The literal '+' key, possibly with modifiers before it.
        let mut parts: Vec<&str> = prefix.split('+').filter(|s| !s.is_empty()).collect();
        parts.push("+");
        return parts;
    }
    spec.split('+').collect()
}

/// Map a key-name token (already isolated from its modifiers) to a [`KeyCode`].
fn parse_key_code(token: &str) -> Option<KeyCode> {
    // A single character (including a literal "+") is a `Char`, case-sensitive.
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(KeyCode::Char(c));
    }
    Some(match token.to_ascii_lowercase().as_str() {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "space" | "spc" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        _ => return None,
    })
}

/// A display label for a bare key code (no modifiers).
fn key_code_label(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase().to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Shift+Tab".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
    }
}

/// A parse failure for a chord or sequence spec, carrying the offending text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyParseError {
    spec: String,
}

impl KeyParseError {
    fn new(spec: &str) -> Self {
        Self {
            spec: spec.to_string(),
        }
    }
}

impl fmt::Display for KeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid key spec: {:?}", self.spec)
    }
}

impl std::error::Error for KeyParseError {}

/// One or more [`Chord`]s typed in order — a single stroke (`ctrl+r`) or a
/// multi-stroke sequence (`g g`, `ctrl+x s`), written space-separated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySequence(Vec<Chord>);

impl KeySequence {
    /// Parse a whitespace-separated sequence of chords. An empty spec is an
    /// error; a single chord yields a one-element sequence.
    pub fn parse(spec: &str) -> Result<Self, KeyParseError> {
        let chords = spec
            .split_whitespace()
            .map(Chord::parse)
            .collect::<Result<Vec<_>, _>>()?;
        if chords.is_empty() {
            return Err(KeyParseError::new(spec));
        }
        Ok(Self(chords))
    }

    /// The chords making up this sequence.
    pub fn chords(&self) -> &[Chord] {
        &self.0
    }

    /// A human-readable label, chords joined by spaces (`"Ctrl+X S"`).
    pub fn display(&self) -> String {
        self.0
            .iter()
            .map(Chord::display)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A binding from a [`KeySequence`] to a command value, with an optional label
/// used for help/hint surfaces.
#[derive(Clone, Debug)]
pub struct Binding<C> {
    sequence: KeySequence,
    command: C,
    label: Option<String>,
}

/// A named, prioritized group of bindings, optionally gated on runtime data so
/// it is only active in a given application mode.
///
/// Gating mirrors OpenTUI's data-driven layer activation: `when("mode",
/// "search")` makes the layer active only while the keymap's `"mode"` datum
/// equals `"search"`. A layer with no `when` clauses is always active.
#[derive(Clone, Debug)]
pub struct Layer<C> {
    name: String,
    priority: i32,
    when: Vec<(String, String)>,
    bindings: Vec<Binding<C>>,
}

impl<C> Layer<C> {
    /// A new, always-active layer with priority `0`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            priority: 0,
            when: Vec::new(),
            bindings: Vec::new(),
        }
    }

    /// This layer's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Set the layer priority. When two active layers both bind the same
    /// sequence, the higher priority wins.
    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Gate this layer on a runtime-data value: it is active only while the
    /// keymap's datum for `key` equals `value`. Multiple `when` clauses must all
    /// match (logical AND).
    pub fn when(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.when.push((key.into(), value.into()));
        self
    }

    /// Add a binding, parsing `keys` as a [`KeySequence`].
    ///
    /// # Panics
    ///
    /// Panics if `keys` is not a valid sequence spec. Bindings are normally
    /// authored as static literals, so a malformed spec is a programmer error;
    /// use [`Layer::try_bind`] to handle dynamic input fallibly.
    pub fn bind(self, keys: &str, command: C) -> Self {
        self.try_bind(keys, command)
            .unwrap_or_else(|err| panic!("{err}"))
    }

    /// Add a binding with a help label, parsing `keys` as a [`KeySequence`].
    ///
    /// # Panics
    ///
    /// Panics if `keys` is not a valid sequence spec (see [`Layer::bind`]).
    pub fn bind_labeled(self, keys: &str, label: impl Into<String>, command: C) -> Self {
        self.try_bind_labeled(keys, Some(label.into()), command)
            .unwrap_or_else(|err| panic!("{err}"))
    }

    /// Fallible [`Layer::bind`] for dynamically sourced specs.
    pub fn try_bind(self, keys: &str, command: C) -> Result<Self, KeyParseError> {
        self.try_bind_labeled(keys, None, command)
    }

    fn try_bind_labeled(
        mut self,
        keys: &str,
        label: Option<String>,
        command: C,
    ) -> Result<Self, KeyParseError> {
        let sequence = KeySequence::parse(keys)?;
        self.bindings.push(Binding {
            sequence,
            command,
            label,
        });
        Ok(self)
    }

    /// Whether this layer is active given the keymap's current runtime data.
    fn is_active(&self, data: &HashMap<String, String>) -> bool {
        self.when
            .iter()
            .all(|(key, value)| data.get(key).map(String::as_str) == Some(value.as_str()))
    }
}

/// The outcome of feeding one key to a [`Keymap`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dispatch<C> {
    /// A binding matched fully; the host should run this command.
    Command(C),
    /// The keys typed so far are a prefix of one or more longer sequences; the
    /// keymap is waiting for the next stroke and has consumed this key.
    Pending,
    /// No active binding matches; the host should handle the key itself.
    Unmatched,
}

/// A discoverable binding for a help surface: its key label, optional
/// description, and the command it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint<C> {
    /// The key sequence label, e.g. `"Ctrl+R"`.
    pub keys: String,
    /// The binding's help label, if one was given.
    pub label: Option<String>,
    /// The command the binding runs.
    pub command: C,
}

/// The keymap engine: a set of [`Layer`]s, a runtime-data store used to gate
/// them, and the pending multi-stroke state.
///
/// See the [module documentation](self) for the model and an example.
#[derive(Clone, Debug, Default)]
pub struct Keymap<C> {
    layers: Vec<Layer<C>>,
    data: HashMap<String, String>,
    pending: Vec<Chord>,
}

impl<C: Clone> Keymap<C> {
    /// An empty keymap.
    pub fn new() -> Self {
        Self {
            layers: Vec::new(),
            data: HashMap::new(),
            pending: Vec::new(),
        }
    }

    /// Add a layer (builder style).
    pub fn layer(mut self, layer: Layer<C>) -> Self {
        self.layers.push(layer);
        self
    }

    /// Add a layer in place.
    pub fn add_layer(&mut self, layer: Layer<C>) {
        self.layers.push(layer);
    }

    /// Set a runtime-data value, used to gate layers via [`Layer::when`].
    /// Changing data clears any pending multi-stroke sequence, since the
    /// bindings that could complete it may no longer be active.
    pub fn set_data(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.data.insert(key.into(), value.into());
        self.pending.clear();
    }

    /// Read a runtime-data value.
    pub fn data(&self, key: &str) -> Option<&str> {
        self.data.get(key).map(String::as_str)
    }

    /// The chords typed so far toward an unfinished multi-stroke sequence.
    pub fn pending(&self) -> &[Chord] {
        &self.pending
    }

    /// Abandon any pending multi-stroke sequence (e.g. on a focus change or a
    /// sequence-timeout tick).
    pub fn reset(&mut self) {
        self.pending.clear();
    }

    /// Feed one translated [`Key`] to the keymap and resolve it.
    ///
    /// Single-stroke bindings resolve immediately. For multi-stroke sequences
    /// the keymap accumulates strokes and returns [`Dispatch::Pending`] until a
    /// binding completes ([`Dispatch::Command`]) or the strokes dead-end
    /// ([`Dispatch::Unmatched`], after which the pending state is cleared and the
    /// final stroke is retried on its own so it can begin a fresh sequence).
    ///
    /// An exact match wins immediately even when it is also the prefix of a
    /// longer binding (so a bound `g` fires without waiting to see if `g g`
    /// follows). Author overlapping bindings accordingly.
    pub fn dispatch(&mut self, key: Key) -> Dispatch<C> {
        let chord = Chord::from_key(key);
        self.pending.push(chord);
        let (exact, prefix) = self.resolve(&self.pending);
        if let Some(command) = exact {
            self.pending.clear();
            return Dispatch::Command(command);
        }
        if prefix {
            return Dispatch::Pending;
        }
        // The accumulated strokes dead-ended. Drop them and give this final
        // stroke a fresh chance to start its own sequence.
        self.pending.clear();
        let single = [chord];
        let (exact, prefix) = self.resolve(&single);
        if let Some(command) = exact {
            return Dispatch::Command(command);
        }
        if prefix {
            self.pending.push(chord);
            return Dispatch::Pending;
        }
        Dispatch::Unmatched
    }

    /// Resolve a candidate stroke sequence against the active layers, returning
    /// the winning exact-match command (highest priority) and whether any active
    /// binding is a strict extension of the candidate (a live prefix).
    fn resolve(&self, candidate: &[Chord]) -> (Option<C>, bool) {
        let mut best: Option<(i32, &C)> = None;
        let mut prefix = false;
        for layer in &self.layers {
            if !layer.is_active(&self.data) {
                continue;
            }
            for binding in &layer.bindings {
                let seq = binding.sequence.chords();
                if seq == candidate {
                    if best.is_none_or(|(p, _)| layer.priority > p) {
                        best = Some((layer.priority, &binding.command));
                    }
                } else if seq.len() > candidate.len() && seq.starts_with(candidate) {
                    prefix = true;
                }
            }
        }
        (best.map(|(_, command)| command.clone()), prefix)
    }

    /// The bindings currently active (their layer's gate is satisfied),
    /// suitable for building a [`KeyHints`](crate::KeyHints) row or a help
    /// overlay. Higher-priority layers come first.
    pub fn hints(&self) -> Vec<Hint<C>> {
        let mut layers: Vec<&Layer<C>> = self
            .layers
            .iter()
            .filter(|l| l.is_active(&self.data))
            .collect();
        layers.sort_by(|a, b| b.priority.cmp(&a.priority));
        layers
            .iter()
            .flat_map(|layer| layer.bindings.iter())
            .map(|binding| Hint {
                keys: binding.sequence.display(),
                label: binding.label.clone(),
                command: binding.command.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Key, KeyCode};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Action {
        Search,
        Quit,
        Top,
        Bottom,
        Close,
    }

    fn key(code: KeyCode) -> Key {
        Key::new(code)
    }

    fn ctrl(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    #[test]
    fn parses_modifier_chords() {
        assert_eq!(
            Chord::parse("ctrl+r").unwrap(),
            Chord {
                code: KeyCode::Char('r'),
                ctrl: true,
                alt: false,
                shift: false,
            }
        );
        // Shift+Tab folds to BackTab, the key terminals actually report.
        assert_eq!(
            Chord::parse("alt+shift+tab").unwrap(),
            Chord {
                code: KeyCode::BackTab,
                ctrl: false,
                alt: true,
                shift: false,
            }
        );
    }

    #[test]
    fn parses_named_and_literal_keys() {
        assert_eq!(Chord::parse("enter").unwrap().code, KeyCode::Enter);
        assert_eq!(Chord::parse("space").unwrap().code, KeyCode::Char(' '));
        assert_eq!(Chord::parse("?").unwrap().code, KeyCode::Char('?'));
        // A trailing '+' is the literal plus key, modifiers and all.
        assert_eq!(
            Chord::parse("ctrl++").unwrap(),
            Chord {
                code: KeyCode::Char('+'),
                ctrl: true,
                alt: false,
                shift: false,
            }
        );
    }

    #[test]
    fn char_chords_ignore_shift() {
        // Shift is folded into the character, so a bare '?' never carries it.
        assert!(!Chord::parse("shift+?").unwrap().shift);
        let from = Chord::from_key(Key {
            code: KeyCode::Char('?'),
            ctrl: false,
            alt: false,
            shift: true,
        });
        assert!(!from.shift);
    }

    #[test]
    fn rejects_garbage_specs() {
        assert!(Chord::parse("").is_err());
        assert!(Chord::parse("bogus+r").is_err());
        assert!(KeySequence::parse("   ").is_err());
    }

    #[test]
    fn display_is_human_readable() {
        assert_eq!(Chord::parse("ctrl+r").unwrap().display(), "Ctrl+R");
        assert_eq!(Chord::parse("space").unwrap().display(), "Space");
        assert_eq!(Chord::parse("enter").unwrap().display(), "Enter");
        assert_eq!(
            KeySequence::parse("ctrl+x s").unwrap().display(),
            "Ctrl+X S"
        );
    }

    #[test]
    fn dispatches_single_chords() {
        let mut keymap = Keymap::new().layer(
            Layer::new("global")
                .bind("ctrl+r", Action::Search)
                .bind("ctrl+d", Action::Quit),
        );
        assert_eq!(
            keymap.dispatch(ctrl('r')),
            Dispatch::Command(Action::Search)
        );
        assert_eq!(keymap.dispatch(ctrl('d')), Dispatch::Command(Action::Quit));
        assert_eq!(keymap.dispatch(ctrl('z')), Dispatch::Unmatched);
    }

    #[test]
    fn resolves_multi_stroke_sequences() {
        let mut keymap = Keymap::new().layer(
            Layer::new("nav")
                .bind("g g", Action::Top)
                .bind("g e", Action::Bottom),
        );
        assert_eq!(keymap.dispatch(key(KeyCode::Char('g'))), Dispatch::Pending);
        assert_eq!(keymap.pending().len(), 1);
        assert_eq!(
            keymap.dispatch(key(KeyCode::Char('g'))),
            Dispatch::Command(Action::Top)
        );
        // Pending is cleared after a completed sequence.
        assert!(keymap.pending().is_empty());

        assert_eq!(keymap.dispatch(key(KeyCode::Char('g'))), Dispatch::Pending);
        assert_eq!(
            keymap.dispatch(key(KeyCode::Char('e'))),
            Dispatch::Command(Action::Bottom)
        );
    }

    #[test]
    fn dead_end_sequence_retries_final_stroke() {
        let mut keymap = Keymap::new().layer(
            Layer::new("nav")
                .bind("g g", Action::Top)
                .bind("ctrl+r", Action::Search),
        );
        // `g` starts a pending sequence; a following ctrl+r dead-ends `g g`, but
        // ctrl+r is itself a live binding and should fire.
        assert_eq!(keymap.dispatch(key(KeyCode::Char('g'))), Dispatch::Pending);
        assert_eq!(
            keymap.dispatch(ctrl('r')),
            Dispatch::Command(Action::Search)
        );
        assert!(keymap.pending().is_empty());
    }

    #[test]
    fn exact_match_wins_over_prefix() {
        // `g` is both a complete binding and the prefix of `g g`; the exact
        // match fires immediately without waiting.
        let mut keymap = Keymap::new().layer(
            Layer::new("nav")
                .bind("g", Action::Bottom)
                .bind("g g", Action::Top),
        );
        assert_eq!(
            keymap.dispatch(key(KeyCode::Char('g'))),
            Dispatch::Command(Action::Bottom)
        );
    }

    #[test]
    fn mode_gated_layers_activate_on_data() {
        let mut keymap = Keymap::new()
            .layer(Layer::new("global").bind("ctrl+d", Action::Quit))
            .layer(
                Layer::new("panel")
                    .when("mode", "panel")
                    .bind("q", Action::Close),
            );
        // The panel layer is dormant until the mode says so.
        assert_eq!(
            keymap.dispatch(key(KeyCode::Char('q'))),
            Dispatch::Unmatched
        );
        keymap.set_data("mode", "panel");
        assert_eq!(
            keymap.dispatch(key(KeyCode::Char('q'))),
            Dispatch::Command(Action::Close)
        );
        // The global layer stays active in every mode.
        assert_eq!(keymap.dispatch(ctrl('d')), Dispatch::Command(Action::Quit));
    }

    #[test]
    fn higher_priority_layer_wins_conflicts() {
        let mut keymap = Keymap::new()
            .layer(Layer::new("base").priority(0).bind("x", Action::Quit))
            .layer(Layer::new("over").priority(10).bind("x", Action::Close));
        assert_eq!(
            keymap.dispatch(key(KeyCode::Char('x'))),
            Dispatch::Command(Action::Close)
        );
    }

    #[test]
    fn changing_mode_clears_pending() {
        let mut keymap = Keymap::new().layer(Layer::new("nav").bind("g g", Action::Top));
        assert_eq!(keymap.dispatch(key(KeyCode::Char('g'))), Dispatch::Pending);
        keymap.set_data("mode", "other");
        assert!(keymap.pending().is_empty());
    }

    #[test]
    fn hints_list_active_bindings_by_priority() {
        let keymap = Keymap::new()
            .layer(Layer::new("global").priority(0).bind_labeled(
                "ctrl+r",
                "search",
                Action::Search,
            ))
            .layer(
                Layer::new("panel")
                    .priority(10)
                    .when("mode", "panel")
                    .bind_labeled("q", "close", Action::Close),
            );
        // Only the always-on layer contributes while no mode is set.
        let hints = keymap.hints();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].keys, "Ctrl+R");
        assert_eq!(hints[0].label.as_deref(), Some("search"));
        assert_eq!(hints[0].command, Action::Search);

        let mut keymap = keymap;
        keymap.set_data("mode", "panel");
        let hints = keymap.hints();
        // Higher-priority panel layer first.
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].command, Action::Close);
        assert_eq!(hints[1].command, Action::Search);
    }
}
