//! The darkening behind the panel's question, drawn by the compositor.
//!
//! It used to be the panel's own: a fullscreen [`DrawingArea`] in the dialog's
//! window, faded by easing that window's opacity on a 16 ms timer. Every tick of
//! that fade repainted the whole output-sized surface and re-uploaded it over
//! the Wayland socket, so the fade ran at a handful of steps a second instead of
//! at the monitor's rate. A layer-shell window is also placed on one output, so
//! the overlay stopped at that output's edge.
//!
//! Here there is no surface and no upload: the compositor blends one solid
//! colour over each output as it draws the frame it was already drawing, and
//! runs the fade on its own clock. The panel only says when to put the dim up
//! and when to take it down.
//!
//! [`DrawingArea`]: gtk4::DrawingArea

use std::{
    cell::RefCell,
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::element::{
        Kind,
        solid::{SolidColorBuffer, SolidColorRenderElement},
    },
    output::Output,
    utils::{Logical, Point, Scale, Size},
};

use crate::state::{AnvilState, Backend};

/// How dark the desktop goes behind the question.
///
/// The panel's overlay used 0.55, and this is that number: the question's plate
/// was designed to be read against this darkness.
pub const DIM_ALPHA: f32 = 0.55;

/// How long the dim takes to come up, or to go away.
///
/// The panel's `MENU_FADE`. The two ends are separate processes, so the number
/// is repeated rather than shared: it is a fade, not part of the protocol.
pub const DIM_FADE: Duration = Duration::from_millis(180);

/// The dim's clock: where it was when it was last told to change, and where it
/// is going.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dim {
    /// The alpha the current fade started from.
    from: f32,
    /// The alpha it is heading for: [`DIM_ALPHA`] or nothing.
    to: f32,
    /// When it started heading there.
    since: Instant,
}

impl Dim {
    /// Nothing over the output.
    ///
    /// `from` and `to` are both zero, so no clock is needed to know that a dim
    /// that was never asked for is nothing: [`Self::alpha`] answers without
    /// looking at the time.
    pub fn off() -> Self {
        Self {
            from: 0.0,
            to: 0.0,
            since: Instant::now(),
        }
    }

    /// Head for `on`, from wherever the dim is at `now`.
    ///
    /// Starting from the current value, not from zero, is what makes a change of
    /// mind mid-fade a change of direction rather than a flash: the question can
    /// be taken back while the dim is still coming up, and it goes down from
    /// where it had got to.
    pub fn towards(self, on: bool, now: Instant) -> Self {
        let to = if on { DIM_ALPHA } else { 0.0 };
        if to == self.to {
            // Already going there. Restarting the clock would stall a fade that
            // is in progress every time the panel repeated itself, and would
            // also turn a change of mind into a fresh fade from the old target.
            return self;
        }
        Self {
            from: self.alpha(now),
            to,
            since: now,
        }
    }

    /// How dark to draw at `now` (`0.0..=DIM_ALPHA`).
    pub fn alpha(&self, now: Instant) -> f32 {
        if self.from == self.to {
            return self.to;
        }
        let t = (now.saturating_duration_since(self.since).as_secs_f32()
            / DIM_FADE.as_secs_f32())
        .clamp(0.0, 1.0);
        self.from + (self.to - self.from) * smoothstep(t)
    }
}

/// Slowest and fastest at the ends, quickest in the middle.
///
/// The same curve the panel fades opacity on (`ease_in_out` there): a cubic is
/// far too extreme for opacity, and a dim that sits looking unchanged for most of
/// its clock and then arrives in three frames reads as a jump, not a fade.
fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// The buffer an output's dim is drawn from.
///
/// Kept per output and reused rather than rebuilt every frame: an element made
/// from a fresh buffer has a fresh id, and the damage tracker would then repaint
/// the whole output every frame for as long as the question was up — a static
/// question should cost nothing. Reused, only a change of colour is damage, and
/// a change of colour is exactly what the fade is.
struct DimBuffer(RefCell<SolidColorBuffer>);

