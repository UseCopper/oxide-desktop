use std::{cell::RefCell, os::unix::io::OwnedFd};

use smithay::{
    desktop::{Window, space::SpaceElement},
    input::pointer::Focus,
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XwmHandler,
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId},
    },
};
use tracing::{error, trace};

use crate::{AnvilState, focus::KeyboardFocusTarget, state::Backend};

use super::{
    FullscreenSurface, PointerMoveSurfaceGrab, PointerResizeSurfaceGrab, ResizeData, ResizeGrabState,
    ResizeState,
    SurfaceData, TouchMoveSurfaceGrab, WindowElement, place_new_window,
};

#[derive(Debug, Default)]
struct OldGeometry(RefCell<Option<Rectangle<i32, Logical>>>);
impl OldGeometry {
    pub fn save(&self, geo: Rectangle<i32, Logical>) {
        *self.0.borrow_mut() = Some(geo);
    }

    pub fn restore(&self) -> Option<Rectangle<i32, Logical>> {
        self.0.borrow_mut().take()
    }
}

impl<BackendData: Backend> XWaylandShellHandler for AnvilState<BackendData> {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

impl<BackendData: Backend> XwmHandler for AnvilState<BackendData> {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("XWM event with no XWM running")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Err(err) = window.set_mapped(true) {
            tracing::warn!(?err, "Failed to set X11 window mapped");
            return;
        }
        let window = WindowElement(Window::new_x11_window(window));
        place_new_window(&mut self.space, self.pointer.current_location(), &window, true);
        let Some(bbox) = self.space.element_bbox(&window) else {
            return;
        };
        let Some(xsurface) = window.0.x11_surface() else {
            return;
        };
        if let Err(err) = xsurface.configure(Some(bbox)) {
            tracing::warn!(?err, "Failed to configure X11 window");
        }
        window.set_ssd(!xsurface.is_decorated());
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let location = window.last_configure().loc;
        let window = WindowElement(Window::new_x11_window(window));
        self.space.map_element(window, location, true);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let maybe = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == &window))
            .cloned();
        if let Some(elem) = maybe {
            self.space.unmap_elem(&elem)
        }
        if !window.is_override_redirect()
            && let Err(err) = window.set_mapped(false)
        {
            tracing::warn!(?err, "Failed to set X11 window unmapped");
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // we just set the new size, but don't let windows move themselves around freely
        let mut geo = window.last_configure();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        let _ = window.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        let Some(elem) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == &window))
            .cloned()
        else {
            return;
        };
        self.space.map_element(elem, geometry.loc, false);
        // TODO: We don't properly handle the order of override-redirect windows here,
        //       they are always mapped top and then never reordered.
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(elem) = self.window_for_x11(&window) else {
            return;
        };
        self.maximize_window(elem);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(elem) = self.window_for_x11(&window) else {
            return;
        };
        self.unmaximize_window(elem);
    }

    fn minimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(elem) = self.window_for_x11(&window) else {
            return;
        };
        self.minimize_request(elem);
    }

    fn unminimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(elem) = self.window_for_x11(&window) else {
            return;
        };
        self.unminimize_request(elem);
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(elem) = self.window_for_x11(&window) else {
            return;
        };
        self.fullscreen_window(elem);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let Some(elem) = self.window_for_x11(&window) else {
            return;
        };
        self.unfullscreen_window(elem);
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, edges: X11ResizeEdge) {
        // luckily anvil only supports one seat anyway...
        let Some(start_data) = self.pointer.grab_start_data() else {
            return;
        };

        let Some(element) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == &window))
            .cloned()
        else {
            return;
        };

        // Content geometry, not `WindowElement::geometry` which includes SSD
        // decoration bounds; the resize math works in content space.
        let geometry = SpaceElement::geometry(&element.0);
        let Some(loc) = self.space.element_location(&element) else {
            return;
        };
        let (initial_window_location, initial_window_size) = (loc, geometry.size);

        if let Some(surface) = element.wl_surface() {
            with_states(&surface, |states| {
                if let Some(data) = states.data_map.get::<RefCell<SurfaceData>>() {
                    data.borrow_mut().resize_state = ResizeState::Resizing(ResizeData::new(
                        edges.into(),
                        initial_window_location,
                        initial_window_size,
                    ));
                }
            });
        }

        let start_location = start_data.location;
        let grab = PointerResizeSurfaceGrab {
            start_data,
            resize: ResizeGrabState::new(
                element,
                edges.into(),
                initial_window_location,
                initial_window_size,
                start_location,
            ),
        };

        let pointer = self.pointer.clone();
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        self.move_request_x11(&window)
    }

    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        if let Some(keyboard) = self.seat.get_keyboard() {
            // check that an X11 window is focused
            if let Some(KeyboardFocusTarget::Window(w)) = keyboard.current_focus() {
                if let Some(surface) = w.x11_surface() {
                    if surface.xwm_id().unwrap() == xwm {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn send_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(err) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                    error!(?err, "Failed to request current wayland clipboard for Xwayland",);
                }
            }
            SelectionTarget::Primary => {
                if let Err(err) = request_primary_client_selection(&self.seat, mime_type, fd) {
                    error!(
                        ?err,
                        "Failed to request current wayland primary selection for Xwayland",
                    );
                }
            }
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        trace!(?selection, ?mime_types, "Got Selection from X11",);
        // TODO check, that focused windows is X11 window before doing this
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ())
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ())
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat)
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat)
                }
            }
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
    }
}

