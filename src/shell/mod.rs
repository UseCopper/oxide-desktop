use std::cell::RefCell;
use std::time::Instant;

#[cfg(feature = "xwayland")]
use smithay::xwayland::XWaylandClientData;

#[cfg(feature = "udev")]
use smithay::wayland::drm_syncobj::DrmSyncobjCachedState;

use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    desktop::{
        LayerSurface, PopupKind, PopupManager, Space, WindowSurface, WindowSurfaceType,
        layer_map_for_output, space::SpaceElement,
    },
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    reexports::{
        calloop::Interest,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer::WlBuffer, wl_output, wl_surface::WlSurface},
        },
    },
    utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER, Serial, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            TraversalAction, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface, with_states,
            with_surface_tree_upward,
        },
        dmabuf::get_dmabuf,
        shell::{
            wlr_layer::{
                Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::{XdgToplevelSurfaceData},
        },
    },
};

use crate::{
    ClientState,
    focus::KeyboardFocusTarget,
    state::{AnvilState, Backend},
};

mod animation;
mod capture;
mod element;
mod geometry;
mod grabs;
mod snap;
pub(crate) mod ssd;
#[cfg(feature = "xwayland")]
mod x11;
mod xdg;

pub use self::animation::*;
pub use self::capture::*;
pub use self::element::*;
pub use self::geometry::*;
pub use self::grabs::*;
pub use self::snap::*;

use self::ssd::{
    BORDER_WIDTH, CLOSE_TIMEOUT, HEADER_BAR_HEIGHT, Snap, VisibilityState,
};

use self::xdg::{decorated_content_size, handle_toplevel_commit, undecorated_content_size};

thread_local! {
    /// A freshly mapped window waiting for keyboard focus. `place_new_window`
    /// runs with only a `Space`, so the actual focus is applied by
    /// [`AnvilState::focus_new_windows`] right after.
    static PENDING_FOCUS: RefCell<Option<WindowElement>> = const { RefCell::new(None) };
}

#[derive(Default)]
pub struct FullscreenSurface(RefCell<Option<WindowElement>>);

impl FullscreenSurface {
    pub fn set(&self, window: WindowElement) {
        *self.0.borrow_mut() = Some(window);
    }

    pub fn get(&self) -> Option<WindowElement> {
        let mut window = self.0.borrow_mut();
        if window.as_ref().map(|w| !w.alive()).unwrap_or(false) {
            *window = None;
        }
        window.clone()
    }

    pub fn clear(&self) -> Option<WindowElement> {
        self.0.borrow_mut().take()
    }
}

impl<BackendData: Backend> BufferHandler for AnvilState<BackendData> {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl<BackendData: Backend> CompositorHandler for AnvilState<BackendData> {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        #[cfg(feature = "xwayland")]
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        panic!("Unknown client data type")
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, move |state, _dh, surface| {
            #[cfg(feature = "udev")]
            let mut acquire_point = None;
            let maybe_dmabuf = with_states(surface, |surface_data| {
                #[cfg(feature = "udev")]
                acquire_point.clone_from(
                    &surface_data
                        .cached_state
                        .get::<DrmSyncobjCachedState>()
                        .pending()
                        .acquire_point,
                );
                surface_data
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    })
            });
            if let Some(dmabuf) = maybe_dmabuf {
                #[cfg(feature = "udev")]
                if let Some(acquire_point) = acquire_point {
                    if let Ok((blocker, source)) = acquire_point.generate_blocker() {
                        let Some(client) = surface.client() else {
                            return;
                        };
                        let res = state.handle.insert_source(source, move |_, _, data| {
                            let dh = data.display_handle.clone();
                            data.client_compositor_state(&client).blocker_cleared(data, &dh);
                            Ok(())
                        });
                        if res.is_ok() {
                            add_blocker(surface, blocker);
                            return;
                        }
                    }
                }
                if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
                    if let Some(client) = surface.client() {
                        let res = state.handle.insert_source(source, move |_, _, data| {
                            let dh = data.display_handle.clone();
                            data.client_compositor_state(&client).blocker_cleared(data, &dh);
                            Ok(())
                        });
                        if res.is_ok() {
                            add_blocker(surface, blocker);
                        }
                    }
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.backend_data.early_import(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root) {
                window.0.on_commit();
                // Keep the buffers the client just committed alive, so an
                // app-triggered close can be drawn from them after the surface
                // is gone. This is a cheap `Arc` clone per surface, not a GPU
                // copy. Done for any surface in the window so subsurface content
                // (e.g. Firefox) is captured too.
                if let Some(frame) = capture_surface_tree(&root) {
                    window.set_last_frame(frame);
                }

                // X11 windows have no Wayland resize pipeline: only refresh
                // their frame cache (done above) and skip the toplevel path.
                if &root == surface && window.is_wayland() {
                    // Snapshot the resize state here, after
                    // `on_commit_buffer_handler`/`Window::on_commit` have refreshed
                    // the render surface size and bounding box. Post-commit hooks
                    // run *before* this handler in Smithay, so snapshotting there
                    // would read the previous commit's size and let the SSD frame
                    // lead the client buffer by one frame.
                    handle_toplevel_commit(&mut self.space, surface);

                    let buffer_offset = with_states(surface, |states| {
                        states
                            .cached_state
                            .get::<SurfaceAttributes>()
                            .current()
                            .buffer_delta
                            .take()
                    });

                    if let Some(buffer_offset) = buffer_offset
                        && let Some(current_loc) = self.space.element_location(&window)
                    {
                        self.space.map_element(window, current_loc + buffer_offset, false);
                    }
                }
            }
        }
        self.popups.commit(surface);