/// The dim over `output`, at `alpha`.
///
/// A one-colour buffer stretched to the output: the cost is one solid quad, not
/// a pixel per frame.
pub fn element(output: &Output, alpha: f32) -> SolidColorRenderElement {
    let scale = output.current_scale().fractional_scale();
    let size = output
        .current_mode()
        .map(|mode| {
            let logical = output
                .current_transform()
                .transform_size(mode.size)
                .to_f64()
                .to_logical(scale);
            Size::<i32, Logical>::from((logical.w.round() as i32, logical.h.round() as i32))
        })
        .unwrap_or_default();
    output
        .user_data()
        .insert_if_missing(|| DimBuffer(RefCell::new(SolidColorBuffer::new(size, [0.0; 4]))));
    let buffer = output.user_data().get::<DimBuffer>().expect("just inserted");
    let mut buffer = buffer.0.borrow_mut();
    // The colour carries the fade, so its commit counter moves exactly when the
    // dim does.
    buffer.update(size, [0.0, 0.0, 0.0, alpha]);
    SolidColorRenderElement::from_buffer(
        &buffer,
        Point::from((0, 0)),
        Scale::from(scale),
        1.0,
        Kind::Unspecified,
    )
}

impl<BackendData: Backend> AnvilState<BackendData> {
    /// Put the dim up or take it down, from wherever it is now.
    pub fn set_dim(&mut self, on: bool) {
        self.dim = self.dim.towards(on, Instant::now());
    }

    /// How dark each output is at `now`.
    pub fn dim_alpha(&self, now: Instant) -> f32 {
        self.dim.alpha(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dim_comes_up_over_its_clock_and_stays() {
        let start = Instant::now();
        let dim = Dim::off().towards(true, start);
        assert_eq!(dim.alpha(start), 0.0, "it starts from nothing");
        let half = dim.alpha(start + DIM_FADE / 2);
        assert!(half > 0.0 && half < DIM_ALPHA, "it is part way at half time");
        assert_eq!(dim.alpha(start + DIM_FADE), DIM_ALPHA);
        assert_eq!(
            dim.alpha(start + DIM_FADE * 10),
            DIM_ALPHA,
            "and does not keep moving once it has arrived"
        );
    }

    #[test]
    fn the_dim_goes_away_again() {
        let start = Instant::now();
        let on = Dim::off().towards(true, start);
        let off = on.towards(false, start + DIM_FADE);
        assert_eq!(off.alpha(start + DIM_FADE), DIM_ALPHA, "it leaves from full");
        assert!(off.alpha(start + DIM_FADE * 3 / 2) > 0.0);
        assert_eq!(off.alpha(start + DIM_FADE * 2), 0.0);
    }

    #[test]
    fn a_change_of_mind_turns_from_where_the_dim_is() {
        let start = Instant::now();
        let halfway = start + DIM_FADE / 2;
        let on = Dim::off().towards(true, start);
        let part = on.alpha(halfway);
        assert!(part > 0.0 && part < DIM_ALPHA);
        let off = on.towards(false, halfway);
        assert!(
            (off.alpha(halfway) - part).abs() < f32::EPSILON,
            "the dim must not jump when it turns"
        );
        // And it reaches nothing one fade from the turn, not one fade from the
        // moment it first came up.
        assert_eq!(off.alpha(halfway + DIM_FADE), 0.0);
    }

    #[test]
    fn repeating_a_request_does_not_restart_the_fade() {
        let start = Instant::now();
        let dim = Dim::off().towards(true, start);
        let later = start + DIM_FADE / 2;
        let again = dim.towards(true, later);
        assert_eq!(again, dim);
        assert_eq!(again.alpha(start + DIM_FADE), DIM_ALPHA);
    }

    #[test]
    fn a_dim_that_was_never_asked_for_is_nothing() {
        let dim = Dim::off();
        assert_eq!(dim.alpha(Instant::now()), 0.0);
        assert_eq!(dim.alpha(Instant::now() + DIM_FADE * 100), 0.0);
    }

    #[test]
    fn the_fade_moves_at_a_visible_rate_and_never_overshoots() {
        let start = Instant::now();
        let dim = Dim::off().towards(true, start);
        let mut last = dim.alpha(start);
        for step in 1..=20 {
            let now = start + DIM_FADE * step / 20;
            let alpha = dim.alpha(now);
            assert!((0.0..=DIM_ALPHA).contains(&alpha), "{alpha} out of range");
            assert!(alpha >= last, "the fade went backwards");
            if step < 20 {
                // No long stretch where nothing visibly changes: the whole point
                // of the curve over a cubic.
                assert!(alpha > last, "the fade stalled at step {step}");
            }
            last = alpha;
        }
    }
}
