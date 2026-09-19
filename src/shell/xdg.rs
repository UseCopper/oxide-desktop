use std::cell::RefCell;

use smithay::{
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Space, Window,
        WindowSurface, WindowSurfaceType, find_popup_root_surface, get_popup_toplevel_coords,
        layer_map_for_output, space::SpaceElement,
    },
    input::{Seat, pointer::Focus},
    output::Output,
    reexports::{
        wayland_protocols::xdg::{decoration as xdg_decoration, shell::server::xdg_toplevel},
        wayland_server::{
            Resource,
            protocol::{wl_output, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER as SCOUNTER, Serial, Size},
    wayland::{
        compositor::with_states,
        seat::WaylandFocus,
        shell::xdg::{
            Configure, PopupSurface, PositionerState, ToplevelCachedState, ToplevelSurface, XdgShellHandler,
            XdgShellState,
        },
    },
};
use tracing::{trace, warn};

use crate::{
    focus::KeyboardFocusTarget,
    shell::{TouchMoveSurfaceGrab, TouchResizeSurfaceGrab},
    state::{AnvilState, Backend},
};

use super::{
    FullscreenSurface, PointerMoveSurfaceGrab, PointerResizeSurfaceGrab, ResizeData, ResizeEdge,
    ResizeGrabState, ResizeState, SurfaceData, WindowElement, advance_resize_configure,
    place_new_window,
};
use super::ssd::{BORDER_WIDTH, HEADER_BAR_HEIGHT};

/// Size a toplevel should use while fullscreen.
///
/// Only server-decorated windows reserve room for the compositor's header bar.
/// Client-decorated windows have to fill the whole output, otherwise the
/// undecorated strip they leave behind is never painted (black).
pub(crate) fn fullscreen_content_size(output: Size<i32, Logical>, is_ssd: bool) -> Size<i32, Logical> {
    if is_ssd {
        Size::from((output.w, (output.h - HEADER_BAR_HEIGHT).max(0)))
    } else {
        output
    }
}

/// Size a toplevel should use while maximized, leaving room for the SSD frame
/// (borders + header bar) so the decorated window fits inside the output
/// instead of hanging off the right/bottom edge.
pub(crate) fn maximize_content_size(output: Size<i32, Logical>, is_ssd: bool) -> Size<i32, Logical> {
    if is_ssd {
        Size::from((
            (output.w - 2 * BORDER_WIDTH).max(0),
            (output.h - HEADER_BAR_HEIGHT - BORDER_WIDTH).max(0),
        ))
    } else {
        output
    }
}

/// Grow an undecorated content size into the full decorated size the SSD frame
/// occupies. Used when configuring X11 clients, whose `configure` takes the
/// frame rectangle rather than the content size.
pub(crate) fn decorated_content_size(
    content: Size<i32, Logical>,
    is_ssd: bool,
) -> Size<i32, Logical> {
    if is_ssd {
        Size::from((
            content.w + 2 * BORDER_WIDTH,
            content.h + HEADER_BAR_HEIGHT + BORDER_WIDTH,
        ))
    } else {
        content
    }
}

/// Shrink a decorated rectangle's size back to the client's undecorated content
/// size. Inverse of [`decorated_content_size`].
pub(crate) fn undecorated_content_size(
    decorated: Size<i32, Logical>,
    is_ssd: bool,
) -> Size<i32, Logical> {
    if is_ssd {
        Size::from((
            (decorated.w - 2 * BORDER_WIDTH).max(1),
            (decorated.h - HEADER_BAR_HEIGHT - BORDER_WIDTH).max(1),
        ))
    } else {
        decorated
    }
}

/// Where to anchor a window being dragged out of the maximized state so the
/// pointer keeps grabbing the same spot on the titlebar: the same horizontal
/// fraction of the width, and the same vertical offset from the top.
pub(crate) fn restore_drag_location(
    window_loc: Point<i32, Logical>,
    decorated_size: Size<i32, Logical>,
    grab: Point<f64, Logical>,
    restore: Option<Size<i32, Logical>>,
    is_ssd: bool,
) -> Point<i32, Logical> {
    let rel_x = grab.x - window_loc.x as f64;
    let rel_y = grab.y - window_loc.y as f64;
    let border = if is_ssd { 2 * BORDER_WIDTH } else { 0 };
    let old_w = decorated_size.w as f64;
    let new_w = restore
        .map(|size| (size.w + border) as f64)
        .unwrap_or(old_w);
    let fraction = if old_w > 0.0 { rel_x / old_w } else { 0.5 };
    Point::from((
        (grab.x - fraction * new_w).round() as i32,
        (grab.y - rel_y).round() as i32,
    ))
}

impl<BackendData: Backend> XdgShellHandler for AnvilState<BackendData> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Do not send a configure here, the initial configure
        // of a xdg_surface has to be sent during the commit if
        // the surface is not already configured
        let window = WindowElement(Window::new_wayland_window(surface.clone()));
        window.begin_open();
        place_new_window(&mut self.space, self.pointer.current_location(), &window, true);
        self.focus_new_windows();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // The client is gone (or closing itself); play its close transition from
        // the cached frame.
        let window = self
            .space
            .elements()
            .find(|window| window.0.toplevel().is_some_and(|t| t == &surface))
            .cloned();
        if let Some(window) = window {
            self.begin_window_ghost(&window);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // Do not send a configure here, the initial configure
        // of a xdg_surface has to be sent during the commit if
        // the surface is not already configured

        self.unconstrain_popup(&surface);

        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            warn!("Failed to track popup: {}", err);
        }
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let seat: Seat<AnvilState<BackendData>> = Seat::from_resource(&seat).unwrap();
        self.move_request_xdg(&surface, &seat, serial)
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let seat: Seat<AnvilState<BackendData>> = Seat::from_resource(&seat).unwrap();

        if let Some(touch) = seat.get_touch() {
            if touch.has_grab(serial) {
                let start_data = touch.grab_start_data().unwrap();

                // If the client disconnects after requesting a resize
                // we can just ignore the request
                let Some(window) = self.window_for_surface(surface.wl_surface()) else {
                    tracing::debug!("resize request ignored: no window");
                    return;
                };

                // If the focus was for a different surface, ignore the request.
                if start_data.focus.is_none()
                    || !start_data
                        .focus
                        .as_ref()
                        .unwrap()
                        .0
                        .same_client_as(&surface.wl_surface().id())
                {
                    tracing::debug!("resize request ignored: different surface");
                    return;
                }
                let geometry = SpaceElement::geometry(&window.0);
                let loc = self.space.element_location(&window).unwrap();
                let (initial_window_location, initial_window_size) = (loc, geometry.size);
                let snap_area = self.snap_area_for(&window);

                with_states(surface.wl_surface(), move |states| {
                    states
                        .data_map
                        .get::<RefCell<SurfaceData>>()
                        .unwrap()
                        .borrow_mut()
                        .resize_state = ResizeState::Resizing(ResizeData::new(
                            edges.into(),
                            initial_window_location,
                            initial_window_size,
                        ));
                });

                let start_location = start_data.location;
                let mut resize = ResizeGrabState::new(
                    window,
                    edges.into(),
                    initial_window_location,
                    initial_window_size,
                    start_location,
                );
                resize.set_snap_area(snap_area);
                let grab = TouchResizeSurfaceGrab { start_data, resize };

                touch.set_grab(self, grab, serial);
                return;
            }
        }

        let Some(pointer) = seat.get_pointer() else {
            return;
        };

        // Check that this surface has a click grab.
        if !pointer.has_grab(serial) {
            return;
        }

        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };

        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };

        // If the focus was for a different surface, ignore the request.
        if start_data.focus.is_none()
            || !start_data
                .focus
                .as_ref()
                .unwrap()
                .0
                .same_client_as(&surface.wl_surface().id())
        {
            return;
        }

        let geometry = SpaceElement::geometry(&window.0);
        let Some(loc) = self.space.element_location(&window) else {
            return;
        };
        let (initial_window_location, initial_window_size) = (loc, geometry.size);
        let snap_area = self.snap_area_for(&window);

        with_states(surface.wl_surface(), |states| {
            if let Some(data) = states.data_map.get::<RefCell<SurfaceData>>() {
                data.borrow_mut().resize_state = ResizeState::Resizing(ResizeData::new(
                    edges.into(),
                    initial_window_location,
                    initial_window_size,
                ));
            }
        });

        let start_location = start_data.location;
        let mut resize = ResizeGrabState::new(
            window,
            edges.into(),
            initial_window_location,
            initial_window_size,
            start_location,
        );
        resize.set_snap_area(snap_area);
        let grab = PointerResizeSurfaceGrab { start_data, resize };

        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn ack_configure(&mut self, surface: WlSurface, configure: Configure) {
        if let Configure::Toplevel(configure) = configure {
            if let Some(serial) = with_states(&surface, |states| {
                if let Some(data) = states.data_map.get::<RefCell<SurfaceData>>() {
                    if let ResizeState::WaitingForFinalAck(_, serial) = data.borrow().resize_state {
                        return Some(serial);
                    }
                }

                None
            }) {
                // When the resize grab is released the surface
                // resize state will be set to WaitingForFinalAck
                // and the client will receive a configure request
                // without the resize state to inform the client
                // resizing has finished. Here we will wait for
                // the client to acknowledge the end of the
                // resizing. To check if the surface was resizing
                // before sending the configure we need to use
                // the current state as the received acknowledge
                // will no longer have the resize state set
                let is_resizing = with_states(&surface, |states| {
                    states
                        .cached_state
                        .get::<ToplevelCachedState>()
                        .current()
                        .last_acked
                        .as_ref()
                        .is_some_and(|c| c.state.states.contains(xdg_toplevel::State::Resizing))
                });

                if configure.serial >= serial && is_resizing {
                    with_states(&surface, |states| {
                        let mut data = states
                            .data_map
                            .get::<RefCell<SurfaceData>>()
                            .unwrap()
                            .borrow_mut();
                        if let ResizeState::WaitingForFinalAck(resize_data, _) = data.resize_state {
                            data.resize_state = ResizeState::WaitingForCommit(resize_data);
                        } else {
                            unreachable!()
                        }
                    });
                }
            }

            let window = self
                .space
                .elements()
                .find(|element| element.wl_surface().as_deref() == Some(&surface));
            if let Some(window) = window {
                use xdg_decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
                let is_ssd = configure
                    .state
                    .decoration_mode
                    .map(|mode| mode == Mode::ServerSide)
                    .unwrap_or(false);
                window.set_ssd(is_ssd);

                // A fullscreen configure may have been sent before the client's
                // decoration preference was known (clients often request
                // fullscreen and server-side decorations in the same batch). Now
                // that the mode is acked, re-send the size so client-decorated
                // windows fill the whole output instead of reserving space for a
                // header bar that is never drawn.
                let is_fullscreen = window.decoration_state().header_bar.fullscreen;
                if is_fullscreen
                    && let Some(output) = self.space.outputs_for_element(window).first().cloned()
                    && let Some(geometry) = self.space.output_geometry(&output)
                    && let Some(toplevel) = window.0.toplevel()
                {
                    let area = super::output_work_area(&self.space, &output).unwrap_or(geometry);
                    let desired = fullscreen_content_size(area.size, is_ssd);
                    if toplevel.with_pending_state(|state| state.size != Some(desired)) {
                        toplevel.with_pending_state(|state| state.size = Some(desired));
                        toplevel.send_configure();
                    }
                }
            }
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, wl_output: Option<wl_output::WlOutput>) {
        self.fullscreen_request_xdg(&surface, wl_output);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.unfullscreen_request_xdg(&surface);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.maximize_request_xdg(&surface);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.unmaximize_request_xdg(&surface);
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };
        self.minimize_request(window);
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let seat: Seat<AnvilState<BackendData>> = Seat::from_resource(&seat).unwrap();
        let kind = PopupKind::Xdg(surface);
        if let Some(root) = find_popup_root_surface(&kind).ok().and_then(|root| {
            self.space
                .elements()
                .find(|w| w.wl_surface().map(|s| *s == root).unwrap_or(false))
                .cloned()
                .map(KeyboardFocusTarget::from)
                .or_else(|| {
                    self.space
                        .outputs()
                        .find_map(|o| {
                            let map = layer_map_for_output(o);
                            map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL).cloned()
                        })
                        .map(KeyboardFocusTarget::LayerSurface)
                })
        }) {
            let ret = self.popups.grab_popup(root, kind, &seat, serial);

            if let Ok(mut grab) = ret {
                if let Some(keyboard) = seat.get_keyboard() {
                    if keyboard.is_grabbed()
                        && !(keyboard.has_grab(serial)
                            || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
                    {
                        grab.ungrab(PopupUngrabStrategy::All);
                        return;
                    }
                    keyboard.set_focus(self, grab.current_grab(), serial);
                    keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
                }
                if let Some(pointer) = seat.get_pointer() {
                    if pointer.is_grabbed()
                        && !(pointer.has_grab(serial)
                            || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
                    {
                        grab.ungrab(PopupUngrabStrategy::All);
                        return;
                    }
                    pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
                }
            }
        }
    }
}