        if matches!(&self.cursor_status, CursorImageStatus::Surface(cursor_surface) if cursor_surface == surface)
        {
            with_states(surface, |states| {
                let cursor_image_attributes = states.data_map.get::<CursorImageSurfaceData>();

                if let Some(Ok(mut cursor_image_attributes)) =
                    cursor_image_attributes.map(|attrs| attrs.lock())
                {
                    let buffer_delta = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer_delta
                        .take();
                    if let Some(buffer_delta) = buffer_delta {
                        tracing::trace!(hotspot = ?cursor_image_attributes.hotspot, ?buffer_delta, "decrementing cursor hotspot");
                        cursor_image_attributes.hotspot -= buffer_delta;
                    }
                }
            });
        }

        if matches!(&self.dnd_icon, Some(icon) if &icon.surface == surface)
            && let Some(dnd_icon) = self.dnd_icon.as_mut()
        {
            with_states(&dnd_icon.surface, |states| {
                let buffer_delta = states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer_delta
                    .take()
                    .unwrap_or_default();
                tracing::trace!(offset = ?dnd_icon.offset, ?buffer_delta, "moving dnd offset");
                dnd_icon.offset += buffer_delta;
            });
        }

        ensure_initial_configure(surface, &self.space, &mut self.popups)
    }
}

impl<BackendData: Backend> WlrLayerShellHandler for AnvilState<BackendData> {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let Some(output) = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned())
        else {
            tracing::warn!("Dropping layer surface with no output available");
            return;
        };
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!(?err, "Failed to map layer surface");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        if let Some((mut map, layer)) = self.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map
                .layers()
                .find(|&layer| layer.layer_surface() == &surface)
                .cloned();
            layer.map(|layer| (map, layer))
        }) {
            map.unmap_layer(&layer);
        }
    }
}

