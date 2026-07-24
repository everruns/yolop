//! Native terminal affordances that live outside the cell grid.
//!
//! OSC 9;4 (originated by ConEmu) drives the terminal's *own* progress
//! indicator — Ghostty renders a bar across the top of the window, Windows
//! Terminal and ConEmu light the taskbar, WezTerm/Konsole/mintty support it
//! too. It is out-of-band: it moves no cursor and paints no cells, so it works
//! in both the inline and full-screen renderers, and terminals that don't
//! understand it silently swallow the unknown OSC.
//!
//! Sequence: `ESC ] 9 ; 4 ; <state> ; <progress> BEL`, where `progress` is
//! 0–100 and `state` is one of [`ProgressState`].

use std::io::{self, Write};

/// Mouse pointer shapes used by interactive terminal regions.
///
/// These are CSS cursor names, understood by Ghostty, Kitty, Foot, and recent
/// iTerm2/xterm releases through OSC 22. Other terminals ignore the sequence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PointerShape {
    /// Restore the terminal's configured pointer.
    #[default]
    Default,
    /// Show the pointing-hand cursor used for links.
    Pointer,
}

/// Encode an OSC 22 pointer-shape sequence.
pub fn encode_pointer_shape(shape: PointerShape) -> &'static str {
    match shape {
        PointerShape::Default => "\x1b]22;default\x1b\\",
        PointerShape::Pointer => "\x1b]22;pointer\x1b\\",
    }
}

/// Set the terminal's mouse pointer shape.
pub fn write_pointer_shape(out: &mut impl Write, shape: PointerShape) -> io::Result<()> {
    out.write_all(encode_pointer_shape(shape).as_bytes())?;
    out.flush()
}

/// OSC 9;4 progress states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressState {
    /// Hide the indicator (progress ignored).
    Clear,
    /// Normal determinate progress at the given percent.
    Normal,
    /// Error state (typically red), at the given percent.
    Error,
    /// Indeterminate / busy (percent ignored).
    Indeterminate,
    /// Warning state (typically yellow), at the given percent.
    Warning,
}

impl ProgressState {
    fn code(self) -> u8 {
        match self {
            ProgressState::Clear => 0,
            ProgressState::Normal => 1,
            ProgressState::Error => 2,
            ProgressState::Indeterminate => 3,
            ProgressState::Warning => 4,
        }
    }
}

/// Encode an OSC 9;4 progress sequence. Pure and unit-testable — no I/O.
pub fn encode(state: ProgressState, percent: u8) -> String {
    format!("\x1b]9;4;{};{}\x07", state.code(), percent.min(100))
}

/// Drives the terminal's native progress indicator, clearing it on drop so a
/// dropped or panicking session never leaves a stuck bar in the taskbar.
pub struct TerminalProgress {
    /// Tracks whether an indicator is currently shown, to avoid redundant
    /// writes and to know whether `Drop` must clear.
    active: bool,
}

impl TerminalProgress {
    /// Create a driver with no indicator shown yet.
    pub fn new() -> Self {
        Self { active: false }
    }

    fn write(&mut self, state: ProgressState, percent: u8) {
        // Best-effort: a failed progress write must never disrupt the session.
        let mut out = io::stdout();
        if out.write_all(encode(state, percent).as_bytes()).is_ok() {
            let _ = out.flush();
        }
        self.active = state != ProgressState::Clear;
    }

    /// Show the busy/indeterminate indicator.
    pub fn indeterminate(&mut self) {
        self.write(ProgressState::Indeterminate, 0);
    }

    /// Show determinate progress at `percent` (0–100).
    pub fn percent(&mut self, percent: u8) {
        self.write(ProgressState::Normal, percent);
    }

    /// Show the error indicator at `percent`.
    pub fn error(&mut self, percent: u8) {
        self.write(ProgressState::Error, percent);
    }

    /// Hide the indicator. Idempotent.
    pub fn clear(&mut self) {
        if self.active {
            self.write(ProgressState::Clear, 0);
        }
    }

    /// Whether an indicator is currently shown.
    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Default for TerminalProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TerminalProgress {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_pointer_shape_encoding() {
        assert_eq!(
            encode_pointer_shape(PointerShape::Default),
            "\x1b]22;default\x1b\\"
        );
        assert_eq!(
            encode_pointer_shape(PointerShape::Pointer),
            "\x1b]22;pointer\x1b\\"
        );
    }

    #[test]
    fn osc_progress_encoding() {
        // ESC ] 9 ; 4 ; state ; percent BEL
        assert_eq!(encode(ProgressState::Indeterminate, 0), "\x1b]9;4;3;0\x07");
        assert_eq!(encode(ProgressState::Normal, 50), "\x1b]9;4;1;50\x07");
        assert_eq!(encode(ProgressState::Clear, 0), "\x1b]9;4;0;0\x07");
        assert_eq!(encode(ProgressState::Error, 12), "\x1b]9;4;2;12\x07");
        // Percent is clamped to 100.
        assert_eq!(encode(ProgressState::Normal, 200), "\x1b]9;4;1;100\x07");
    }
}