impl<BackendData: Backend> AnvilState<BackendData> {
    pub fn minimize_request(&mut self, window: WindowElement) {
        let location = match self.space.element_location(&window) {
            Some(loc) => loc,
            None => return,
        };
        if self.minimized.iter().any(|(w, _)| w == &window) {
            return;
        }
        self.minimized.push((window.clone(), location));
        self.space.unmap_elem(&window);
        // While minimized the window isn't rendered, so don't keep holding the
        // client's buffers (and let it recycle them).
        window.decoration_state().last_frame = None;
        if let Some(keyboard) = self.seat.get_keyboard() {
            if matches!(
                keyboard.current_focus(),
                Some(KeyboardFocusTarget::Window(w)) if w == window.0
            ) {
                keyboard.set_focus(self, None, SCOUNTER.next_serial());
            }
        }
    }

    pub fn unminimize_request(&mut self, window: WindowElement) {
        let Some(index) = self.minimized.iter().position(|(w, _)| w == &window) else {
            return;
        };
        let (_, location) = self.minimized.remove(index);
        self.space.map_element(window, location, true);
    }

    pub fn restore_last_minimized(&mut self) {
        let Some((window, location)) = self.minimized.pop() else {
            return;
        };
        self.space.map_element(window, location, true);
    }

    // Shared window-management actions. Both the SSD title-bar buttons and the
    // XDG / X11 protocol request handlers go through these, so a request and a
    // button click always have the exact same effect.