impl<BackendData: Backend> AnvilState<BackendData> {
    /// Give keyboard focus to any window `place_new_window` just mapped.
    pub fn focus_new_windows(&mut self) {
        let window = PENDING_FOCUS.with(|cell| cell.borrow_mut().take());
        let Some(window) = window else {
            return;
        };
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(window.into()), SERIAL_COUNTER.next_serial());
        }
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<WindowElement> {
        self.space
            .elements()
            .find(|window| {
                window.wl_surface().map(|s| &*s == surface).unwrap_or(false)
                    || window_is_x11_surface(window, surface)
            })
            .cloned()
    }

    /// Start an ease-out transition of `window` from its current geometry to
    /// the given content size and location. The client keeps drawing at its own
    /// pace; the frame and content are stretched to the animated rect until it
    /// catches up.
    pub fn animate_window(
        &mut self,
        window: &WindowElement,
        target_content: Size<i32, Logical>,
        target_loc: Point<i32, Logical>,
    ) {
        let Some(start_loc) = self.space.element_location(window) else {
            return;
        };
        let start = WindowRect::from_geometry(start_loc, window.resize_content_size());
        let end = WindowRect::from_geometry(target_loc, target_content);
        if start.loc == end.loc && start.content == end.content {
            return;
        }
        window.decoration_state().animation =
            Some(WindowAnimation::new(start, end, WINDOW_ANIMATION_DURATION));
    }

    /// Start an unmaximize transition for a client-decorated window, whose
    /// restored size is chosen by the client and so isn't known yet. The window
    /// is frozen at its current (maximized) geometry, keeping the pre-maximize
    /// frame for the crossfade, and the commit handler fills in the final size
    /// with [`WindowAnimation::resolve_content`] once the client commits it.
    pub fn animate_client_unmaximize(
        &mut self,
        window: &WindowElement,
        target_loc: Point<i32, Logical>,
    ) {
        let Some(start_loc) = self.space.element_location(window) else {
            return;
        };
        let content = window.resize_content_size();
        let start = WindowRect::from_geometry(start_loc, content);
        let end = WindowRect::from_geometry(target_loc, content);
        let mut animation = WindowAnimation::new(start, end, WINDOW_ANIMATION_DURATION);
        animation.wait_for_content();
        // Raise/activate without moving: the animation drives the location once
        // the client has chosen its size.
        self.space.map_element(window.clone(), start_loc, true);
        window.decoration_state().animation = Some(animation);
    }

    /// Pin any in-flight transition of `window` to its current location. Called
    /// when an interactive drag ends so the window stays where it was dropped
    /// while the size transition finishes.
    pub fn pin_window_animation(&self, window: &WindowElement) {
        let Some(loc) = self.space.element_location(window) else {
            return;
        };
        if let Some(animation) = window.decoration_state().animation.as_mut() {
            animation.pin_location(loc.to_f64());
        }
    }

    /// Resolve the snap target under `global`. `exclude` is the window being
    /// dragged: it is ignored when deciding whether the bottom half is
    /// occupied, which is what turns a top-edge slam into a top-half snap
    /// instead of fullscreen.
    pub fn snap_zone_at(
        &self,
        global: Point<f64, Logical>,
        exclude: Option<&WindowElement>,
    ) -> Option<SnapTarget> {
        let output = self.space.output_under(global).next().cloned()?;
        let area = output_work_area(&self.space, &output)?;
        let mut zone = snap::zone_at(area, global)?;
        if zone == SnapZone::TopHalf && !self.bottom_half_occupied(&output, area, exclude) {
            zone = SnapZone::Maximize;
        }
        Some(SnapTarget {
            output,
            area,
            zone,
        })
    }

    /// Whether any window other than `exclude` currently fills the bottom half
    /// of `area` on `output`.
    fn bottom_half_occupied(
        &self,
        output: &Output,
        area: Rectangle<i32, Logical>,
        exclude: Option<&WindowElement>,
    ) -> bool {
        const TOLERANCE: i32 = 4;
        let target = SnapZone::BottomHalf.rect(area);
        self.space.elements_for_output(output).any(|window| {
            if window.is_ghosting() || exclude == Some(window) {
                return false;
            }
            let Some(loc) = self.space.element_location(window) else {
                return false;
            };
            let size = window.geometry().size;
            (loc.x - target.loc.x).abs() <= TOLERANCE
                && (loc.y - target.loc.y).abs() <= TOLERANCE
                && (size.w - target.size.w).abs() <= TOLERANCE
                && (size.h - target.size.h).abs() <= TOLERANCE
        })
    }

    /// Record the snap zone under the pointer. Whenever the zone changes the
    /// dwell timer restarts and any preview starts fading out; the preview for
    /// the new zone is shown by [`Self::tick_snap_preview`] once the pointer
    /// has stayed there long enough.
    pub fn note_snap_target(&mut self, target: Option<SnapTarget>) {
        if self.snap_candidate.as_ref() == target.as_ref() {
            return;
        }
        self.snap_candidate = target;
        self.snap_candidate_since = Instant::now();
        // Fade out whatever was showing; the new zone fades in after the dwell.
        self.hide_snap_preview();
    }

    /// Fade every snap preview out.
    fn hide_snap_preview(&self) {
        for output in self.space.outputs() {
            if let Some(preview) = output.user_data().get::<SnapPreviewState>() {
                preview.hide();
            }
        }
    }

    /// Drop the snap candidate and fade the preview out.
    pub fn clear_snap_preview(&mut self) {
        self.snap_candidate = None;
        self.hide_snap_preview();
    }

    /// Advance the snap preview: show it once the pointer has dwelled in the
    /// current zone for long enough, and fade all previews toward their target.
    pub fn tick_snap_preview(&mut self, now: Instant) {
        let dwelled = self.snap_candidate.is_some()
            && now.saturating_duration_since(self.snap_candidate_since) >= SNAP_PREVIEW_DWELL;
        let show = if dwelled {
            self.snap_candidate.clone()
        } else {
            None
        };

        for output in self.space.outputs() {
            if let Some(target) = show.as_ref().filter(|target| target.output == *output) {
                output
                    .user_data()
                    .insert_if_missing(SnapPreviewState::default);
                // Preview the cell the window will actually occupy: reuse the
                // group's current divider grid so the box matches the result,
                // not a freshly centered zone.
                let grid = self
                    .group_grid(output)
                    .unwrap_or_else(|| SnapGrid::centered(target.area));
                if let Some(preview) = output.user_data().get::<SnapPreviewState>() {
                    preview.show(grid.rect(target.zone, target.area));
                }
            }
            if let Some(preview) = output.user_data().get::<SnapPreviewState>() {
                preview.tick(now);
            }
        }
    }

    /// Tile `window` into `target`'s zone. The top zone hands off to the
    /// titlebar maximize button's behaviour; every other zone configures the
    /// client to the zone and animates the frame into place, remembering the
    /// floating geometry so a later drag can restore it.
    pub fn apply_snap(&mut self, window: &WindowElement, target: &SnapTarget) {
        if window.is_ghosting() || self.space.element_location(window).is_none() {
            return;
        }
        if target.zone == SnapZone::Maximize {
            self.toggle_maximize(window.clone());
            return;
        }
        // A fullscreen window covers the output; there is nothing to tile.
        if window.decoration_state().header_bar.fullscreen {
            return;
        }

        // Join the group already on this output: reuse its divider grid (which a
        // resize may have moved) so the new window lands in the existing layout
        // rather than a freshly centered one.
        let grid = self.group_grid(&target.output).unwrap_or_else(|| SnapGrid::centered(target.area));
        // Tile to exactly the rectangle the preview showed, so the two never
        // disagree on odd work-area sizes.
        let rect = grid.rect(target.zone, target.area);
        let floating = window
            .decoration_state()
            .snap
            .map(|snap| snap.floating)
            .or_else(|| relative_geometry_of_output(&self.space, &target.output, window));
        if let Some(floating) = floating {
            window.decoration_state().snap = Some(Snap {
                zone: target.zone,
                grid,
                floating,
            });
        }
        let animated = self.configure_snapped(window, rect, true);
        self.animate_window(window, animated, rect.loc);
    }

    /// The divider grid shared by the snapped windows on `output`, if any.
    fn group_grid(&self, output: &Output) -> Option<SnapGrid> {
        self.space
            .elements_for_output(output)
            .find_map(|window| window.decoration_state().snap.map(|snap| snap.grid))
    }

    /// Restore a snapped window to its floating geometry at `loc`, animating
    /// the transition like an unmaximize. `content` is the undecorated client
    /// size.
    pub fn restore_snapped(
        &mut self,
        window: &WindowElement,
        loc: Point<i32, Logical>,
        content: Size<i32, Logical>,
    ) {
        // Restoring supersedes the snap, so stop treating it as tiled.
        window.decoration_state().clear_snap();
        let is_ssd = window.is_ssd();
        let rect = Rectangle::new(loc, decorated_content_size(content, is_ssd));
        self.configure_snapped(window, rect, false);
        self.space.map_element(window.clone(), loc, true);
        self.animate_window(window, content, loc);
    }

    /// The work area `window` is snapped into, or `None` when it floats.
    pub fn snap_area_for(&self, window: &WindowElement) -> Option<Rectangle<i32, Logical>> {
        if !window.decoration_state().is_snapped() {
            return None;
        }
        let output = output_for_window(&self.space, window)?;
        output_work_area(&self.space, &output)
    }

    /// The edges a sibling in `zone` moves when the group divider changes: the
    /// divider-facing edge(s) of its cell.
    /// Push a moved division edge to the zones across it, reconfiguring
    /// neighbours from the updated grid. Called from the resize grab each frame.
    pub fn split_resize_neighbours_from_intent(
        &mut self,
        window: &WindowElement,
        area: Rectangle<i32, Logical>,
        edges: ResizeEdge,
        intended_size: Size<i32, Logical>,
    ) {
        let Some(snap) = window.decoration_state().snap else {
            return;
        };
        let zone = snap.zone;
        let grid = snap.grid;
        if zone == SnapZone::Maximize {
            return;
        }

        let dragged = Edges {
            left: edges.intersects(ResizeEdge::LEFT),
            right: edges.intersects(ResizeEdge::RIGHT),
            top: edges.intersects(ResizeEdge::TOP),
            bottom: edges.intersects(ResizeEdge::BOTTOM),
        };
        // The grid dividers live in work-area (decorated) coordinates, but the
        // resize intent is a *content* size. Grow it by the SSD chrome so the
        // divider lands where the decorated frame will be.
        let decorated = self::xdg::decorated_content_size(intended_size, window.is_ssd());
        let grid = grid.with_dragged_edges(zone, area, dragged, decorated);
        // Stop the whole group if any member can't shrink enough to fit its new
        // cell: clamp the divider so every present member stays at or above its
        // minimum size.
        let grid = self.clamp_grid_to_minimums(grid, area, window);
        if let Some(snap) = window.decoration_state().snap.as_mut() {
            snap.grid = grid;
        }
        self.reflow_group(window, grid, area);
    }

    /// Clamp the moved dividers so no present member's cell is smaller than that
    /// member's minimum decorated size. Returns the adjusted grid.
    fn clamp_grid_to_minimums(
        &self,
        grid: SnapGrid,
        area: Rectangle<i32, Logical>,
        resizing: &WindowElement,
    ) -> SnapGrid {
        let mut grid = grid;
        let members: Vec<(WindowElement, SnapZone)> = self
            .space
            .elements()
            .filter(|other| !other.is_ghosting())
            .filter_map(|other| {
                let zone = other.decoration_state().snap_zone()?;
                Some((other.clone(), zone))
            })
            .collect();

        let left = area.loc.x;
        let top = area.loc.y;
        let right = area.loc.x + area.size.w;
        let bottom = area.loc.y + area.size.h;

        for (member, zone) in &members {
            if member == resizing {
                // The dragged window's own min is already applied to its size.
                continue;
            }
            let (min_size, _) = self::grabs::toplevel_min_max_size(member);
            let is_ssd = member.is_ssd();
            let deco = if is_ssd {
                self::xdg::decorated_content_size(Size::from((0, 0)), true)
            } else {
                Size::from((0, 0))
            };
            let min_w = min_size.w + deco.w;
            let min_h = min_size.h + deco.h;

            // A left cell's width is (grid.x - left); a right cell's is
            // (right - grid.x). Same on the other axis.
            if !zone.spans_width() && min_w > 0 {
                if zone.left_of_center() {
                    // grid.x must be at least left + min_w.
                    grid.x = grid.x.max(left + min_w);
                } else {
                    grid.x = grid.x.min(right - min_w);
                }
            }
            if !zone.spans_height() && min_h > 0 {
                if zone.above_center() {
                    grid.y = grid.y.max(top + min_h);
                } else {
                    grid.y = grid.y.min(bottom - min_h);
                }
            }
        }

        // Keep the dividers inside the area after clamping.
        grid.x = grid.x.clamp(left + 1, right - 1);
        grid.y = grid.y.clamp(top + 1, bottom - 1);
        grid
    }

    /// The edges a sibling in `zone` moves when the group divider changes: the
    /// divider-facing edge(s) of its cell.
    fn sibling_edges(zone: SnapZone) -> ResizeEdge {
        let mut edges = ResizeEdge::NONE;
        if zone.left_of_center() && !zone.right_of_center() {
            edges |= ResizeEdge::RIGHT;
        } else if zone.right_of_center() && !zone.left_of_center() {
            edges |= ResizeEdge::LEFT;
        }
        if zone.above_center() && !zone.below_center() {
            edges |= ResizeEdge::BOTTOM;
        } else if zone.below_center() && !zone.above_center() {
            edges |= ResizeEdge::TOP;
        }
        edges
    }

    /// Push a moved division edge to the zones across it, reconfiguring
    /// neighbours from the updated grid.
    fn reflow_group(
        &mut self,
        window: &WindowElement,
        grid: SnapGrid,
        area: Rectangle<i32, Logical>,
    ) {
        let members: Vec<WindowElement> = self
            .space
            .elements()
            .filter(|other| !other.is_ghosting() && other.decoration_state().is_snapped())
            .cloned()
            .collect();

        for other in members {
            if &other == window {
                continue;
            }
            if other.decoration_state().header_bar.fullscreen {
                continue;
            }
            let Some(zone) = other.decoration_state().snap_zone() else {
                continue;
            };
            let rect = grid.rect(zone, area);
            if let Some(snap) = other.decoration_state().snap.as_mut() {
                snap.grid = grid;
            }
            let edges = Self::sibling_edges(zone);
            self::grabs::drive_sibling_resize(&other, &mut self.space, edges, rect);
        }
    }

    /// Re-tile the members left in `window`'s snap group after it leaves it, on
    /// a fresh centered grid over the output's work area. Their old grid may
    /// have had driven dividers, so resetting keeps the remaining group aligned
    /// now that one member is gone. Each member is reconfigured and animated
    /// into its new cell.
    pub fn recenter_snap_group(&mut self, window: &WindowElement) {
        let Some(area) = self.snap_area_for(window).or_else(|| {
            let output = output_for_window(&self.space, window)?;
            output_work_area(&self.space, &output)
        }) else {
            return;
        };
        let grid = SnapGrid::centered(area);
        let members: Vec<WindowElement> = self
            .space
            .elements()
            .filter(|other| {
                *other != window && !other.is_ghosting() && other.decoration_state().is_snapped()
            })
            .cloned()
            .collect();

        for other in members {
            if other.decoration_state().header_bar.fullscreen {
                continue;
            }
            let Some(zone) = other.decoration_state().snap_zone() else {
                continue;
            };
            if let Some(snap) = other.decoration_state().snap.as_mut() {
                snap.grid = grid;
            }
            let rect = grid.rect(zone, area);
            let content = self.configure_snapped(&other, rect, true);
            self.animate_window(&other, content, rect.loc);
        }
    }

    /// Finalize every sibling that was driven along with `window`'s resize, so
    /// they leave the special `Resizing` state and settle on their final size.
    pub fn finish_group_resizes(&mut self, window: &WindowElement, serial: Serial) {
        let members: Vec<WindowElement> = self
            .space
            .elements()
            .filter(|other| !other.is_ghosting() && other.decoration_state().is_snapped())
            .cloned()
            .collect();
        for other in members {
            if &other == window || other.decoration_state().header_bar.fullscreen {
                continue;
            }
            self::grabs::finish_sibling_resize(&other, &mut self.space, serial);
        }
    }
    /// return the window origin the drag should start from, keeping the pointer
    /// over the same spot on the titlebar. Returns `None` for a floating
    /// window. Used by the client-initiated move grabs.
    pub fn take_snap_restore_for_drag(
        &mut self,
        window: &WindowElement,
        pointer_global: Point<f64, Logical>,
    ) -> Option<Point<i32, Logical>> {
        let snap = window.decoration_state().snap.take()?;
        let rel = snap.floating;
        let current_loc = self.space.element_location(window)?;
        let decorated_size = self
            .space
            .element_geometry(window)
            .map(|geo| geo.size)
            .unwrap_or_default();
        let (_, content) = absolute_geometry(&self.space, window, rel)?;
        let restored = self::xdg::restore_drag_location(
            current_loc,
            decorated_size,
            pointer_global,
            Some(content),
            window.is_ssd(),
        );
        self.restore_snapped(window, restored, content);
        Some(restored)
    }

    /// Configure `window`'s client to occupy the decorated rectangle `rect`.
    /// Wayland clients are given the undecorated content size; X11's
    /// `configure` takes the frame rectangle directly. Returns the size to
    /// animate the frame to. When `maximized` is true the Wayland client is
    /// also told it is in maximized state (so CSD apps tile correctly).
    fn configure_snapped(
        &mut self,
        window: &WindowElement,
        rect: Rectangle<i32, Logical>,
        maximized: bool,
    ) -> Size<i32, Logical> {
        let is_ssd = window.is_ssd();
        let content = undecorated_content_size(rect.size, is_ssd);
        match window.0.underlying_surface() {
            WindowSurface::Wayland(toplevel) => {
                toplevel.with_pending_state(|state| {
                    if maximized {
                        state.states.set(xdg_toplevel::State::Maximized);
                    } else {
                        state.states.unset(xdg_toplevel::State::Maximized);
                    }
                    state.size = Some(content);
                });
                if toplevel.is_initial_configure_sent() {
                    toplevel.send_configure();
                }
                content
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(surface) => {
                if let Err(err) = surface.configure(rect) {
                    tracing::warn!(?err, "Failed to configure snapped X11 window");
                }
                rect.size
            }
        }
    }

    /// Advance every in-flight window animation. Called once per frame before
    /// rendering: updates where the window is mapped and retires animations
    /// whose client has caught up with the configured size.
    pub fn tick_animations(&mut self) {
        let now = Instant::now();
        let windows: Vec<WindowElement> = self.space.elements().cloned().collect();
        let mut retire_ghosts = Vec::new();
        for window in &windows {
            // Ghosts have no live surface: just retire them when their close
            // transition has finished.
            if window.is_ghosting() {
                let finished = window
                    .decoration_state()
                    .visibility
                    .animation
                    .as_ref()
                    .map(|animation| animation.finished(now))
                    .unwrap_or(true);
                if finished {
                    retire_ghosts.push(window.clone());
                }
                continue;
            }

            // A client-decorated unmaximize waits for the client to commit the
            // size it chose. If that never arrives, settle on the last committed
            // size so the transition can finish instead of hanging.
            let pending_timed_out = window
                .decoration_state()
                .animation
                .as_ref()
                .map(|animation| {
                    animation.content_pending()
                        && now.saturating_duration_since(animation.started_at())
                            >= CONTENT_PENDING_TIMEOUT
                })
                .unwrap_or(false);
            if pending_timed_out {
                let committed = window.resize_content_size();
                if let Some(animation) = window.decoration_state().animation.as_mut() {
                    animation.resolve_content(committed);
                }
            }

            // Clone the animation out before touching the window again: sampling
            // it and asking for the committed size both borrow the window state.
            let Some(animation) = window.decoration_state().animation.clone() else {
                continue;
            };
            let (rect, progress) = animation.sample(now);
            let loc = Point::<i32, Logical>::from((
                rect.loc.x.round() as i32,
                rect.loc.y.round() as i32,
            ));
            // While a client move grab owns the window, it drives the position;
            // the animation only supplies the (shrinking) size.
            let being_dragged = self.dragging_window.as_ref() == Some(window);
            if !being_dragged && self.space.element_location(window) != Some(loc) {
                self.space.relocate_element(window, loc);
            }
            // Once the curve is done, keep the animation (which holds the window
            // at its target geometry) until the client has adopted the new size,
            // so a slow client doesn't snap back to its old geometry.
            if progress >= 1.0 && !animation.content_pending() {
                let target: Size<i32, Logical> = rect.content.to_i32_round();
                let committed = window.resize_content_size();
                if (committed.w - target.w).abs() <= 1 && (committed.h - target.h).abs() <= 1 {
                    window.decoration_state().animation = None;
                }
            }
        }

        for window in retire_ghosts {
            // Unmap while still marked as a ghost so the guarded surface paths
            // are skipped, then release the ghost flag.
            self.space.unmap_elem(&window);
            window.end_ghost();
        }

        self.tick_visibility_animations(&windows, now);
        self.tick_snap_preview(now);
        #[cfg(feature = "panel")]
        self.tick_panel();
    }

    /// Advance every window's open/close transition: start opens that were
    /// waiting for their first buffer, retire finished opens, and deliver the
    /// close request once a close transition has played out.
    fn tick_visibility_animations(&mut self, windows: &[WindowElement], now: Instant) {
        let mut to_close = Vec::new();
        for window in windows {
            if window.is_ghosting() {
                continue;
            }
            // Resolve the committed size before borrowing the decoration state:
            // it reads that same state on the SSD path.
            let content = window.resize_content_size();
            let mut send_close = false;
            {
                let mut state = window.decoration_state();

                if state.visibility.open_pending
                    && state.visibility.animation.is_none()
                    && !state.visibility.closing
                    && content.w > 0
                    && content.h > 0
                {
                    state.visibility.open_pending = false;
                    state.visibility.animation = Some(VisibilityAnimation::open());
                }

                let finished = state
                    .visibility
                    .animation
                    .as_ref()
                    .map(|animation| (animation.kind(), animation.finished(now)));
                match finished {
                    Some((VisibilityKind::Open, true)) => {
                        state.visibility.animation = None;
                    }
                    Some((VisibilityKind::Close, true)) => {
                        if state.visibility.close_sent_at.is_none() {
                            state.visibility.close_sent_at = Some(now);
                            send_close = true;
                        }
                    }
                    _ => {}
                }

                // A client that ignores the close request should not stay hidden
                // forever: after a grace period, un-hide it.
                if let Some(sent_at) = state.visibility.close_sent_at
                    && now.saturating_duration_since(sent_at) > CLOSE_TIMEOUT
                {
                    state.visibility = VisibilityState::default();
                }
            }
            if send_close {
                to_close.push(window.clone());
            }
        }
        for window in to_close {
            self.send_close(&window);
        }
    }

    /// Move the keyboard focus off `window` if it currently holds it. Used when
    /// a window stops being interactive.
    pub fn clear_window_focus(&mut self, window: &WindowElement) {
        if let Some(keyboard) = self.seat.get_keyboard()
            && matches!(
                keyboard.current_focus(),
                Some(KeyboardFocusTarget::Window(w)) if w == window.0
            )
        {
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
    }

    /// Turn any window whose client is already gone into a closing ghost. Run
    /// this *before* `Space::refresh` removes dead elements, so an abrupt client
    /// exit (crash, SIGINT/disconnect) still animates from the kept buffers.
    pub fn reap_closing_windows(&mut self) {
        let dead: Vec<WindowElement> = self
            .space
            .elements()
            .filter(|window| !window.0.alive() && !window.is_ghosting())
            .cloned()
            .collect();
        for window in dead {
            self.begin_window_ghost(&window);
        }

        // Minimized windows are out of the space and never animate, so a
        // minimized window whose client exited would otherwise be kept (with
        // its buffers) forever.
        self.minimized.retain(|(window, _)| window.0.alive());
    }

    /// Start the close transition for a window whose client is already gone,
    /// drawing it from its cached frame. Returns without effect when there is no
    /// cached frame or the close already played out.
    pub fn begin_window_ghost(&mut self, window: &WindowElement) {        // The window is gone or going; make sure the keyboard isn't left
        // pointing at its dead surface.
        self.clear_window_focus(window);

        let now = Instant::now();
        let mut state = window.decoration_state();
        let Some(content) = state
            .last_frame
            .as_ref()
            .map(|frame| frame.geometry.size)
        else {
            return;
        };
        // A ghost reserves room for the SSD chrome it will redraw, so its
        // geometry matches the decorated window.
        let is_ssd = state.is_ssd;
        let fullscreen = state.header_bar.fullscreen;
        let ghost_size: Size<i32, Logical> = if is_ssd {
            if fullscreen {
                Size::from((content.w, (HEADER_BAR_HEIGHT + content.h).max(0)))
            } else {
                Size::from((
                    content.w + 2 * BORDER_WIDTH,
                    content.h + HEADER_BAR_HEIGHT + BORDER_WIDTH,
                ))
            }
        } else {
            content
        };
        let animation = match state.visibility.animation.as_ref() {
            Some(animation) if animation.kind() == VisibilityKind::Close => {
                if animation.finished(now) {
                    return;
                }
                animation.clone()
            }
            _ => VisibilityAnimation::close(),
        };
        state.visibility.closing = true;
        state.visibility.open_pending = false;
        state.visibility.animation = Some(animation);
        drop(state);
        window.begin_ghost(ghost_size);
    }
}


#[derive(Default)]
pub struct SurfaceData {
    pub geometry: Option<Rectangle<i32, Logical>>,
    pub resize_state: ResizeState,
}

fn ensure_initial_configure(surface: &WlSurface, space: &Space<WindowElement>, popups: &mut PopupManager) {
    with_surface_tree_upward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            states
                .data_map
                .insert_if_missing(|| RefCell::new(SurfaceData::default()));
        },
        |_, _, _| true,
    );

    if let Some(window) = space
        .elements()
        .find(|window| window.wl_surface().map(|s| &*s == surface).unwrap_or(false))
        .cloned()
    {
        // send the initial configure if relevant
        #[cfg_attr(not(feature = "xwayland"), allow(irrefutable_let_patterns))]
        if let Some(toplevel) = window.0.toplevel() {
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });
            if !initial_configure_sent {
                toplevel.send_configure();
            }
        }

        // For xdg toplevels `handle_toplevel_commit` clears this *after* it has
        // anchored the final location, so the move stays atomic with the commit.
        // Windows without a toplevel (e.g. X11) have no such hook and finish here.
        #[cfg_attr(not(feature = "xwayland"), allow(irrefutable_let_patterns))]
        if window.0.toplevel().is_none() {
            with_states(surface, |states| {
                let mut data = states
                    .data_map
                    .get::<RefCell<SurfaceData>>()
                    .unwrap()
                    .borrow_mut();

                if let ResizeState::WaitingForCommit(_) = data.resize_state {
                    data.resize_state = ResizeState::NotResizing;
                }
            });
        }

        return;
    }

    if let Some(popup) = popups.find_popup(surface) {
        let popup = match popup {
            PopupKind::Xdg(ref popup) => popup,
            // Doesn't require configure
            PopupKind::InputMethod(ref _input_popup) => {
                return;
            }
        };

        if !popup.is_initial_configure_sent() {
            // NOTE: This should never fail as the initial configure is always
            // allowed.
            popup.send_configure().expect("initial configure failed");
        }

        return;
    };

    if let Some(output) = space.outputs().find(|o| {
        let map = layer_map_for_output(o);
        map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .is_some()
    }) {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        let mut map = layer_map_for_output(output);

        // arrange the layers before sending the initial configure
        // to respect any size the client may have sent
        map.arrange();
        // send the initial configure if relevant
        if !initial_configure_sent {
            let layer = map
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .unwrap();

            layer.layer_surface().send_configure();
        }
    };
}

