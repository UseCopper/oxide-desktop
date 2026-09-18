//! Pointer-driven window snapping.
//!
//! While a window is being dragged, the pointer position is resolved against
//! the work area of the output under it. Releasing the drag inside one of the
//! edge bands tiles the window there:
//!
//! ```text
//!  ┌─────┬─────┐   corners  -> quarters
//!  │ TL  │ TR  │   sides    -> halves
//!  ├─────┼─────┤   bottom   -> bottom half
//!  │ BL  │ BR  │   top      -> maximize (like the titlebar button), or top
//!  └─────┴─────┘               half when the bottom half is already occupied
//! ```

use std::{
    cell::{Cell, RefCell},
    time::{Duration, Instant},
};

use smithay::{
    output::Output,
    utils::{Logical, Point, Rectangle, Size},
};

use super::ssd::RelativeGeometry;

/// How far (in logical pixels) from a work-area edge the pointer must be for
/// that edge to count as "slammed".
pub const SNAP_EDGE_MARGIN: i32 = 16;

/// How long the pointer must stay in a zone before its preview appears.
pub const SNAP_PREVIEW_DWELL: Duration = Duration::from_millis(100);
/// How long the preview takes to fade in or out.
pub const SNAP_PREVIEW_FADE: Duration = Duration::from_millis(150);

/// The translucent fill of a snap preview.
pub const SNAP_PREVIEW_COLOR: [f32; 4] = [0.42, 0.62, 0.95, 1.0];
/// How opaque the preview is drawn at full fade-in.
pub const SNAP_PREVIEW_ALPHA: f32 = 0.35;

/// One of the tile targets a dragged window can snap to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapZone {
    LeftHalf,
    RightHalf,
    TopHalf,
    BottomHalf,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Maximize,
}

impl SnapZone {
    /// The zone as fractions of a work area, for `absolute_geometry_for_output`.
    pub fn relative(self) -> RelativeGeometry {
        let (x, y, w, h) = match self {
            SnapZone::LeftHalf => (0.0, 0.0, 0.5, 1.0),
            SnapZone::RightHalf => (0.5, 0.0, 0.5, 1.0),
            SnapZone::TopHalf => (0.0, 0.0, 1.0, 0.5),
            SnapZone::BottomHalf => (0.0, 0.5, 1.0, 0.5),
            SnapZone::TopLeft => (0.0, 0.0, 0.5, 0.5),
            SnapZone::TopRight => (0.5, 0.0, 0.5, 0.5),
            SnapZone::BottomLeft => (0.0, 0.5, 0.5, 0.5),
            SnapZone::BottomRight => (0.5, 0.5, 0.5, 0.5),
            SnapZone::Maximize => (0.0, 0.0, 1.0, 1.0),
        };
        RelativeGeometry { x, y, w, h }
    }

    /// The absolute rectangle this zone covers within `area`. Odd work-area
    /// sizes give the left/top half the extra pixel so the halves never
    /// overlap.
    pub fn rect(self, area: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
        let left = area.loc.x;
        let top = area.loc.y;
        let half_w = area.size.w / 2;
        let half_h = area.size.h / 2;
        let right = left + half_w;
        let bottom = top + half_h;
        let right_w = area.size.w - half_w;
        let bottom_h = area.size.h - half_h;
        match self {
            SnapZone::LeftHalf => Rectangle::new(Point::from((left, top)), Size::from((half_w, area.size.h))),
            SnapZone::RightHalf => Rectangle::new(Point::from((right, top)), Size::from((right_w, area.size.h))),
            SnapZone::TopHalf => Rectangle::new(Point::from((left, top)), Size::from((area.size.w, half_h))),
            SnapZone::BottomHalf => Rectangle::new(Point::from((left, bottom)), Size::from((area.size.w, bottom_h))),
            SnapZone::TopLeft => Rectangle::new(Point::from((left, top)), Size::from((half_w, half_h))),
            SnapZone::TopRight => Rectangle::new(Point::from((right, top)), Size::from((right_w, half_h))),
            SnapZone::BottomLeft => Rectangle::new(Point::from((left, bottom)), Size::from((half_w, bottom_h))),
            SnapZone::BottomRight => Rectangle::new(Point::from((right, bottom)), Size::from((right_w, bottom_h))),
            SnapZone::Maximize => area,
        }
    }
}

/// The zone a global pointer position falls into within `area`, if any.
///
/// The top edge alone yields [`SnapZone::TopHalf`]; the caller may downgrade
/// that to fullscreen when the bottom half is free.
pub fn zone_at(area: Rectangle<i32, Logical>, point: Point<f64, Logical>) -> Option<SnapZone> {
    let margin = SNAP_EDGE_MARGIN as f64;
    let left = point.x <= area.loc.x as f64 + margin;
    let right = point.x >= (area.loc.x + area.size.w) as f64 - margin;
    let top = point.y <= area.loc.y as f64 + margin;
    let bottom = point.y >= (area.loc.y + area.size.h) as f64 - margin;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(SnapZone::TopLeft),
        (_, true, true, _) => Some(SnapZone::TopRight),
        (true, _, _, true) => Some(SnapZone::BottomLeft),
        (_, true, _, true) => Some(SnapZone::BottomRight),
        (true, _, _, _) => Some(SnapZone::LeftHalf),
        (_, true, _, _) => Some(SnapZone::RightHalf),
        (_, _, true, _) => Some(SnapZone::TopHalf),
        (_, _, _, true) => Some(SnapZone::BottomHalf),
        _ => None,
    }
}

