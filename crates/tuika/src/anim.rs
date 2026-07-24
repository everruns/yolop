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

/// An easing curve: a pure function shaping normalized progress. The functions
/// in this module ([`linear`], [`ease_in`], [`ease_out`], [`ease_in_out`]) all
/// have this signature, and a [`Keyframe`] stores one to shape the segment
/// leading into it.
pub type Easing = fn(Phase) -> Phase;

/// What a [`Timeline`] does once the playhead passes its last keyframe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Repeat {
    /// Hold the final keyframe's value forever (the default).
    #[default]
    Once,
    /// Jump back to the start and play again, indefinitely.
    Loop,
    /// Play forward then backward, alternating each cycle.
    PingPong,
}

/// One stop in a [`Timeline`]: a value reached at frame offset `at`, with the
/// [`Easing`] that shapes the segment *leading into* it from the previous stop.
#[derive(Clone, Copy, Debug)]
struct Keyframe {
    at: u64,
    value: f32,
    easing: Easing,
}

/// A keyframed animation track: the scheduler-free analog of OpenTUI's Timeline.
///
/// Where the easing functions above shape a single 0→1 ramp, a `Timeline`
/// interpolates a scalar through a sequence of value stops over time, each
/// segment eased independently, with optional looping or ping-pong. It owns no
/// clock and spawns nothing: like every animated component in tuika it is a pure
/// function of the host's frame counter, so [`sample`](Self::sample) is
/// deterministic and testable, and several timelines compose by the host sampling
/// each (one per animated property) rather than a retained tween tree.
///
/// ```
/// use tuika::anim::{Repeat, Timeline, ease_out};
///
/// // Slide 0 → 100 over 30 frames (eased), then hold.
/// let slide = Timeline::new()
///     .keyframe(0, 0.0)
///     .ease(30, 100.0, ease_out);
/// assert_eq!(slide.sample(0), 0.0);
/// assert_eq!(slide.sample(30), 100.0);
/// assert_eq!(slide.sample(999), 100.0); // Repeat::Once holds the end
///
/// // A looping 0 → 1 → 0 pulse over 20 frames.
/// let pulse = Timeline::new()
///     .keyframe(0, 0.0)
///     .keyframe(10, 1.0)
///     .keyframe(20, 0.0)
///     .repeat(Repeat::Loop);
/// assert_eq!(pulse.sample(30), 1.0); // frame 30 wraps to local frame 10
/// ```
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    /// Stops sorted ascending by `at`; a same-`at` insert replaces in place.
    keyframes: Vec<Keyframe>,
    repeat: Repeat,
}

impl Timeline {
    /// An empty timeline. Add stops with [`keyframe`](Self::keyframe) /
    /// [`ease`](Self::ease); an empty timeline samples to `0.0`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a stop of `value` at frame offset `at`, reached **linearly** from the
    /// previous stop. Order-independent: stops are kept sorted by `at`, and a
    /// second stop at the same `at` replaces the first.
    pub fn keyframe(self, at: u64, value: f32) -> Self {
        self.ease(at, value, linear)
    }

    /// Like [`keyframe`](Self::keyframe) but shaping the segment *into* this stop
    /// with `easing` (e.g. [`ease_out`]).
    pub fn ease(mut self, at: u64, value: f32, easing: Easing) -> Self {
        let kf = Keyframe { at, value, easing };
        match self.keyframes.binary_search_by_key(&at, |k| k.at) {
            Ok(i) => self.keyframes[i] = kf,
            Err(i) => self.keyframes.insert(i, kf),
        }
        self
    }

    /// Set the end behavior (default [`Repeat::Once`]).
    pub fn repeat(mut self, repeat: Repeat) -> Self {
        self.repeat = repeat;
        self
    }

    /// The frame offset of the last stop — the length of one play-through. Zero
    /// for an empty or single-stop timeline.
    pub fn duration(&self) -> u64 {
        self.keyframes.last().map(|k| k.at).unwrap_or(0)
    }

    /// Whether the timeline has finished — only ever true for [`Repeat::Once`]
    /// once `frame` reaches [`duration`](Self::duration). Looping timelines never
    /// complete.
    pub fn is_complete(&self, frame: u64) -> bool {
        matches!(self.repeat, Repeat::Once) && frame >= self.duration()
    }