fn place_new_window(
    space: &mut Space<WindowElement>,
    pointer_location: Point<f64, Logical>,
    window: &WindowElement,
    activate: bool,
) {
    // Center the window on the output under the pointer (the monitor the mouse
    // is on), falling back to the first output, then a fixed area.
    let output = space
        .output_under(pointer_location)
        .next()
        .or_else(|| space.outputs().next())
        .cloned();
    let area = output
        .as_ref()
        .and_then(|output| output_work_area(space, output))
        .unwrap_or_else(|| Rectangle::from_size((800, 800).into()));

    // Set the initial toplevel bounds.
    #[allow(irrefutable_let_patterns)]
    if let Some(toplevel) = window.0.toplevel() {
        toplevel.with_pending_state(|state| {
            state.bounds = Some(area.size);
        });
    }

    // `geometry()` is the decorated size, so the whole frame ends up centered.
    let size = window.geometry().size;
    let location = Point::from((
        area.loc.x + (area.size.w - size.w).max(0) / 2,
        area.loc.y + (area.size.h - size.h).max(0) / 2,
    ));
    space.map_element(window.clone(), location, activate);

    // `map_element(.., true)` only sets the xdg activated state; give the
    // window the actual keyboard focus too.
    if activate {
        state_focus(space, window);
    }
}

