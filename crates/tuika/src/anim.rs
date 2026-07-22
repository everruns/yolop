//! Animation helpers: easing curves and a frame phase.
//!
//! `tuika` has no internal clock — the host owns time and passes a monotonically
//! increasing frame counter (yolop uses `App::busy_frame`) into animated
//! components. This module turns that counter into normalized progress and
//! shapes it with easing curves, the minimal analog of OpenTUI's Timeline API
//! without a scheduler or reconciler.

/// Normalized time in `0.0..=1.0`.
pub type Phase = f32;

/// Linear identity curve.
pub fn linear(t: Phase) -> Phase {
    t.clamp(0.0, 1.0)
}

/// Quadratic ease-in (slow start).
pub fn ease_in(t: Phase) -> Phase {
    let t = t.clamp(0.0, 1.0);
    t * t
}

/// Quadratic ease-out (slow end).
pub fn ease_out(t: Phase) -> Phase {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Cubic ease-in-out (slow start and end).
pub fn ease_in_out(t: Phase) -> Phase {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let f = -2.0 * t + 2.0;
        1.0 - f * f * f / 2.0
    }
}

/// Triangle wave in `0.0..=1.0..=0.0` over one period, for ping-pong motion.
///
/// `frame` is the host's counter, `period` the number of frames for a full
/// there-and-back cycle.
pub fn ping_pong(frame: u64, period: u64) -> Phase {
    if period == 0 {
        return 0.0;
    }
    let pos = frame % period;
    let half = period as f32 / 2.0;
    let up = pos as f32 / half;
    if up <= 1.0 { up } else { 2.0 - up }
}

/// Sawtooth wave in `0.0..1.0`, wrapping every `period` frames, for looping
/// motion like an indeterminate marquee.
pub fn sawtooth(frame: u64, period: u64) -> Phase {
    if period == 0 {
        return 0.0;
    }
    (frame % period) as f32 / period as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_endpoints_and_midpoints() {
        for f in [linear, ease_in, ease_out, ease_in_out] {
            assert!((f(0.0) - 0.0).abs() < 1e-6);
            assert!((f(1.0) - 1.0).abs() < 1e-6);
        }
        // Cubic ease-in-out is symmetric about 0.5.
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-6);
        // Clamps out-of-range input.
        assert_eq!(linear(2.0), 1.0);
        assert_eq!(ease_out(-1.0), 0.0);
    }

    #[test]
    fn ping_pong_and_sawtooth_shapes() {
        assert!((ping_pong(0, 60) - 0.0).abs() < 1e-6);
        assert!((ping_pong(30, 60) - 1.0).abs() < 1e-6); // peak at half period
        assert!((ping_pong(60, 60) - 0.0).abs() < 1e-6); // back to start
        assert!((sawtooth(0, 10) - 0.0).abs() < 1e-6);
        assert!((sawtooth(5, 10) - 0.5).abs() < 1e-6);
        assert!((sawtooth(10, 10) - 0.0).abs() < 1e-6); // wraps
    }
}
