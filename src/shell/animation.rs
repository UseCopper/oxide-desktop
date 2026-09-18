use std::time::{Duration, Instant};

use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer,
    utils::{Logical, Point, Size},
};

use super::ssd::{BORDER_WIDTH, HEADER_BAR_HEIGHT};

/// How long the maximize/unmaximize transition takes.
pub const WINDOW_ANIMATION_DURATION: Duration = Duration::from_millis(270);

/// How long a window takes to fade/scale in when it opens, or out when it
/// closes.
pub const WINDOW_VISIBILITY_DURATION: Duration = Duration::from_millis(150);

/// How much larger (as a fraction) than its final size a window is drawn at the
/// start of the open transition; the close transition grows out by the same
/// amount. `0.08` means 8% larger.
pub const VISIBILITY_SCALE_EXCESS: f64 = 0.08;

/// Which direction [`VisibilityAnimation`] plays in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityKind {
    /// Fade in with an ease-out curve, shrinking from a larger size.
    Open,
    /// Fade out with an ease-in curve, growing to a larger size.
    Close,
}

/// A window's open/close transition: a purely visual fade plus a scale about
/// the window's center. Unlike [`WindowAnimation`] it never changes the window's
/// layout, so the client is unaware of it.
#[derive(Debug, Clone)]
pub struct VisibilityAnimation {
    kind: VisibilityKind,
    start_time: Instant,
    duration: Duration,
}

impl VisibilityAnimation {
    pub fn open() -> Self {
        Self::new(VisibilityKind::Open, WINDOW_VISIBILITY_DURATION)
    }

    pub fn close() -> Self {
        Self::new(VisibilityKind::Close, WINDOW_VISIBILITY_DURATION)
    }

    pub fn new(kind: VisibilityKind, duration: Duration) -> Self {
        Self {
            kind,
            start_time: Instant::now(),
            duration,
        }
    }

    pub fn kind(&self) -> VisibilityKind {
        self.kind
    }

    fn progress(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.start_time).as_secs_f64();
        (elapsed / self.duration.as_secs_f64()).clamp(0.0, 1.0)
    }

    /// Whether the transition has fully played out at `now`.
    pub fn finished(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }

    /// The opacity (`0.0..=1.0`) and center scale to draw the window with at
    /// `now`.
    pub fn sample(&self, now: Instant) -> (f32, f64) {
        visibility_at(self.kind, self.progress(now))
    }
}

/// The opacity and center scale for a visibility transition at raw `progress`
/// (`0.0..=1.0`).
///
/// Opening uses ease-out for both curves (the window snaps towards its final
/// look early), closing uses ease-in (it lingers, then leaves quickly).
fn visibility_at(kind: VisibilityKind, progress: f64) -> (f32, f64) {
    let progress = progress.clamp(0.0, 1.0);
    match kind {
        VisibilityKind::Open => {
            let eased = ease_out_cubic(progress);
            (eased as f32, lerp(1.0 + VISIBILITY_SCALE_EXCESS, 1.0, eased))
        }
        VisibilityKind::Close => {
            let eased = ease_in_cubic(progress);
            (
                (1.0 - eased) as f32,
                lerp(1.0, 1.0 + VISIBILITY_SCALE_EXCESS, eased),
            )
        }
    }
}

/// A window's visual geometry: the decorated top-left corner plus the *content*
/// size (the area the client draws into). The SSD chrome is derived from the
/// content size, so animating the content is enough to animate the whole frame.
#[derive(Debug, Clone, Copy)]
pub struct WindowRect {
    pub loc: Point<f64, Logical>,
    pub content: Size<f64, Logical>,
}

impl WindowRect {
    pub fn from_geometry(loc: Point<i32, Logical>, content: Size<i32, Logical>) -> Self {
        Self {
            loc: loc.to_f64(),
            content: content.to_f64(),
        }
    }

    /// The full decorated size (content plus SSD chrome) for a server-decorated
    /// window; equal to the content size for client-decorated windows.
    pub fn decorated(&self, is_ssd: bool) -> Size<f64, Logical> {
        if is_ssd {
            Size::from((
                self.content.w + 2.0 * BORDER_WIDTH as f64,
                self.content.h + HEADER_BAR_HEIGHT as f64 + BORDER_WIDTH as f64,
            ))
        } else {
            self.content
        }
    }
}

/// An in-flight window transition. Sampling interpolates the geometry with an
/// ease-out curve; the window is drawn at the sampled rect for the whole
/// duration so it does not have to wait for the client to commit a new size.
#[derive(Debug, Clone)]
pub struct WindowAnimation {
    start: WindowRect,
    end: WindowRect,
    start_time: Instant,
    duration: Duration,
    snapshot: SnapshotState,
}

/// The pre-transition pixels of the window, captured once so the transition can
/// crossfade from them to the live frame.
#[derive(Debug, Clone)]
enum SnapshotState {
    /// Not captured yet; a renderer must do so before the next draw.
    Pending,
    /// The frozen pre-transition frame and its (logical) content size.
    Captured {
        buffer: MemoryRenderBuffer,
        size: Size<i32, Logical>,
    },
    /// Capture isn't possible (e.g. the window has no buffer yet).
    Unavailable,
}