    /// Map the monotonic host `frame` onto a local time in `0..=duration`,
    /// according to the repeat mode.
    fn local_time(&self, frame: u64) -> u64 {
        let duration = self.duration();
        if duration == 0 {
            return 0;
        }
        match self.repeat {
            Repeat::Once => frame.min(duration),
            Repeat::Loop => frame % duration,
            Repeat::PingPong => {
                let pos = frame % duration;
                // Even cycles play forward, odd cycles backward.
                if (frame / duration).is_multiple_of(2) {
                    pos
                } else {
                    duration - pos
                }
            }
        }
    }

    /// The interpolated value at host `frame`. Empty → `0.0`; before the first
    /// stop or after the last (under [`Repeat::Once`]) the value is clamped to
    /// that stop.
    pub fn sample(&self, frame: u64) -> f32 {
        match self.keyframes.as_slice() {
            [] => 0.0,
            [only] => only.value,
            keyframes => {
                let t = self.local_time(frame);
                // Find the segment [lo, hi] straddling t. `t <= duration` always.
                let hi = keyframes
                    .iter()
                    .position(|k| k.at >= t)
                    .unwrap_or(keyframes.len() - 1)
                    .max(1);
                let (lo, hi) = (&keyframes[hi - 1], &keyframes[hi]);
                let span = hi.at - lo.at;
                if span == 0 {
                    return hi.value;
                }
                let p = (t - lo.at) as f32 / span as f32;
                let eased = (hi.easing)(p);
                lo.value + (hi.value - lo.value) * eased
            }
        }
    }
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

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-4, "{a} != {b}");
    }

    #[test]
    fn timeline_empty_and_single_stop() {
        approx(Timeline::new().sample(0), 0.0);
        approx(Timeline::new().sample(100), 0.0);
        let one = Timeline::new().keyframe(5, 42.0);
        approx(one.sample(0), 42.0);
        approx(one.sample(999), 42.0);
        assert_eq!(one.duration(), 5);
    }

    #[test]
    fn timeline_linear_interpolates_and_holds() {
        let t = Timeline::new().keyframe(0, 0.0).keyframe(10, 100.0);
        approx(t.sample(0), 0.0);
        approx(t.sample(5), 50.0);
        approx(t.sample(10), 100.0);
        // Repeat::Once holds the final value past the end.
        approx(t.sample(50), 100.0);
        assert!(t.is_complete(10));
        assert!(!t.is_complete(9));
    }

    #[test]
    fn timeline_insertion_order_is_normalized() {
        // Stops added out of order still form an ascending, correctly-segmented
        // timeline.
        let a = Timeline::new()
            .keyframe(20, 2.0)
            .keyframe(0, 0.0)
            .keyframe(10, 1.0);
        approx(a.sample(5), 0.5);
        approx(a.sample(15), 1.5);
        // A same-`at` insert replaces in place rather than duplicating.
        let b = Timeline::new().keyframe(10, 1.0).keyframe(10, 9.0);
        approx(b.sample(10), 9.0);
        assert_eq!(b.duration(), 10);
    }

    #[test]
    fn timeline_easing_shapes_the_segment() {
        let eased = Timeline::new().keyframe(0, 0.0).ease(10, 1.0, ease_in);
        // ease_in is quadratic: midpoint sits below the linear 0.5.
        assert!(eased.sample(5) < 0.5);
        approx(eased.sample(0), 0.0);
        approx(eased.sample(10), 1.0);
    }

    #[test]
    fn timeline_loop_and_ping_pong() {
        let looping = Timeline::new()
            .keyframe(0, 0.0)
            .keyframe(10, 1.0)
            .keyframe(20, 0.0)
            .repeat(Repeat::Loop);
        approx(looping.sample(10), 1.0);
        approx(looping.sample(30), 1.0); // 30 % 20 == 10
        assert!(!looping.is_complete(1_000_000));

        let pp = Timeline::new()
            .keyframe(0, 0.0)
            .keyframe(10, 1.0)
            .repeat(Repeat::PingPong);
        approx(pp.sample(0), 0.0);
        approx(pp.sample(10), 1.0); // end of forward cycle
        approx(pp.sample(15), 0.5); // backward cycle, halfway home
        approx(pp.sample(20), 0.0); // back to start
    }
}
