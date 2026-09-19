//! Work-area and fractional-geometry helpers.
//!
//! Windows remember their floating geometry as fractions of their output's
//! *work area* (the output minus layer-shell exclusive zones), so the layout
//! survives resolution changes and monitor rearrangements. These helpers turn
//! between absolute coordinates and those fractions, and reapply them when an
//! output's mode changes.

use smithay::{
    desktop::{Space, space::SpaceElement, layer_map_for_output},
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Rectangle, Size},
};

use super::{
    SnapGrid, WindowElement,
    ssd::{RelativeGeometry, RestoreTarget},
    xdg::{fullscreen_content_size, maximize_content_size, undecorated_content_size},
};

/// The area of an output that floating windows are laid out within, i.e. its
/// geometry minus any layer-shell exclusive zones (panels, docks, ...).
pub fn output_work_area(space: &Space<WindowElement>, output: &Output) -> Option<Rectangle<i32, Logical>> {
    let geo = space.output_geometry(output)?;
    // Refresh the exclusive zones: a layer surface may have changed its
    // reserved edge since the last arrange.
    let mut map = layer_map_for_output(output);
    map.arrange();
    let zone = map.non_exclusive_zone();
    let area = Rectangle::new(geo.loc + zone.loc, zone.size);
    (area.size.w > 0 && area.size.h > 0).then_some(area)
}

/// Clamp a dragged window's proposed top-left so its titlebar can't be moved
/// under a layer-shell exclusive zone (e.g. a top panel), keeping it grabbable.
/// The output under `pointer` decides which work area applies.
pub fn clamp_window_position(
    space: &Space<WindowElement>,
    window: &WindowElement,
    pointer: Point<f64, Logical>,
    proposed: Point<i32, Logical>,
) -> Point<i32, Logical> {
    let output = space
        .output_under(pointer)
        .next()
        .cloned()
        .or_else(|| output_for_window(space, window));
    let Some(area) = output.and_then(|output| output_work_area(space, &output)) else {
        return proposed;
    };
    Point::from((proposed.x, proposed.y.max(area.loc.y)))
}

/// The output a window belongs to.
///
/// [`Space::outputs_for_element`] returns its outputs in an unspecified order
/// (they are kept in a `HashMap`), which is wrong when a window overlaps more
/// than one output — e.g. a fullscreen window on a secondary monitor. Prefer
/// the output under the window's top-left corner and only fall back to the
/// arbitrary order/primary output when nothing contains it.
pub fn output_for_window(space: &Space<WindowElement>, window: &WindowElement) -> Option<Output> {
    let outputs = space.outputs_for_element(window);
    if let Some(loc) = space.element_location(window)
        && let Some(output) = outputs.iter().find(|output| {
            space
                .output_geometry(output)
                .map(|geo| geo.contains(loc))
                .unwrap_or(false)
        })
    {
        return Some(output.clone());
    }
    outputs
        .first()
        .cloned()
        .or_else(|| space.outputs().next().cloned())
}

/// Compute a window's geometry as fractions of a specific output's work area.
pub fn relative_geometry_of_output(
    space: &Space<WindowElement>,
    output: &Output,
    window: &WindowElement,
) -> Option<RelativeGeometry> {
    let area = output_work_area(space, output)?;
    let loc = space.element_location(window)?;
    Some(RelativeGeometry::capture(loc, window.geometry().size, area))
}

/// Compute a window's geometry as fractions of its output's work area.
pub fn relative_geometry_of(
    space: &Space<WindowElement>,
    window: &WindowElement,
) -> Option<RelativeGeometry> {
    let output = output_for_window(space, window)?;
    relative_geometry_of_output(space, &output, window)
}

/// Turn a fractional geometry's position back into an absolute location against
/// a specific output's work area, without needing a committed size. Used by
/// client-decorated windows, which restore their own size after unmaximizing.
pub fn absolute_location_for_output(
    space: &Space<WindowElement>,
    output: &Output,
    rel: RelativeGeometry,
) -> Option<Point<i32, Logical>> {
    Some(rel.location(output_work_area(space, output)?))
}

/// Turn a fractional geometry's position back into an absolute location.
pub fn absolute_location(
    space: &Space<WindowElement>,
    window: &WindowElement,
    rel: RelativeGeometry,
) -> Option<Point<i32, Logical>> {
    let output = output_for_window(space, window)?;
    absolute_location_for_output(space, &output, rel)
}

