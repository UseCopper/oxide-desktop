use std::cell::RefCell;
use std::time::Instant;

#[cfg(feature = "xwayland")]
use smithay::xwayland::XWaylandClientData;

#[cfg(feature = "udev")]
use smithay::wayland::drm_syncobj::DrmSyncobjCachedState;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ExportMem, ImportAll, ImportMem, Offscreen, Renderer, Texture,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, memory::MemoryRenderBuffer, surface::WaylandSurfaceRenderElement,
            },
            gles::GlesTexture,
            utils::{RendererSurfaceStateUserData, on_commit_buffer_handler},
        },
    },
    desktop::{
        LayerSurface, PopupKind, PopupManager, Space, WindowSurface, WindowSurfaceType,
        layer_map_for_output, space::SpaceElement,
    },
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    reexports::{
        calloop::Interest,
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer::WlBuffer, wl_output, wl_surface::WlSurface},
        },
    },
    utils::{Buffer, IsAlive, Logical, Physical, Point, Rectangle, SERIAL_COUNTER, Scale, Size, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            TraversalAction, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface, with_states,
            with_surface_tree_downward, with_surface_tree_upward,
        },
        dmabuf::get_dmabuf,
        shell::{
            wlr_layer::{
                Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::{SurfaceCachedState, XdgToplevelSurfaceData},
        },
    },
};

use crate::{
    ClientState,
    focus::KeyboardFocusTarget,
    state::{AnvilState, Backend},
};

mod animation;
mod element;
mod grabs;
mod snap;
pub(crate) mod ssd;
#[cfg(feature = "xwayland")]
mod x11;
mod xdg;

pub use self::animation::*;
pub use self::element::*;
pub use self::grabs::*;
pub use self::snap::*;

use self::ssd::{
    BORDER_WIDTH, CLOSE_TIMEOUT, HEADER_BAR_HEIGHT, LastFrame, RelativeGeometry, SurfaceFrame,
    VisibilityState,
};