/// Hand `window` to [`AnvilState::focus_new_windows`], which applies the real
/// keyboard focus. `place_new_window` only has a `Space`, so it can't do it here.
fn state_focus(_space: &Space<WindowElement>, window: &WindowElement) {
    PENDING_FOCUS.with(|cell| *cell.borrow_mut() = Some(window.clone()));
}

pub fn fixup_positions(space: &mut Space<WindowElement>, pointer_location: Point<f64, Logical>) {
    // fixup outputs
    let mut offset = Point::<i32, Logical>::from((0, 0));
    for output in space.outputs().cloned().collect::<Vec<_>>().into_iter() {
        let size = space
            .output_geometry(&output)
            .map(|geo| geo.size)
            .unwrap_or_else(|| Size::from((0, 0)));
        space.map_output(&output, offset);
        layer_map_for_output(&output).arrange();
        offset.x += size.w;
    }

    // fixup windows
    let mut orphaned_windows = Vec::new();
    let outputs = space
        .outputs()
        .flat_map(|o| {
            let geo = space.output_geometry(o)?;
            let map = layer_map_for_output(o);
            let zone = map.non_exclusive_zone();
            Some(Rectangle::new(geo.loc + zone.loc, zone.size))
        })
        .collect::<Vec<_>>();
    for window in space.elements() {
        if window.is_ghosting() {
            continue;
        }
        let window_location = match space.element_location(window) {
            Some(loc) => loc,
            None => continue,
        };
        let geo_loc = window.bbox().loc + window_location;

        if !outputs.iter().any(|o_geo| o_geo.contains(geo_loc)) {
            orphaned_windows.push(window.clone());
        }
    }
    for window in orphaned_windows.into_iter() {
        place_new_window(space, pointer_location, &window, false);
    }
}

/// Whether `window` is an X11 window whose Wayland surface is `surface`.
#[cfg(feature = "xwayland")]
fn window_is_x11_surface(window: &WindowElement, surface: &WlSurface) -> bool {
    window
        .0
        .x11_surface()
        .and_then(|x11| x11.wl_surface())
        .as_ref()
        == Some(surface)
}

#[cfg(not(feature = "xwayland"))]
fn window_is_x11_surface(_window: &WindowElement, _surface: &WlSurface) -> bool {
    false
}
