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

/// The default fill of a snap preview, used until the panel reports the
/// desktop accent.
pub const SNAP_PREVIEW_COLOR: [f32; 3] = [0.42, 0.62, 0.95];
/// How opaque the preview is drawn at full fade-in.
pub const SNAP_PREVIEW_ALPHA: f32 = 0.35;

thread_local! {
    static PREVIEW_COLOR: Cell<[f32; 3]> = const { Cell::new(SNAP_PREVIEW_COLOR) };
}

/// Set the accent the snap preview is tinted with.
pub fn set_preview_color(rgb: [f32; 3]) {
    PREVIEW_COLOR.with(|color| color.set(rgb));
}

/// The accent the snap preview is tinted with.
pub fn preview_color() -> [f32; 3] {
    PREVIEW_COLOR.with(Cell::get)
}

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

    /// Whether this zone includes the work area's left edge.
    pub fn left_of_center(self) -> bool {
        matches!(
            self,
            SnapZone::LeftHalf
                | SnapZone::TopLeft
                | SnapZone::BottomLeft
                | SnapZone::TopHalf
                | SnapZone::BottomHalf
                | SnapZone::Maximize
        )
    }

    /// Whether this zone includes the work area's right edge.
    pub fn right_of_center(self) -> bool {
        matches!(
            self,
            SnapZone::RightHalf
                | SnapZone::TopRight
                | SnapZone::BottomRight
                | SnapZone::TopHalf
                | SnapZone::BottomHalf
                | SnapZone::Maximize
        )
    }

    /// Whether this zone includes the work area's top edge.
    pub fn above_center(self) -> bool {
        matches!(
            self,
            SnapZone::TopHalf
                | SnapZone::TopLeft
                | SnapZone::TopRight
                | SnapZone::LeftHalf
                | SnapZone::RightHalf
                | SnapZone::Maximize
        )
    }

    /// Whether this zone includes the work area's bottom edge.
    pub fn below_center(self) -> bool {
        matches!(
            self,
            SnapZone::BottomHalf
                | SnapZone::BottomLeft
                | SnapZone::BottomRight
                | SnapZone::LeftHalf
                | SnapZone::RightHalf
                | SnapZone::Maximize
        )
    }

    /// Whether the zone spans the full width (a horizontal half or maximize).
    pub fn spans_width(self) -> bool {
        self.left_of_center() && self.right_of_center()
    }

    /// Whether the zone spans the full height (a vertical half or maximize).
    pub fn spans_height(self) -> bool {
        self.above_center() && self.below_center()
    }

    /// The absolute rectangle this zone covers within `area`.
    ///
    /// A zone that spans a full axis (a half or maximize) ignores that axis's
    /// divider; quarters use both dividers.
    pub fn rect(self, area: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
        SnapGrid::centered(area).rect(self, area)
    }
}

/// The two dividers (column x, row y) a snapped layout is built from. Any zone
/// is a region of the work area cut by these dividers; a zone spanning an axis
/// ignores that axis's divider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapGrid {
    pub x: i32,
    pub y: i32,
}

impl SnapGrid {
    /// The default even grid for a work area.
    pub fn centered(area: Rectangle<i32, Logical>) -> Self {
        Self {
            x: area.loc.x + area.size.w / 2,
            y: area.loc.y + area.size.h / 2,
        }
    }

    /// The rectangle `zone` occupies within `area` for this grid.
    pub fn rect(self, zone: SnapZone, area: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
        if zone == SnapZone::Maximize {
            return area;
        }
        let left = area.loc.x;
        let top = area.loc.y;
        let right = area.loc.x + area.size.w;
        let bottom = area.loc.y + area.size.h;

        let (x0, x1) = if zone.spans_width() {
            (left, right)
        } else if zone.left_of_center() {
            (left, self.x)
        } else {
            (self.x, right)
        };
        let (y0, y1) = if zone.spans_height() {
            (top, bottom)
        } else if zone.above_center() {
            (top, self.y)
        } else {
            (self.y, bottom)
        };

        Rectangle::new(
            Point::from((x0, y0)),
            Size::from(((x1 - x0).max(1), (y1 - y0).max(1))),
        )
    }

    /// Move the divider(s) the resized `zone` controls to match its `intended`
    /// size, keeping the dividers inside the area.
    ///
    /// A left/above cell pushes the divider right/down as it grows; a
    /// right/below cell pushes it left/up. A zone spanning an axis has no
    /// divider to move on that axis.
    pub fn with_dragged_edges(
        mut self,
        zone: SnapZone,
        area: Rectangle<i32, Logical>,
        dragged: Edges,
        intended: Size<i32, Logical>,
    ) -> Self {
        let left = area.loc.x;
        let top = area.loc.y;
        let right = area.loc.x + area.size.w;
        let bottom = area.loc.y + area.size.h;

        if (dragged.left || dragged.right) && !zone.spans_width() {
            let moved = if zone.left_of_center() {
                left + intended.w
            } else {
                right - intended.w
            };
            self.x = moved.clamp(left + 1, right - 1);
        }
        if (dragged.top || dragged.bottom) && !zone.spans_height() {
            let moved = if zone.above_center() {
                top + intended.h
            } else {
                bottom - intended.h
            };
            self.y = moved.clamp(top + 1, bottom - 1);
        }

        self
    }
}

/// Which edges an interactive resize is dragging. A plain struct so the grid
/// math can be tested without the compositor's `ResizeEdge`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

#[cfg(test)]
mod grid_tests {
    use super::*;

    fn area() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((0, 0)), Size::from((1000, 800)))
    }

    #[test]
    fn default_grid_divides_evenly() {
        let area = area();
        let grid = SnapGrid::centered(area);
        assert_eq!(grid.rect(SnapZone::TopLeft, area), Rectangle::new(Point::from((0, 0)), Size::from((500, 400))));
        assert_eq!(grid.rect(SnapZone::LeftHalf, area).size, Size::from((500, 800)));
        assert_eq!(grid.rect(SnapZone::TopHalf, area).size, Size::from((1000, 400)));
    }

    #[test]
    fn growing_top_right_shrinks_every_other_cell() {
        let area = area();
        // Top-right grows from 500x400 to 700x500: divider moves left and down.
        let grid = SnapGrid::centered(area).with_dragged_edges(
            SnapZone::TopRight,
            area,
            Edges { left: true, bottom: true, ..Default::default() },
            Size::from((700, 500)),
        );
        assert_eq!(grid, SnapGrid { x: 300, y: 500 });

        assert_eq!(
            grid.rect(SnapZone::TopRight, area),
            Rectangle::new(Point::from((300, 0)), Size::from((700, 500)))
        );
        // Below loses height, left loses width, diagonal loses both.
        assert_eq!(grid.rect(SnapZone::BottomRight, area).size, Size::from((700, 300)));
        assert_eq!(grid.rect(SnapZone::TopLeft, area).size, Size::from((300, 500)));
        assert_eq!(grid.rect(SnapZone::BottomLeft, area).size, Size::from((300, 300)));
    }

    #[test]
    fn dragging_left_edge_of_right_cell_moves_divider() {
        let area = area();
        let grid = SnapGrid::centered(area).with_dragged_edges(
            SnapZone::TopRight,
            area,
            Edges { left: true, ..Default::default() },
            Size::from((600, 400)),
        );
        assert_eq!(grid.x, 400);
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
