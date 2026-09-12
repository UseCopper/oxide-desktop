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
    utils::{Logical, Point, SERIAL_COUNTER as SCOUNTER, Serial, Size},
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
    fullscreen_output_geometry, place_new_window,
};
use super::ssd::HEADER_BAR_HEIGHT;

impl<BackendData: Backend> XdgShellHandler for AnvilState<BackendData> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Do not send a configure here, the initial configure
        // of a xdg_surface has to be sent during the commit if
        // the surface is not already configured
        let window = WindowElement(Window::new_wayland_window(surface.clone()));
        place_new_window(&mut self.space, self.pointer.current_location(), &window, true);
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
                let grab = TouchResizeSurfaceGrab {
                    start_data,
                    resize: ResizeGrabState::new(
                        window,
                        edges.into(),
                        initial_window_location,
                        initial_window_size,
                        start_location,
                    ),
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
        let grab = PointerResizeSurfaceGrab {
            start_data,
            resize: ResizeGrabState::new(
                window,
                edges.into(),
                initial_window_location,
                initial_window_size,
                start_location,
            ),
        };

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
        let outputs_for_window = self.space.outputs_for_element(&window);
        let output = outputs_for_window
            .first()
            // The window hasn't been mapped yet, use the primary output instead
            .or_else(|| self.space.outputs().next());

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
            state.size = output.and_then(|output| self.space.output_geometry(output)).map(|geo| geo.size);
        });
        if let Some(output) = output {
            if let Some(geometry) = self.space.output_geometry(output) {
                self.space.map_element(window, geometry.loc, true);
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

    fn unmaximize_request_xdg(&mut self, surface: &ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
            state.size = None;
        });

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

        let output_geometry = fullscreen_output_geometry(wl_surface, wl_output.as_ref(), &mut self.space);

        if let Some(geometry) = output_geometry {
            let output = wl_output
                .as_ref()
                .and_then(Output::from_resource)
                .or_else(|| self.space.outputs().next().cloned());
            let Some(output) = output else {
                return;
            };
            let Ok(client) = self.display_handle.get_client(wl_surface.id()) else {
                return;
            };
            for output in output.client_outputs(&client) {
                wl_output = Some(output);
            }
            let Some(window) = self.window_for_surface(wl_surface) else {
                return;
            };

            let content_size = SpaceElement::geometry(&window.0).size;
            let location = self.space.element_location(&window);
            let mut state = window.decoration_state();
            state.header_bar.fullscreen = true;
            state.header_bar.pointer_loc = None;
            if state.fullscreen_restore.is_none() {
                if let Some(location) = location {
                    state.fullscreen_restore = Some((location, content_size));
                }
            }
            drop(state);

            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.size = Some(Size::from((
                    geometry.size.w,
                    (geometry.size.h - HEADER_BAR_HEIGHT).max(0),
                )));
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
        }

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
        let restore: Option<(WindowElement, Point<i32, Logical>, Size<i32, Logical>)> = self
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
                    .map(|(loc, size)| (window.clone(), loc, size))
            });

        let ret = surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Fullscreen);
            state.size = restore.as_ref().map(|(_, _, size)| *size);
            state.fullscreen_output.take()
        });
        if let Some((window, location, _)) = restore {
            self.space.map_element(window, location, false);
        }
        if let Some(output) = ret.and_then(|output| Output::from_resource(&output)) {
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

                // If surface is maximized then unmaximize it
                let changed = surface.with_pending_state(|state| {
                    if state.states.unset(xdg_toplevel::State::Maximized) {
                        state.size = None;
                        true
                    } else {
                        false
                    }
                });
                if changed {
                    surface.send_configure();

                    // NOTE: In real compositor mouse location should be mapped to a new window size
                    // For example, you could:
                    // 1) transform mouse pointer position from compositor space to window space (location relative)
                    // 2) divide the x coordinate by width of the window to get the percentage
                    //   - 0.0 would be on the far left of the window
                    //   - 0.5 would be in middle of the window
                    //   - 1.0 would be on the far right of the window
                    // 3) multiply the percentage by new window width
                    // 4) by doing that, drag will look a lot more natural
                    //
                    // but for anvil needs setting location to pointer location is fine
                    initial_window_location = start_data.location.to_i32_round();
                }

                let grab = TouchMoveSurfaceGrab {
                    start_data,
                    window,
                    initial_window_location,
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

        // If surface is maximized then unmaximize it
        let changed = surface.with_pending_state(|state| {
            if state.states.unset(xdg_toplevel::State::Maximized) {
                state.size = None;
                true
            } else {
                false
            }
        });
        if changed {
            surface.send_configure();

            // NOTE: In real compositor mouse location should be mapped to a new window size
            // For example, you could:
            // 1) transform mouse pointer position from compositor space to window space (location relative)
            // 2) divide the x coordinate by width of the window to get the percentage
            //   - 0.0 would be on the far left of the window
            //   - 0.5 would be in middle of the window
            //   - 1.0 would be on the far right of the window
            // 3) multiply the percentage by new window width
            // 4) by doing that, drag will look a lot more natural
            //
            // but for anvil needs setting location to pointer location is fine
            let pos = pointer.current_location();
            initial_window_location = (pos.x as i32, pos.y as i32).into();
        }

        let grab = PointerMoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
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