/// Turn a fractional geometry back into an absolute location and the
/// (undecorated) content size to configure the client with, against a specific
/// output's work area.
pub fn absolute_geometry_for_output(
    space: &Space<WindowElement>,
    output: &Output,
    rel: RelativeGeometry,
    is_ssd: bool,
) -> Option<(Point<i32, Logical>, Size<i32, Logical>)> {
    let area = output_work_area(space, output)?;
    Some((rel.location(area), rel.content_size(area, is_ssd)))
}

/// Turn a fractional geometry back into an absolute location and the
/// (undecorated) content size to configure the client with.
pub fn absolute_geometry(
    space: &Space<WindowElement>,
    window: &WindowElement,
    rel: RelativeGeometry,
) -> Option<(Point<i32, Logical>, Size<i32, Logical>)> {
    let output = output_for_window(space, window)?;
    absolute_geometry_for_output(space, &output, rel, window.is_ssd())
}

/// Resolve where `window` should return to for the given floating geometry.
/// Server-decorated windows restore the compositor-owned content size;
/// client-decorated windows keep their own size (`content: None`), so only the
/// position is restored.
pub fn restore_target(
    space: &Space<WindowElement>,
    window: &WindowElement,
    rel: RelativeGeometry,
) -> Option<RestoreTarget> {
    let output = output_for_window(space, window)?;
    let area = output_work_area(space, &output)?;
    let is_ssd = window.is_ssd();
    // A server-decorated window with no real geometry recorded (it maximized
    // before committing) has nothing to restore; the client picks the size.
    if is_ssd && (rel.w <= 0.0 || rel.h <= 0.0) {
        return None;
    }
    Some(RestoreTarget {
        loc: rel.location(area),
        content: is_ssd.then(|| rel.content_size(area, true)),
    })
}

/// Snapshot every floating window's position and size as a fraction of its
/// output's work area. Call this *before* changing the output's mode.
pub fn capture_relative_geometries(space: &Space<WindowElement>, output: &Output) {
    for window in space.elements_for_output(output) {
        if window.is_ghosting() {
            continue;
        }
        // Fullscreen and maximized windows are re-configured to the output
        // rather than scaled, so they don't need a snapshot.
        if window.decoration_state().header_bar.fullscreen || window.is_maximized() {
            continue;
        }
        if let Some(rel) = relative_geometry_of(space, window) {
            window.decoration_state().relative = Some(rel);
        }
    }
}

/// Reapply fractional geometry against the output's (possibly new) work area,
/// and refresh fullscreen/maximized windows to the new output size. Call this
/// *after* the output's mode has changed.
pub fn apply_relative_geometries(space: &mut Space<WindowElement>, output: &Output) {
    let Some(output_geo) = space.output_geometry(output) else {
        return;
    };
    let work_area = output_work_area(space, output).unwrap_or(output_geo);

    let windows: Vec<WindowElement> = space.elements_for_output(output).cloned().collect();
    for window in windows {
        if window.is_ghosting() {
            continue;
        }
        // An output mode change re-lays out every window, so any in-flight
        // transition is superseded by the new geometry.
        window.decoration_state().animation = None;
        if let Some(toplevel) = window.0.toplevel() {
            let fullscreen = window.decoration_state().header_bar.fullscreen;
            if fullscreen {
                let size = fullscreen_content_size(work_area.size, window.is_ssd());
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.size = Some(size);
                });
                toplevel.send_configure();
                space.map_element(window.clone(), work_area.loc, false);
                continue;
            }
            // A snapped window is reconfigured to its cell in the new work
            // area, not maximized to the whole output.
            if let Some(zone) = window.decoration_state().snap_zone() {
                let grid = SnapGrid::centered(work_area);
                let rect = grid.rect(zone, work_area);
                if let Some(snap) = window.decoration_state().snap.as_mut() {
                    snap.grid = grid;
                }
                let content = undecorated_content_size(rect.size, window.is_ssd());
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Maximized);
                    state.size = Some(content);
                });
                toplevel.send_configure();
                space.map_element(window.clone(), rect.loc, false);
                continue;
            }
            if window.is_maximized() {
                let size = maximize_content_size(work_area.size, window.is_ssd());
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Maximized);
                    state.size = Some(size);
                });
                toplevel.send_configure();
                space.map_element(window.clone(), work_area.loc, false);
                continue;
            }
        }

        let Some(rel) = window.decoration_state().relative else {
            continue;
        };
        if let Some((loc, size)) = absolute_geometry(space, &window, rel) {
            if let Some(toplevel) = window.0.toplevel() {
                toplevel.with_pending_state(|state| state.size = Some(size));
                toplevel.send_configure();
            }
            space.map_element(window.clone(), loc, false);
        }
    }
}