impl<BackendData: Backend> AnvilState<BackendData> {
    fn window_for_x11(&self, window: &X11Surface) -> Option<WindowElement> {
        self.space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == window))
            .cloned()
    }

    pub fn maximize_request_x11(&mut self, window: &X11Surface) {
        let Some(elem) = self.window_for_x11(window) else {
            return;
        };

        let old_geo = match self.space.element_bbox(&elem) {
            Some(geo) => geo,
            None => return,
        };
        let outputs_for_window = self.space.outputs_for_element(&elem);
        let output = outputs_for_window
            .first()
            // The window hasn't been mapped yet, use the primary output instead
            .or_else(|| self.space.outputs().next());
        let Some(output) = output else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(output) else {
            return;
        };

        if let Err(err) = window.set_maximized(true) {
            tracing::warn!(?err, "Failed to set X11 maximized");
            return;
        }
        if let Err(err) = window.configure(geometry) {
            tracing::warn!(?err, "Failed to configure maximized X11 window");
        }
        window.user_data().insert_if_missing(OldGeometry::default);
        if let Some(data) = window.user_data().get::<OldGeometry>() {
            data.save(old_geo);
        }
        self.space.map_element(elem, geometry.loc, false);
    }

    pub fn unmaximize_request_x11(&mut self, window: &X11Surface) {
        let Some(elem) = self.window_for_x11(window) else {
            return;
        };

        if let Err(err) = window.set_maximized(false) {
            tracing::warn!(?err, "Failed to unset X11 maximized");
        }
        if let Some(old_geo) = window
            .user_data()
            .get::<OldGeometry>()
            .and_then(|data| data.restore())
        {
            if let Err(err) = window.configure(old_geo) {
                tracing::warn!(?err, "Failed to restore X11 window geometry");
            }
            self.space.map_element(elem, old_geo.loc, false);
        }
    }

    pub fn fullscreen_request_x11(&mut self, window: &X11Surface) {
        let Some(elem) = self.window_for_x11(window) else {
            return;
        };

        let outputs_for_window = self.space.outputs_for_element(&elem);
        let output = outputs_for_window
            .first()
            // The window hasn't been mapped yet, use the primary output instead
            .or_else(|| self.space.outputs().next());
        let Some(output) = output else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(output) else {
            return;
        };

        if let Err(err) = window.set_fullscreen(true) {
            tracing::warn!(?err, "Failed to set X11 fullscreen");
            return;
        }
        elem.set_ssd(false);
        if let Err(err) = window.configure(geometry) {
            tracing::warn!(?err, "Failed to configure fullscreen X11 window");
        }
        output.user_data().insert_if_missing(FullscreenSurface::default);
        if let Some(fs) = output.user_data().get::<FullscreenSurface>() {
            // Clear any previous fullscreen window on this output so it doesn't leak.
            if let Some(prev) = fs.get()
                && prev != elem
            {
                tracing::debug!("Replacing previous fullscreen window");
            }
            fs.set(elem.clone());
        }
        trace!("Fullscreening: {:?}", elem);
    }

    pub fn unfullscreen_request_x11(&mut self, window: &X11Surface) {
        let Some(elem) = self.window_for_x11(window) else {
            return;
        };

        if let Err(err) = window.set_fullscreen(false) {
            tracing::warn!(?err, "Failed to unset X11 fullscreen");
        }
        elem.set_ssd(!window.is_decorated());
        if let Some(output) = self.space.outputs().find(|o| {
            o.user_data()
                .get::<FullscreenSurface>()
                .and_then(|f| f.get())
                .map(|w| &w == &elem)
                .unwrap_or(false)
        }) {
            trace!("Unfullscreening: {:?}", elem);
            if let Some(fs) = output.user_data().get::<FullscreenSurface>() {
                fs.clear();
            }
            if let Some(bbox) = self.space.element_bbox(&elem)
                && let Err(err) = window.configure(bbox)
            {
                tracing::warn!(?err, "Failed to restore X11 window geometry");
            }
            self.backend_data.reset_buffers(output);
        }
    }

    pub fn move_request_x11(&mut self, window: &X11Surface) {
        if let Some(touch) = self.seat.get_touch() {
            if let Some(start_data) = touch.grab_start_data() {
                let element = self
                    .space
                    .elements()
                    .find(|e| matches!(e.0.x11_surface(), Some(w) if w == window));

                if let Some(element) = element {
                    let Some(mut initial_window_location) = self.space.element_location(element) else {
                        return;
                    };

                    // If surface is maximized then unmaximize it
                    if window.is_maximized() {
                        if let Err(err) = window.set_maximized(false) {
                            tracing::warn!(?err, "Failed to unset X11 maximized");
                        }
                        let pos = start_data.location;
                        initial_window_location = (pos.x as i32, pos.y as i32).into();
                        if let Some(old_geo) = window
                            .user_data()
                            .get::<OldGeometry>()
                            .and_then(|data| data.restore())
                            && let Err(err) =
                                window.configure(Rectangle::new(initial_window_location, old_geo.size))
                        {
                            tracing::warn!(?err, "Failed to configure X11 window");
                        }
                    }

                    let grab = TouchMoveSurfaceGrab {
                        start_data,
                        window: element.clone(),
                        initial_window_location,
                    };

                    touch.set_grab(self, grab, SERIAL_COUNTER.next_serial());
                    return;
                }
            }
        }

        // luckily anvil only supports one seat anyway...
        let Some(start_data) = self.pointer.grab_start_data() else {
            return;
        };

        let Some(element) = self
            .space
            .elements()
            .find(|e| matches!(e.0.x11_surface(), Some(w) if w == window))
        else {
            return;
        };

        let Some(mut initial_window_location) = self.space.element_location(element) else {
            return;
        };

        // If surface is maximized then unmaximize it
        if window.is_maximized() {
            if let Err(err) = window.set_maximized(false) {
                tracing::warn!(?err, "Failed to unset X11 maximized");
            }
            let pos = self.pointer.current_location();
            initial_window_location = (pos.x as i32, pos.y as i32).into();
            if let Some(old_geo) = window
                .user_data()
                .get::<OldGeometry>()
                .and_then(|data| data.restore())
                && let Err(err) = window.configure(Rectangle::new(initial_window_location, old_geo.size))
            {
                tracing::warn!(?err, "Failed to configure X11 window");
            }
        }

        let grab = PointerMoveSurfaceGrab {
            start_data,
            window: element.clone(),
            initial_window_location,
        };

        let pointer = self.pointer.clone();
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }
}