    pub fn close_window(&mut self, window: WindowElement) {
        if window.is_closing() {
            return;
        }
        // Play the close transition first; the client is told to close once it
        // has finished (see `tick_animations`).
        window.begin_close();
        // The window is no longer interactive, so move focus off it.
        self.clear_window_focus(&window);
    }

    /// Deliver a close request to the client without any transition.
    pub fn send_close(&mut self, window: &WindowElement) {
        match window.0.underlying_surface() {
            WindowSurface::Wayland(w) => w.send_close(),
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(w) => {
                let _ = w.close();
            }
        }
    }

    pub fn maximize_window(&mut self, window: WindowElement) {
        match window.0.underlying_surface() {
            WindowSurface::Wayland(w) => self.maximize_request_xdg(w),
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(w) => self.maximize_request_x11(w),
        }
    }

    pub fn unmaximize_window(&mut self, window: WindowElement) {
        match window.0.underlying_surface() {
            WindowSurface::Wayland(w) => self.unmaximize_request_xdg(w),
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(w) => self.unmaximize_request_x11(w),
        }
    }

    /// The titlebar maximize button's behaviour: toggle the window between
    /// maximized and floating. A snapped (tiled) window un-snaps, and a client
    /// that put itself fullscreen exits that instead, so the user is never
    /// stuck. Shared by the button and the top-edge snap zone.
    pub fn toggle_maximize(&mut self, window: WindowElement) {
        let fullscreen = window.decoration_state().header_bar.fullscreen;
        let tiled = window.decoration_state().header_bar.snap_restore.is_some();
        if fullscreen {
            self.unfullscreen_window(window);
        } else if window.is_maximized() {
            self.unmaximize_window(window);
        } else if tiled {
            self.unsnap_window(window);
        } else {
            self.maximize_window(window);
        }
    }

