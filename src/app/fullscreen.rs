//! Alternate-screen renderer.
//!
//! Fullscreen mode deliberately shares the regular presentation renderer so
//! both modes keep the same transcript, composer, colors, layout, and status
//! bar. The mode remains responsible for alternate-screen lifecycle and
//! fullscreen scrolling in `app::run_tui`.

use ratatui::Frame;

use super::App;

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    super::render::draw_shared(f, app);
}