use self::xdg::{decorated_content_size, handle_toplevel_commit, undecorated_content_size};

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
                if let Some(preview) = output.user_data().get::<SnapPreviewState>() {
                    preview.show(target.zone.rect(target.area));
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

        // Tile to exactly the rectangle the preview showed, so the two never
        // disagree on odd work-area sizes.
        let rect = target.zone.rect(target.area);
        if window.decoration_state().header_bar.snap_restore.is_none() {
            window.decoration_state().header_bar.snap_restore =
                relative_geometry_of_output(&self.space, &target.output, window);
        }
        let animated = self.configure_snapped(window, rect);
        self.animate_window(window, animated, rect.loc);
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
        let is_ssd = window.is_ssd();
        let rect = Rectangle::new(loc, decorated_content_size(content, is_ssd));
        self.configure_snapped(window, rect);
        self.space.map_element(window.clone(), loc, true);
        self.animate_window(window, content, loc);
    }

    /// If `window` was tiled by a snap, restore its floating geometry and
    /// return the window origin the drag should start from, keeping the pointer
    /// over the same spot on the titlebar. Returns `None` for a floating
    /// window. Used by the client-initiated move grabs.
    pub fn take_snap_restore_for_drag(
        &mut self,
        window: &WindowElement,
        pointer_global: Point<f64, Logical>,
    ) -> Option<Point<i32, Logical>> {
        let rel = window.decoration_state().header_bar.snap_restore.take()?;
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
    /// animate the frame to.
    fn configure_snapped(
        &mut self,
        window: &WindowElement,
        rect: Rectangle<i32, Logical>,
    ) -> Size<i32, Logical> {
        let is_ssd = window.is_ssd();
        let content = undecorated_content_size(rect.size, is_ssd);
        match window.0.underlying_surface() {
            WindowSurface::Wayland(toplevel) => {
                toplevel.with_pending_state(|state| state.size = Some(content));
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
            if progress >= 1.0 {
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

/// Capture the pre-transition pixels of every window that is waiting for a
/// snapshot, so its animation can crossfade from it. Must be called with the
/// output framebuffer unbound, before rendering the frame.
pub fn capture_window_snapshots<R>(space: &Space<WindowElement>, renderer: &mut R)
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let windows: Vec<WindowElement> = space.elements().cloned().collect();
    for window in windows {
        if window.is_ghosting() {
            continue;
        }

        let pending = window
            .decoration_state()
            .animation
            .as_ref()
            .map(|animation| animation.needs_snapshot())
            .unwrap_or(false);
        if pending {
            let content = window.resize_content_size();
            if content.w > 0 && content.h > 0 {
                let size = Size::<i32, Buffer>::from((content.w, content.h));
                if let Some(buffer) = capture_window_content(renderer, &window, size) {
                    window
                        .decoration_state()
                        .animation
                        .as_mut()
                        .unwrap()
                        .set_snapshot(buffer, content);
                } else {
                    window
                        .decoration_state()
                        .animation
                        .as_mut()
                        .unwrap()
                        .set_snapshot_unavailable();
                }
            } else {
                window
                    .decoration_state()
                    .animation
                    .as_mut()
                    .unwrap()
                    .set_snapshot_unavailable();
            }
        }
    }
}

/// Walk a window's surface tree and keep every mapped surface's buffer, with its
/// position relative to the window's top-left. Holding the buffers keeps the
/// pixels alive after the client is gone, so an app-triggered close can render
/// the whole window (content and subsurfaces).
fn capture_surface_tree(root: &WlSurface) -> Option<LastFrame> {
    let mut surfaces: Vec<SurfaceFrame> = Vec::new();

    with_surface_tree_downward(
        root,
        Point::<f64, Logical>::from((0.0, 0.0)),
        |_, states, location| {
            let mut location = *location;
            if let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() {
                if let Some(view) = data.lock().unwrap().view() {
                    location += view.offset.to_f64();
                    TraversalAction::DoChildren(location)
                } else {
                    TraversalAction::SkipChildren
                }
            } else {
                TraversalAction::SkipChildren
            }
        },
        |_, states, location| {
            let mut location = *location;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return;
            };
            let data = data.lock().unwrap();
            let Some(view) = data.view() else {
                return;
            };
            location += view.offset.to_f64();
            if let Some(buffer) = data.buffer() {
                surfaces.push(SurfaceFrame {
                    buffer: buffer.clone(),
                    location: Point::from((
                        location.x.round() as i32,
                        location.y.round() as i32,
                    )),
                    size: view.dst,
                    scale: data.buffer_scale(),
                    transform: data.buffer_transform(),
                    src: Some(view.src),
                });
            }
        },
        |_, _, _| true,
    );

    (!surfaces.is_empty()).then_some(LastFrame {
        geometry: with_states(root, |states| {
            states.cached_state.get::<SurfaceCachedState>().current().geometry
        })
        .unwrap_or_else(|| {
            Rectangle::from_size(surfaces.first().map(|s| s.size).unwrap_or_default())
        }),
        surfaces,
    })
}

/// Render a window's client content into an offscreen buffer and read it back
/// as a [`MemoryRenderBuffer`]. Returns `None` if the renderer cannot provide
/// a readable offscreen target.
fn capture_window_content<R>(
    renderer: &mut R,
    window: &WindowElement,
    size: Size<i32, Buffer>,
) -> Option<MemoryRenderBuffer>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let mut target = renderer.create_buffer(Fourcc::Abgr8888, size).ok()?;
    let mut framebuffer = renderer.bind(&mut target).ok()?;

    let elements: Vec<WaylandSurfaceRenderElement<R>> = AsRenderElements::render_elements(
        &window.0,
        renderer,
        Point::from((0, 0)),
        Scale::from(1.0),
        1.0,
    );

    let physical_size = Size::<i32, Physical>::from((size.w, size.h));
    let mut tracker = OutputDamageTracker::new(physical_size, 1.0, Transform::Normal);
    tracker
        .render_output(renderer, &mut framebuffer, 0, &elements, [0.0, 0.0, 0.0, 0.0])
        .ok()?;

    let region = Rectangle::<i32, Buffer>::from_size(size);
    let mapping = renderer
        .copy_framebuffer(&framebuffer, region, Fourcc::Abgr8888)
        .ok()?;
    let data = renderer.map_texture(&mapping).ok()?;
    // Smithay's GL renderer flips y while drawing into a `Normal` target, so the
    // readback already comes out top-down: no extra transform is needed.
    Some(MemoryRenderBuffer::from_slice(
        data,
        Fourcc::Abgr8888,
        size,
        1,
        Transform::Normal,
        None,
    ))
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
    // place the window at a random location on same output as pointer
    // or if there is not output in a [0;800]x[0;800] square
    use rand::distributions::{Distribution, Uniform};

    let output = space
        .output_under(pointer_location)
        .next()
        .or_else(|| space.outputs().next())
        .cloned();
    let output_geometry = output
        .and_then(|o| {
            let geo = space.output_geometry(&o)?;
            let map = layer_map_for_output(&o);
            let zone = map.non_exclusive_zone();
            Some(Rectangle::new(geo.loc + zone.loc, zone.size))
        })
        .unwrap_or_else(|| Rectangle::from_size((800, 800).into()));

    // set the initial toplevel bounds
    #[allow(irrefutable_let_patterns)]
    if let Some(toplevel) = window.0.toplevel() {
        toplevel.with_pending_state(|state| {
            state.bounds = Some(output_geometry.size);
        });
    }

    // Guard against tiny outputs where the random range would be empty (panics in `Uniform::new`).
    let max_x = output_geometry.loc.x + (((output_geometry.size.w as f32) / 3.0) * 2.0) as i32;
    let max_y = output_geometry.loc.y + (((output_geometry.size.h as f32) / 3.0) * 2.0) as i32;
    let mut rng = rand::thread_rng();
    let x = if max_x > output_geometry.loc.x {
        Uniform::new(output_geometry.loc.x, max_x).sample(&mut rng)
    } else {
        output_geometry.loc.x
    };
    let y = if max_y > output_geometry.loc.y {
        Uniform::new(output_geometry.loc.y, max_y).sample(&mut rng)
    } else {
        output_geometry.loc.y
    };

    space.map_element(window.clone(), (x, y), activate);
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
    let size = window.geometry().size;
    Some(RelativeGeometry {
        x: (loc.x - area.loc.x) as f64 / area.size.w as f64,
        y: (loc.y - area.loc.y) as f64 / area.size.h as f64,
        w: size.w as f64 / area.size.w as f64,
        h: size.h as f64 / area.size.h as f64,
    })
}

/// Compute a window's geometry as fractions of its output's work area.
pub fn relative_geometry_of(
    space: &Space<WindowElement>,
    window: &WindowElement,
) -> Option<RelativeGeometry> {
    let output = output_for_window(space, window)?;
    relative_geometry_of_output(space, &output, window)
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
    let loc = Point::from((
        area.loc.x + (rel.x * area.size.w as f64).round() as i32,
        area.loc.y + (rel.y * area.size.h as f64).round() as i32,
    ));
    let decoration: Size<i32, Logical> = if is_ssd {
        Size::from((2 * BORDER_WIDTH, HEADER_BAR_HEIGHT + BORDER_WIDTH))
    } else {
        Size::from((0, 0))
    };
    let size = Size::from((
        ((rel.w * area.size.w as f64).round() as i32 - decoration.w).max(1),
        ((rel.h * area.size.h as f64).round() as i32 - decoration.h).max(1),
    ));
    Some((loc, size))
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
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

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
                let size = self::xdg::fullscreen_content_size(work_area.size, window.is_ssd());
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.size = Some(size);
                });
                toplevel.send_configure();
                space.map_element(window, work_area.loc, false);
                continue;
            }
            if window.is_maximized() {
                let size = self::xdg::maximize_content_size(work_area.size, window.is_ssd());
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Maximized);
                    state.size = Some(size);
                });
                toplevel.send_configure();
                space.map_element(window, work_area.loc, false);
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
            space.map_element(window, loc, false);
        }
    }
}