    /// Restore a snapped window to its floating geometry with the same
    /// transition as an unmaximize.
    pub fn unsnap_window(&mut self, window: WindowElement) {
        let Some(rel) = window.decoration_state().header_bar.snap_restore.take() else {
            return;
        };
        let is_ssd = window.is_ssd();
        let Some((loc, content)) = super::absolute_geometry(&self.space, &window, rel) else {
            return;
        };
        // Raise the window above the rest of the snap group without moving it,
        // so the transition is visible and its position can animate to `loc`
        // rather than jumping there.
        if let Some(current) = self.space.element_location(&window) {
            self.space.map_element(window.clone(), current, true);
        }
        let rect = Rectangle::new(loc, decorated_content_size(content, is_ssd));
        self.configure_snapped(&window, rect, false);
        self.animate_window(&window, content, loc);
    }

    pub fn fullscreen_window(&mut self, window: WindowElement) {
        match window.0.underlying_surface() {
            WindowSurface::Wayland(w) => self.fullscreen_request_xdg(w, None),
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(w) => self.fullscreen_request_x11(w),
        }
    }

    pub fn unfullscreen_window(&mut self, window: WindowElement) {
        match window.0.underlying_surface() {
            WindowSurface::Wayland(w) => self.unfullscreen_request_xdg(w),
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(w) => self.unfullscreen_request_x11(w),
        }
    }