/// Where a dragged window would land if released right now.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapTarget {
    pub output: Output,
    pub area: Rectangle<i32, Logical>,
    pub zone: SnapZone,
}

/// The translucent preview of the active snap target, stored on the output it
/// belongs to so the renderer can pick it up without threading state through.
///
/// The rectangle is kept while the preview fades out, and cleared once it is
/// fully transparent.
#[derive(Debug)]
pub struct SnapPreviewState {
    rect: RefCell<Option<Rectangle<i32, Logical>>>,
    alpha: Cell<f32>,
    target: Cell<f32>,
    last_update: Cell<Instant>,
}

impl Default for SnapPreviewState {
    fn default() -> Self {
        Self {
            rect: RefCell::new(None),
            alpha: Cell::new(0.0),
            target: Cell::new(0.0),
            last_update: Cell::new(Instant::now()),
        }
    }
}

impl SnapPreviewState {
    /// Show `rect`, fading in from the current alpha.
    pub fn show(&self, rect: Rectangle<i32, Logical>) {
        *self.rect.borrow_mut() = Some(rect);
        self.target.set(1.0);
    }

    /// Fade the preview out; the rectangle is retained until it is invisible.
    pub fn hide(&self) {
        self.target.set(0.0);
    }

    /// The current opacity, `0.0..=1.0`.
    pub fn alpha(&self) -> f32 {
        self.alpha.get()
    }

    /// The preview rectangle, present while it is visible or fading out.
    pub fn get(&self) -> Option<Rectangle<i32, Logical>> {
        *self.rect.borrow()
    }

    /// Advance the fade towards its target. Returns whether it changed.
    pub fn tick(&self, now: Instant) -> bool {
        let dt = now
            .saturating_duration_since(self.last_update.get())
            .as_secs_f32();
        self.last_update.set(now);

        let target = self.target.get();
        let current = self.alpha.get();
        if (current - target).abs() < f32::EPSILON {
            if target == 0.0 {
                *self.rect.borrow_mut() = None;
            }
            return false;
        }

        let step = if SNAP_PREVIEW_FADE.is_zero() {
            1.0
        } else {
            dt / SNAP_PREVIEW_FADE.as_secs_f32()
        };
        let next = if current < target {
            (current + step).min(target)
        } else {
            (current - step).max(target)
        };
        self.alpha.set(next);
        if next == 0.0 {
            *self.rect.borrow_mut() = None;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((100, 30)), Size::from((200, 100)))
    }

    #[test]
    fn detects_edges_and_corners() {
        let area = area();
        assert_eq!(zone_at(area, (105.0, 80.0).into()), Some(SnapZone::LeftHalf));
        assert_eq!(zone_at(area, (295.0, 80.0).into()), Some(SnapZone::RightHalf));
        assert_eq!(zone_at(area, (200.0, 35.0).into()), Some(SnapZone::TopHalf));
        assert_eq!(zone_at(area, (200.0, 125.0).into()), Some(SnapZone::BottomHalf));
        assert_eq!(zone_at(area, (105.0, 35.0).into()), Some(SnapZone::TopLeft));
        assert_eq!(zone_at(area, (295.0, 35.0).into()), Some(SnapZone::TopRight));
        assert_eq!(zone_at(area, (105.0, 125.0).into()), Some(SnapZone::BottomLeft));
        assert_eq!(zone_at(area, (295.0, 125.0).into()), Some(SnapZone::BottomRight));
        assert_eq!(zone_at(area, (200.0, 80.0).into()), None);
    }

    #[test]
    fn preview_fades_in_and_out_keeping_its_rect() {
        let preview = SnapPreviewState::default();
        let rect = Rectangle::new(Point::from((0, 0)), Size::from((10, 10)));
        let start = Instant::now();

        preview.show(rect);
        preview.tick(start);
        preview.tick(start + SNAP_PREVIEW_FADE);
        assert!((preview.alpha() - 1.0).abs() < 1e-6);
        assert_eq!(preview.get(), Some(rect));

        preview.hide();
        preview.tick(start + SNAP_PREVIEW_FADE * 2);
        assert!(preview.alpha().abs() < 1e-6);
        assert_eq!(preview.get(), None, "rect is dropped once invisible");
    }

    #[test]
    fn zones_tile_the_work_area_without_gaps() {
        let area = area();
        assert_eq!(
            SnapZone::LeftHalf.rect(area).size.w + SnapZone::RightHalf.rect(area).size.w,
            area.size.w
        );
        assert_eq!(
            SnapZone::TopHalf.rect(area).size.h + SnapZone::BottomHalf.rect(area).size.h,
            area.size.h
        );
        assert_eq!(SnapZone::BottomRight.rect(area).loc, Point::from((200, 80)));
    }
}
