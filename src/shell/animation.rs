use std::time::{Duration, Instant};

use smithay::{
    backend::renderer::element::memory::MemoryRenderBuffer,
    utils::{Logical, Point, Size},
};

use super::ssd::{BORDER_WIDTH, HEADER_BAR_HEIGHT};

/// How long the maximize/unmaximize transition takes.
pub const WINDOW_ANIMATION_DURATION: Duration = Duration::from_millis(270);

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

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}