    fn maximize_request_xdg(&mut self, surface: &ToplevelSurface) {
        // NOTE: This should use layer-shell when it is implemented to
        // get the correct maximum size
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };
        // Maximizing supersedes any snap; the floating geometry would be stale.
        window.decoration_state().header_bar.snap_restore = None;
        let outputs_for_window = self.space.outputs_for_element(&window);
        let output = outputs_for_window
            .first()
            // The window hasn't been mapped yet, use the primary output instead
            .or_else(|| self.space.outputs().next());

        // Remember where the window was so unmaximizing can put it back, stored
        // as a fraction of the work area so it survives resolution changes.
        let already_maximized =
            surface.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Maximized));
        if !already_maximized {
            window.decoration_state().maximize_restore = super::relative_geometry_of(&self.space, &window);
        }

        let target = output.and_then(|output| super::output_work_area(&self.space, output));
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
            state.size = target.map(|geo| maximize_content_size(geo.size, window.is_ssd()));
        });
        if let Some(geometry) = target {
            // Animate from the floating geometry to the maximized one instead of
            // snapping, even though the client is configured immediately.
            let start_loc = self.space.element_location(&window).unwrap_or(geometry.loc);
            self.animate_window(
                &window,
                maximize_content_size(geometry.size, window.is_ssd()),
                geometry.loc,
            );
            // Raise/focus the window without moving it off its start position.
            self.space.map_element(window, start_loc, true);
        }

        // The protocol demands us to always reply with a configure,
        // regardless of we fulfilled the request or not
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        } else {
            // Will be sent during initial configure
        }
    }

    fn unmaximize_request_xdg(&mut self, surface: &ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };
        let restore = window.decoration_state().maximize_restore.take();
        let geometry = restore.and_then(|rel| super::absolute_geometry(&self.space, &window, rel));

        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
            state.size = geometry.map(|(_, size)| size);
        });
        if let Some((location, size)) = geometry {
            self.animate_window(&window, size, location);
        }

        // The protocol demands us to always reply with a configure,
        // regardless of we fulfilled the request or not
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        } else {
            // Will be sent during initial configure
        }
    }

    fn fullscreen_request_xdg(&mut self, surface: &ToplevelSurface, mut wl_output: Option<wl_output::WlOutput>) {
        // NOTE: This is only one part of the solution. We can set the
        // location and configure size here, but the surface should be rendered fullscreen
        // independently from its buffer size
        let wl_surface = surface.wl_surface();

        let Some(window) = self.window_for_surface(wl_surface) else {
            return;
        };
        // Fullscreen supersedes any snap; dragging a fullscreen window must not
        // restore a stale floating geometry.
        window.decoration_state().header_bar.snap_restore = None;
        // A specific output may be requested; otherwise use the output the window
        // is actually on, not `Space`'s arbitrary first output.
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| super::output_for_window(&self.space, &window))
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let Ok(client) = self.display_handle.get_client(wl_surface.id()) else {
            return;
        };
        for client_output in output.client_outputs(&client) {
            wl_output = Some(client_output);
        }

        // Remember where the window was, relative to the output it is being
        // fullscreened on, so unfullscreening can put it back on the same
        // monitor. Skip storing a zero-sized restore (the window may not have
        // committed a buffer yet — Firefox, for instance, starts fullscreen).
        let restore = super::relative_geometry_of_output(&self.space, &output, &window)
            .filter(|rel| rel.w > 0.0 && rel.h > 0.0);
        let mut state = window.decoration_state();
        state.header_bar.fullscreen = true;
        state.header_bar.pointer_loc = None;
        if state.fullscreen_restore.is_none() {
            state.fullscreen_restore = restore.map(|rel| (output.downgrade(), rel));
        }
        drop(state);

        // A fullscreen window covers the whole output, including any
        // layer-shell exclusive zone: the panel is hidden while fullscreen.
        let is_ssd = window.is_ssd();
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.size = Some(fullscreen_content_size(output_geo.size, is_ssd));
            state.fullscreen_output = wl_output;
        });
        output.user_data().insert_if_missing(FullscreenSurface::default);
        if let Some(fs) = output.user_data().get::<FullscreenSurface>() {
            // Don't leak a previous fullscreen window if a second one takes over.
            if let Some(prev) = fs.get()
                && prev.wl_surface().as_deref() != Some(wl_surface)
            {
                tracing::debug!("Replacing previous fullscreen window");
            }
            fs.set(window.clone());
        }
        trace!("Fullscreening: {:?}", window);

        // The protocol demands us to always reply with a configure,
        // regardless of we fulfilled the request or not
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        } else {
            // Will be sent during initial configure
        }
    }

    fn unfullscreen_request_xdg(&mut self, surface: &ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        let restore = self
            .space
            .elements()
            .find(|window| window.wl_surface().map(|s| &*s == wl_surface).unwrap_or(false))
            .and_then(|window| {
                let mut state = window.decoration_state();
                state.header_bar.fullscreen = false;
                state.header_bar.pointer_loc = None;
                state
                    .fullscreen_restore
                    .take()
                    .map(|(output, rel)| (window.clone(), output, rel))
            });

        let geometry = restore.as_ref().and_then(|(window, output, rel)| {
            // The output the window was fullscreened on may have been removed.
            let output = output
                .upgrade()
                .filter(|output| self.space.outputs().any(|o| o == output))
                .or_else(|| super::output_for_window(&self.space, window))?;
            super::absolute_geometry_for_output(&self.space, &output, *rel, window.is_ssd())
        });
        // If there's no stored restore (window spawned fullscreen), fall back
        // to the current output's work area so the client gets a real size
        // instead of 0×0.
        let geometry = geometry.or_else(|| {
            let window = self
                .space
                .elements()
                .find(|w| w.wl_surface().map(|s| &*s == wl_surface).unwrap_or(false))
                .cloned()?;
            let output = super::output_for_window(&self.space, &window)?;
            let area = super::output_work_area(&self.space, &output)?;
            let is_ssd = window.is_ssd();
            let size = fullscreen_content_size(area.size, is_ssd);
            Some((area.loc, size))
        });
        let ret = surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Fullscreen);
            state.size = geometry.map(|(_, size)| size);
            state.fullscreen_output.take()
        });
        if let Some((location, _)) = geometry
            && let Some((window, _, _)) = restore
        {
            self.space.map_element(window, location, false);
        }

        // Clear the output's fullscreen surface so rendering falls back to the
        // normal space layout. This must happen even when the client did not
        // name an output, otherwise the fullscreen render path keeps drawing
        // the window (and hides the panel) with a stale size.
        let client_output = ret.and_then(|output| Output::from_resource(&output));
        let target = client_output
            .clone()
            .or_else(|| {
                self.space.elements()
                    .find(|w| w.wl_surface().map(|s| &*s == wl_surface).unwrap_or(false))
                    .and_then(|window| super::output_for_window(&self.space, window))
            });
        if let Some(output) = target {
            if let Some(fullscreen) = output.user_data().get::<FullscreenSurface>() {
                trace!("Unfullscreening: {:?}", fullscreen.get());
                fullscreen.clear();
                self.backend_data.reset_buffers(&output);
            }
        }

        // The protocol demands us to always reply with a configure,
        // regardless of we fulfilled the request or not
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        } else {
            // Will be sent during initial configure
        }
    }

    pub fn move_request_xdg(&mut self, surface: &ToplevelSurface, seat: &Seat<Self>, serial: Serial) {
        if let Some(touch) = seat.get_touch() {
            if touch.has_grab(serial) {
                let start_data = touch.grab_start_data().unwrap();

                // If the client disconnects after requesting a move
                // we can just ignore the request
                let Some(window) = self.window_for_surface(surface.wl_surface()) else {
                    return;
                };

                // If the focus was for a different surface, ignore the request.
                if start_data.focus.is_none()
                    || !start_data
                        .focus
                        .as_ref()
                        .unwrap()
                        .0
                        .same_client_as(&surface.wl_surface().id())
                {
                    return;
                }

        let Some(mut initial_window_location) = self.space.element_location(&window) else {
            return;
        };

        // If the surface is maximized, unmaximize it and keep the touch over
        // the same spot on the titlebar while the window restores.
        let mut restore_size = None;
        if surface.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Maximized)) {
            let decorated_size = self
                .space
                .element_geometry(&window)
                .map(|geo| geo.size)
                .unwrap_or_default();
            let restore = window.decoration_state().maximize_restore.take();
            restore_size = restore
                .and_then(|rel| super::absolute_geometry(&self.space, &window, rel))
                .map(|(_, size)| size);
            initial_window_location = restore_drag_location(
                initial_window_location,
                decorated_size,
                start_data.location,
                restore_size,
                window.is_ssd(),
            );
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Maximized);
                state.size = restore_size;
            });
            surface.send_configure();
        }

        // The grab drives the position, so restore/unmaximize transitions only
        // animate the window's size while it follows the finger.
        self.dragging_window = Some(window.clone());

        // A snapped window restores to its floating size as the drag starts.
        if let Some(restored) = self.take_snap_restore_for_drag(&window, start_data.location) {
            initial_window_location = restored;
        }

        if let Some(size) = restore_size {
            self.animate_window(&window, size, initial_window_location);
        }

        let grab = TouchMoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
            snap_target: None,
        };

        touch.set_grab(self, grab, serial);
        return;
            }
        }

        let Some(pointer) = seat.get_pointer() else {
            return;
        };

        // Check that this surface has a click grab.
        if !pointer.has_grab(serial) {
            return;
        }

        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };

        // If the client disconnects after requesting a move
        // we can just ignore the request
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };

        // If the focus was for a different surface, ignore the request.
        if start_data.focus.is_none()
            || !start_data
                .focus
                .as_ref()
                .unwrap()
                .0
                .same_client_as(&surface.wl_surface().id())
        {
            return;
        }

        let mut initial_window_location = self.space.element_location(&window).unwrap();

        // If the surface is maximized, unmaximize it and keep the pointer over
        // the same spot on the titlebar while the window restores.
        let mut restore_size = None;
        if surface.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Maximized)) {
            let decorated_size = self
                .space
                .element_geometry(&window)
                .map(|geo| geo.size)
                .unwrap_or_default();
            let restore = window.decoration_state().maximize_restore.take();
            restore_size = restore
                .and_then(|rel| super::absolute_geometry(&self.space, &window, rel))
                .map(|(_, size)| size);
            initial_window_location = restore_drag_location(
                initial_window_location,
                decorated_size,
                start_data.location,
                restore_size,
                window.is_ssd(),
            );
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Maximized);
                state.size = restore_size;
            });
            surface.send_configure();
        }

        // The grab drives the position, so restore/unmaximize transitions only
        // animate the window's size while it follows the pointer.
        self.dragging_window = Some(window.clone());

        // A snapped window restores to its floating size as the drag starts.
        if let Some(restored) = self.take_snap_restore_for_drag(&window, start_data.location) {
            initial_window_location = restored;
        }

        if let Some(size) = restore_size {
            self.animate_window(&window, size, initial_window_location);
        }

        let grab = PointerMoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
            snap_target: None,
        };

        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self.window_for_surface(&root) else {
            return;
        };

        let mut outputs_for_window = self.space.outputs_for_element(&window);
        if outputs_for_window.is_empty() {
            return;
        }

        // Get a union of all outputs' geometries.
        let mut outputs_geo = self
            .space
            .output_geometry(&outputs_for_window.pop().unwrap())
            .unwrap();
        for output in outputs_for_window {
            outputs_geo = outputs_geo.merge(self.space.output_geometry(&output).unwrap());
        }

        let window_geo = self.space.element_geometry(&window).unwrap();

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = outputs_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