impl WindowAnimation {
    pub fn new(start: WindowRect, end: WindowRect, duration: Duration) -> Self {
        Self {
            start,
            end,
            start_time: Instant::now(),
            duration,
            snapshot: SnapshotState::Pending,
        }
    }

    /// Whether a renderer still needs to capture the pre-transition pixels.
    pub fn needs_snapshot(&self) -> bool {
        matches!(self.snapshot, SnapshotState::Pending)
    }

    /// The frozen pre-transition frame and the content size it was captured at.
    pub fn snapshot(&self) -> Option<(&MemoryRenderBuffer, Size<i32, Logical>)> {
        match &self.snapshot {
            SnapshotState::Captured { buffer, size } => Some((buffer, *size)),
            _ => None,
        }
    }

    pub fn set_snapshot(&mut self, buffer: MemoryRenderBuffer, size: Size<i32, Logical>) {
        self.snapshot = SnapshotState::Captured { buffer, size };
    }

    pub fn set_snapshot_unavailable(&mut self) {
        self.snapshot = SnapshotState::Unavailable;
    }

    fn progress(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.start_time).as_secs_f64();
        (elapsed / self.duration.as_secs_f64()).clamp(0.0, 1.0)
    }

    /// Pin the animation's location to `loc`, leaving the content interpolation
    /// untouched. Used when an interactive drag ends mid-transition: the window
    /// stays where it was dropped instead of sliding to the original target
    /// while its size finishes animating.
    pub fn pin_location(&mut self, loc: Point<f64, Logical>) {
        self.start.loc = loc;
        self.end.loc = loc;
    }

    /// The interpolated geometry at `now`, along with the eased progress in
    /// `0.0..=1.0`. The same eased value drives the crossfade so the fade and
    /// the geometry move at the same rate.
    pub fn sample(&self, now: Instant) -> (WindowRect, f64) {
        let progress = self.progress(now);
        let eased = ease_out_cubic(progress);
        let rect = WindowRect {
            loc: Point::from((
                lerp(self.start.loc.x, self.end.loc.x, eased),
                lerp(self.start.loc.y, self.end.loc.y, eased),
            )),
            content: Size::from((
                lerp(self.start.content.w, self.end.content.w, eased),
                lerp(self.start.content.h, self.end.content.h, eased),
            )),
        };
        (rect, eased)
    }
}

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

fn ease_in_cubic(t: f64) -> f64 {
    t.powi(3)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_fades_in_and_shrinks_into_place() {
        let (alpha, scale) = visibility_at(VisibilityKind::Open, 0.0);
        assert!(alpha.abs() < 1e-6, "starts transparent");
        assert!((scale - (1.0 + VISIBILITY_SCALE_EXCESS)).abs() < 1e-6);

        let (alpha, scale) = visibility_at(VisibilityKind::Open, 1.0);
        assert!((alpha - 1.0).abs() < 1e-6, "ends opaque");
        assert!((scale - 1.0).abs() < 1e-6, "ends at natural size");
    }

    #[test]
    fn close_fades_out_and_grows_away() {
        let (alpha, scale) = visibility_at(VisibilityKind::Close, 0.0);
        assert!((alpha - 1.0).abs() < 1e-6);
        assert!((scale - 1.0).abs() < 1e-6);

        let (alpha, scale) = visibility_at(VisibilityKind::Close, 1.0);
        assert!(alpha.abs() < 1e-6, "ends transparent");
        assert!((scale - (1.0 + VISIBILITY_SCALE_EXCESS)).abs() < 1e-6);
    }

    #[test]
    fn close_mirrors_open() {
        // Closing is the open transition played backwards: the eased progress
        // and scale are symmetric around the half-way point.
        for progress in [0.0, 0.2, 0.5, 0.8, 1.0] {
            let open = visibility_at(VisibilityKind::Open, progress);
            let close = visibility_at(VisibilityKind::Close, 1.0 - progress);
            assert!((open.0 - close.0).abs() < 1e-6);
            assert!((open.1 - close.1).abs() < 1e-6);
        }
    }

    #[test]
    fn pinning_location_keeps_size_interpolation() {
        let start = WindowRect::from_geometry((0, 0).into(), (100, 100).into());
        let end = WindowRect::from_geometry((200, 200).into(), (50, 50).into());
        let mut animation = WindowAnimation::new(start, end, Duration::from_millis(100));
        animation.pin_location((10.0, 20.0).into());
        let (rect, _) = animation.sample(Instant::now());
        assert_eq!(rect.loc, Point::from((10.0, 20.0)));
        assert!(rect.content.w <= 100.0 && rect.content.w >= 50.0);
    }

    #[test]
    fn finished_only_after_the_duration() {
        let duration = Duration::from_millis(100);
        let before = Instant::now();
        let animation = VisibilityAnimation::new(VisibilityKind::Open, duration);
        let after = Instant::now() + duration;
        assert!(!animation.finished(before));
        assert!(animation.finished(after));
        assert!(animation.finished(after + duration));
    }
}
