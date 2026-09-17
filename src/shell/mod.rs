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
            utils::on_commit_buffer_handler,
        },
    },
    desktop::{
        LayerSurface, PopupKind, PopupManager, Space, WindowSurfaceType, layer_map_for_output,
        space::SpaceElement,
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
            with_surface_tree_upward,
        },
        dmabuf::get_dmabuf,
        shell::{
            wlr_layer::{
                Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::XdgToplevelSurfaceData,
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
pub(crate) mod ssd;
#[cfg(feature = "xwayland")]
mod x11;
mod xdg;

pub use self::animation::*;
pub use self::element::*;
pub use self::grabs::*;

use self::ssd::{BORDER_WIDTH, CLOSE_TIMEOUT, HEADER_BAR_HEIGHT, RelativeGeometry, VisibilityState};

use self::xdg::handle_toplevel_commit;

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

                if &root == surface {
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
            .find(|window| window.wl_surface().map(|s| &*s == surface).unwrap_or(false))
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

    /// Advance every in-flight window animation. Called once per frame before
    /// rendering: updates where the window is mapped and retires animations
    /// whose client has caught up with the configured size.
    pub fn tick_animations(&mut self) {
        let now = Instant::now();
        let windows: Vec<WindowElement> = self.space.elements().cloned().collect();
        for window in &windows {
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
            if self.space.element_location(window) != Some(loc) {
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

        self.tick_visibility_animations(&windows, now);
    }

    /// Advance every window's open/close transition: start opens that were
    /// waiting for their first buffer, retire finished opens, and deliver the
    /// close request once a close transition has played out.
    fn tick_visibility_animations(&mut self, windows: &[WindowElement], now: Instant) {
        let mut to_close = Vec::new();
        for window in windows {
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
        let pending = window
            .decoration_state()
            .animation
            .as_ref()
            .map(|animation| animation.needs_snapshot())
            .unwrap_or(false);
        if !pending {
            continue;
        }

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
                continue;
            }
        }
        window
            .decoration_state()
            .animation
            .as_mut()
            .unwrap()
            .set_snapshot_unavailable();
    }
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

/// The area of an output that floating windows are laid out within, i.e. its
/// geometry minus any layer-shell exclusive zones (panels, docks, ...).
fn output_work_area(space: &Space<WindowElement>, output: &Output) -> Option<Rectangle<i32, Logical>> {
    let geo = space.output_geometry(output)?;
    let zone = layer_map_for_output(output).non_exclusive_zone();
    let area = Rectangle::new(geo.loc + zone.loc, zone.size);
    (area.size.w > 0 && area.size.h > 0).then_some(area)
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

    let windows: Vec<WindowElement> = space.elements_for_output(output).cloned().collect();
    for window in windows {
        // An output mode change re-lays out every window, so any in-flight
        // transition is superseded by the new geometry.
        window.decoration_state().animation = None;
        if let Some(toplevel) = window.0.toplevel() {
            let fullscreen = window.decoration_state().header_bar.fullscreen;
            if fullscreen {
                let size = self::xdg::fullscreen_content_size(output_geo.size, window.is_ssd());
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.size = Some(size);
                });
                toplevel.send_configure();
                space.map_element(window, output_geo.loc, false);
                continue;
            }
            if window.is_maximized() {
                let size = self::xdg::maximize_content_size(output_geo.size, window.is_ssd());
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Maximized);
                    state.size = Some(size);
                });
                toplevel.send_configure();
                space.map_element(window, output_geo.loc, false);
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