/// Update a toplevel's committed geometry and drive the resize pipeline after a
/// buffer commit. Must run after `on_commit_buffer_handler`/`Window::on_commit`
/// so the resize snapshot matches the buffer that will be rendered.
pub(crate) fn handle_toplevel_commit(space: &mut Space<WindowElement>, surface: &WlSurface) -> Option<()> {
    let window = space
        .elements()
        .find(|w| w.wl_surface().as_deref() == Some(surface))
        .cloned()?;

    let mut window_loc = space.element_location(&window)?;
    // `resize_content_size` uses the surface bbox for SSD windows so the move
    // and resize land on the same commit; client-decorated windows keep using
    // their declared geometry.
    let geometry = window.committed_content_size();

    // Read the *committed* resize state (if any) and snapshot the committed
    // geometry atomically with the buffer. The displayed frame is always taken
    // from this snapshot, so a request can never advance what is drawn. This
    // covers the in-progress state and the final ack/commit states.
    let resize = with_states(window.wl_surface().as_deref()?, |states| {
        let data = states.data_map.get::<RefCell<SurfaceData>>()?;
        let mut data = data.borrow_mut();
        match &mut data.resize_state {
            ResizeState::Resizing(resize)
            | ResizeState::WaitingForFinalAck(resize, _)
            | ResizeState::WaitingForCommit(resize) => {
                resize.committed_size = geometry;
                Some(*resize)
            }
            ResizeState::NotResizing => None,
        }
    });

    let new_loc: Option<Point<Option<i32>, Logical>> = resize
        .filter(|resize| resize.edges.intersects(ResizeEdge::TOP_LEFT))
        .map(|resize| {
            let loc = resize.initial_window_location;
            let size = resize.initial_window_size;

            // The edge opposite the one being dragged stays fixed.
            let new_x = resize
                .edges
                .intersects(ResizeEdge::LEFT)
                .then_some(loc.x + (size.w - geometry.w));

            let new_y = resize
                .edges
                .intersects(ResizeEdge::TOP)
                .then_some(loc.y + (size.h - geometry.h));

            (new_x, new_y).into()
        });

    if let Some(new_loc) = new_loc {
        if let Some(new_x) = new_loc.x {
            window_loc.x = new_x;
        }
        if let Some(new_y) = new_loc.y {
            window_loc.y = new_y;
        }

        if new_loc.x.is_some() || new_loc.y.is_some() {
            // If TOP or LEFT side of the window got resized, we have to move it
            space.map_element(window.clone(), window_loc, false);
        }
    }

    // Advance the serialized resize pipeline immediately: if the latest intended
    // size differs from the size that was just committed, request it now instead
    // of waiting for another pointer frame. This runs for every resize edge.
    advance_resize_configure(&window, space);

    // The final commit has landed; leave the resize state so the displayed
    // geometry becomes the plain committed geometry.
    if let Some(surface) = window.wl_surface() {
        with_states(&surface, |states| {
            if let Some(data) = states.data_map.get::<RefCell<SurfaceData>>() {
                let mut data = data.borrow_mut();
                if matches!(data.resize_state, ResizeState::WaitingForCommit(_)) {
                    data.resize_state = ResizeState::NotResizing;
                }
            }
        });
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_drag_keeps_relative_grab() {
        let loc = Point::from((0, 0));
        let decorated = Size::from((1280, 800));
        let grab = Point::from((640.0, 15.0));
        let restore = Some(Size::from((640, 480)));
        // new decorated width = 640 + 4 = 644; middle -> 322
        // origin = (640 - 322, 15 - 15) = (318, 0)
        assert_eq!(
            restore_drag_location(loc, decorated, grab, restore, true),
            Point::from((318, 0))
        );

        // no restore size -> fraction of the current width is unchanged
        let out = restore_drag_location(loc, decorated, grab, None, true);
        assert_eq!(out, Point::from((0, 0)));
    }
}
