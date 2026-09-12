use std::cell::RefCell;

use smithay::{
    backend::input::InputTime,
    desktop::{Space, WindowSurface},
    input::{
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
            GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
            GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
            PointerInnerHandle, RelativeMotionEvent,
        },
        touch::{GrabStartData as TouchGrabStartData, TouchGrab},
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{IsAlive, Logical, Point, Serial, Size, SERIAL_COUNTER as SCOUNTER},
    wayland::{
        compositor::with_states,
        shell::xdg::{SurfaceCachedState, ToplevelCachedState},
    },
};
#[cfg(feature = "xwayland")]
use smithay::{utils::Rectangle, xwayland::xwm::ResizeEdge as X11ResizeEdge};

use super::{SurfaceData, WindowElement};
use crate::{
    focus::PointerFocusTarget,
    state::{AnvilState, Backend},
};

pub struct PointerMoveSurfaceGrab<BackendData: Backend + 'static> {
    pub start_data: PointerGrabStartData<AnvilState<BackendData>>,
    pub window: WindowElement,
    pub initial_window_location: Point<i32, Logical>,
}

impl<BackendData: Backend> PointerGrab<AnvilState<BackendData>> for PointerMoveSurfaceGrab<BackendData> {
    fn motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        let delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;

        data.space
            .map_element(self.window.clone(), new_location.to_i32_round(), true);
    }

    fn relative_motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            // No more buttons are pressed, release the grab.
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<AnvilState<BackendData>> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut AnvilState<BackendData>) {}
}

pub struct TouchMoveSurfaceGrab<BackendData: Backend + 'static> {
    pub start_data: TouchGrabStartData<AnvilState<BackendData>>,
    pub window: WindowElement,
    pub initial_window_location: Point<i32, Logical>,
}

impl<BackendData: Backend> TouchGrab<AnvilState<BackendData>> for TouchMoveSurfaceGrab<BackendData> {
    fn down(
        &mut self,
        _data: &mut AnvilState<BackendData>,
        _handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(
            <AnvilState<BackendData> as smithay::input::SeatHandler>::TouchFocus,
            Point<f64, Logical>,
        )>,
        _event: &smithay::input::touch::DownEvent,
    ) {
    }

    fn up(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::UpEvent,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }

        handle.up(data, event);
        handle.unset_grab(self, data);
    }

    fn motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        _handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(
            <AnvilState<BackendData> as smithay::input::SeatHandler>::TouchFocus,
            Point<f64, Logical>,
        )>,
        event: &smithay::input::touch::MotionEvent,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }

        let delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;
        data.space
            .map_element(self.window.clone(), new_location.to_i32_round(), true);
    }

    fn frame(
        &mut self,
        _data: &mut AnvilState<BackendData>,
        _handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
    ) {
    }

    fn cancel(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
    ) {
        handle.cancel(data);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::ShapeEvent,
    ) {
        handle.shape(data, event);
    }

    fn orientation(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::OrientationEvent,
    ) {
        handle.orientation(data, event);
    }

    fn start_data(&self) -> &smithay::input::touch::GrabStartData<AnvilState<BackendData>> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut AnvilState<BackendData>) {}
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const NONE = 0;
        const TOP = 1;
        const BOTTOM = 2;
        const LEFT = 4;
        const TOP_LEFT = 5;
        const BOTTOM_LEFT = 6;
        const RIGHT = 8;
        const TOP_RIGHT = 9;
        const BOTTOM_RIGHT = 10;
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap()
    }
}

#[cfg(feature = "xwayland")]
impl From<X11ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(edge: X11ResizeEdge) -> Self {
        match edge {
            X11ResizeEdge::Bottom => ResizeEdge::BOTTOM,
            X11ResizeEdge::BottomLeft => ResizeEdge::BOTTOM_LEFT,
            X11ResizeEdge::BottomRight => ResizeEdge::BOTTOM_RIGHT,
            X11ResizeEdge::Left => ResizeEdge::LEFT,
            X11ResizeEdge::Right => ResizeEdge::RIGHT,
            X11ResizeEdge::Top => ResizeEdge::TOP,
            X11ResizeEdge::TopLeft => ResizeEdge::TOP_LEFT,
            X11ResizeEdge::TopRight => ResizeEdge::TOP_RIGHT,
        }
    }
}

/// Information about the resize operation.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ResizeData {
    /// The edges the surface is being resized with.
    pub edges: ResizeEdge,
    /// The initial window location.
    pub initial_window_location: Point<i32, Logical>,
    /// The initial window size (geometry width and height).
    pub initial_window_size: Size<i32, Logical>,
    /// Latest intended content size. Requested-only state: it drives configure
    /// requests and anchor math, never the rendered geometry.
    pub last_window_size: Size<i32, Logical>,
    /// Committed content size captured on the last client commit. This is the
    /// only size the display/decoration is allowed to use while resizing.
    pub committed_size: Size<i32, Logical>,
    /// Size carried by the most recently sent configure.
    pub last_sent_size: Size<i32, Logical>,
    /// Serial of the most recently sent configure that the client has not yet
    /// committed. While `Some`, no further size request may be sent.
    pub outstanding: Option<Serial>,
}

impl ResizeData {
    /// State for a freshly started resize. The intended, sent and committed
    /// sizes all begin at the window's current size.
    pub fn new(
        edges: ResizeEdge,
        initial_window_location: Point<i32, Logical>,
        initial_window_size: Size<i32, Logical>,
    ) -> Self {
        Self {
            edges,
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
            committed_size: initial_window_size,
            last_sent_size: initial_window_size,
            outstanding: None,
        }
    }
}

/// State of the resize operation.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum ResizeState {
    /// The surface is not being resized.
    #[default]
    NotResizing,
    /// The surface is currently being resized.
    Resizing(ResizeData),
    /// The resize has finished, and the surface needs to ack the final configure.
    WaitingForFinalAck(ResizeData, Serial),
    /// The resize has finished, and the surface needs to commit its final state.
    WaitingForCommit(ResizeData),
}

/// Record the latest intended resize size in the surface state. This drives
/// configures and the anchor math; the committed geometry is what gets rendered.
fn publish_resize_size(window: &WindowElement, size: Size<i32, Logical>) {
    let Some(surface) = window.wl_surface() else {
        return;
    };
    with_states(&surface, |states| {
        if let Some(data) = states.data_map.get::<RefCell<SurfaceData>>() {
            if let ResizeState::Resizing(resize) = &mut data.borrow_mut().resize_state {
                resize.last_window_size = size;
            }
        }
    });
}

fn resize_intended_size(window: &WindowElement) -> Option<Size<i32, Logical>> {
    let surface = window.wl_surface()?;
    with_states(&surface, |states| {
        let data = states.data_map.get::<RefCell<SurfaceData>>()?;
        if let ResizeState::Resizing(resize) = &data.borrow().resize_state {
            Some(resize.last_window_size)
        } else {
            None
        }
    })
}

/// Intended element location for TOP/LEFT edges: the edge opposite the one
/// being dragged stays fixed relative to the original geometry.
fn resize_target_location(
    edges: ResizeEdge,
    initial_location: Point<i32, Logical>,
    initial_size: Size<i32, Logical>,
    size: Size<i32, Logical>,
    current: Point<i32, Logical>,
) -> Point<i32, Logical> {
    let mut location = current;
    if edges.intersects(ResizeEdge::LEFT) {
        location.x = initial_location.x + (initial_size.w - size.w);
    }
    if edges.intersects(ResizeEdge::TOP) {
        location.y = initial_location.y + (initial_size.h - size.h);
    }
    location
}

/// Advance the serialized resize pipeline.
///
/// If the outstanding configure has been committed and the latest intended size
/// differs from the last sent size, send the next configure immediately. At most
/// one resize request is in flight. Safe to call both from the grab's frame
/// callback and from the toplevel commit handler, which is what lets the resize
/// keep advancing after the pointer has stopped.
pub fn advance_resize_configure(window: &WindowElement, space: &mut Space<WindowElement>) {
    let Some(surface) = window.wl_surface() else {
        return;
    };

    // Decide under the surface-state borrow, then drop it before sending.
    let next = with_states(&surface, |states| {
        let data = states.data_map.get::<RefCell<SurfaceData>>()?;
        let mut data = data.borrow_mut();
        let ResizeState::Resizing(resize) = &mut data.resize_state else {
            return None;
        };

        if let Some(serial) = resize.outstanding {
            let committed = states
                .cached_state
                .get::<ToplevelCachedState>()
                .current()
                .last_acked
                .as_ref()
                .is_some_and(|configure| configure.serial.is_no_older_than(&serial));
            if !committed {
                return None;
            }
            resize.outstanding = None;
        }

        if resize.last_window_size == resize.last_sent_size {
            return None;
        }

        Some((
            resize.last_window_size,
            resize.edges,
            resize.initial_window_location,
            resize.initial_window_size,
        ))
    });

    let Some((size, edges, initial_location, initial_size)) = next else {
        return;
    };

    let sent = match window.0.underlying_surface() {
        WindowSurface::Wayland(toplevel) => {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                state.size = Some(size);
            });
            match toplevel.send_pending_configure() {
                Some(serial) => Some(serial),
                // No pending change was sent; retry on the next advance.
                None => return,
            }
        }
        #[cfg(feature = "xwayland")]
        WindowSurface::X11(x11) => {
            if x11.pending_configure().is_some() {
                return;
            }
            let Some(current) = space.element_location(window) else {
                return;
            };
            let location =
                resize_target_location(edges, initial_location, initial_size, size, current);
            if x11
                .configure_with_sync(Rectangle::new(location, size), None)
                .is_err()
            {
                return;
            }
            None
        }
    };

    // Record the newly outstanding configure so the next call waits for its
    // commit (Wayland) before sending anything else.
    with_states(&surface, |states| {
        if let Some(data) = states.data_map.get::<RefCell<SurfaceData>>() {
            if let ResizeState::Resizing(resize) = &mut data.borrow_mut().resize_state {
                resize.outstanding = sent;
                resize.last_sent_size = size;
            }
        }
    });
}

fn toplevel_min_max_size(window: &WindowElement) -> (Size<i32, Logical>, Size<i32, Logical>) {
    if let Some(surface) = window.wl_surface() {
        with_states(&surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceCachedState>();
            let data = guard.current();
            (data.min_size, data.max_size)
        })
    } else {
        ((0, 0).into(), (0, 0).into())
    }
}

/// Shared interactive-resize state, driven from the pointer/touch/tablet grabs.
///
/// The requested size (`last_window_size`) is computed from the pointer and is
/// used only to configure the client and to anchor the opposite edge. What is
/// rendered is always the client's committed geometry, so the window never
/// stretches, never shows a gap, and never desyncs from its frame.
///
/// Configure requests are serialized: a new size is only sent once the client
/// has acked/committed the previous one, so fast input can't flood the client.
pub struct ResizeGrabState {
    window: WindowElement,
    edges: ResizeEdge,
    initial_window_location: Point<i32, Logical>,
    initial_window_size: Size<i32, Logical>,
    start_location: Point<f64, Logical>,
    pointer: Point<f64, Logical>,
}

impl ResizeGrabState {
    pub fn new(
        window: WindowElement,
        edges: ResizeEdge,
        initial_window_location: Point<i32, Logical>,
        initial_window_size: Size<i32, Logical>,
        start_location: Point<f64, Logical>,
    ) -> Self {
        Self {
            window,
            edges,
            initial_window_location,
            initial_window_size,
            start_location,
            pointer: start_location,
        }
    }

    /// Remember the latest pointer position. The heavy work happens once per
    /// input frame in [`Self::on_frame`].
    pub fn update_pointer(&mut self, location: Point<f64, Logical>) {
        self.pointer = location;
    }

    /// Recompute the intended size and advance the configure pipeline. Returns
    /// `false` if the window died.
    pub fn on_frame<BackendData: Backend>(&mut self, data: &mut AnvilState<BackendData>) -> bool {
        if !self.window.alive() {
            return false;
        }

        let (mut dx, mut dy) = (self.pointer - self.start_location).into();
        let mut width = self.initial_window_size.w;
        let mut height = self.initial_window_size.h;

        if self.edges.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT) {
            if self.edges.intersects(ResizeEdge::LEFT) {
                dx = -dx;
            }
            width = (self.initial_window_size.w as f64 + dx) as i32;
        }

        if self.edges.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM) {
            if self.edges.intersects(ResizeEdge::TOP) {
                dy = -dy;
            }
            height = (self.initial_window_size.h as f64 + dy) as i32;
        }

        let (min_size, max_size) = toplevel_min_max_size(&self.window);
        let min_width = min_size.w.max(1);
        let min_height = min_size.h.max(1);
        let max_width = if max_size.w == 0 { i32::MAX } else { max_size.w };
        let max_height = if max_size.h == 0 { i32::MAX } else { max_size.h };
        width = width.max(min_width).min(max_width);
        height = height.max(min_height).min(max_height);

        publish_resize_size(&self.window, (width, height).into());
        advance_resize_configure(&self.window, &mut data.space);

        true
    }

    /// Finish the drag: request the final size without the `Resizing` state and
    /// hand control to the existing `WaitingForFinalAck`/`WaitingForCommit` ack
    /// machine. The final size supersedes any still-outstanding configure, so it
    /// is never lost.
    pub fn finish<BackendData: Backend>(&mut self, data: &mut AnvilState<BackendData>, serial: Serial) {
        if !self.window.alive() {
            return;
        }
        let Some(surface) = self.window.wl_surface() else {
            return;
        };
        let Some(size) = resize_intended_size(&self.window) else {
            return;
        };

        match self.window.0.underlying_surface() {
            WindowSurface::Wayland(toplevel) => {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                    state.size = Some(size);
                });
                let configure_serial = toplevel.send_pending_configure().unwrap_or(serial);

                // Do not move the window here. The anchor location is applied by
                // `handle_toplevel_commit` when the client commits the final
                // size, so the displayed frame never leads the client buffer.

                with_states(&surface, |states| {
                    if let Some(data) = states.data_map.get::<RefCell<SurfaceData>>() {
                        let mut data = data.borrow_mut();
                        if let ResizeState::Resizing(resize_data) = data.resize_state {
                            data.resize_state =
                                ResizeState::WaitingForFinalAck(resize_data, configure_serial);
                        }
                    }
                });
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(x11) => {
                let Some(current) = data.space.element_location(&self.window) else {
                    return;
                };
                let location = resize_target_location(
                    self.edges,
                    self.initial_window_location,
                    self.initial_window_size,
                    size,
                    current,
                );
                data.space.map_element(self.window.clone(), location, true);
                if let Err(err) = x11.configure_with_sync(Rectangle::new(location, size), None) {
                    tracing::warn!(?err, "Failed to configure X11 window at end of resize");
                }

                with_states(&surface, |states| {
                    if let Some(data) = states.data_map.get::<RefCell<SurfaceData>>() {
                        let mut data = data.borrow_mut();
                        if let ResizeState::Resizing(resize_data) = data.resize_state {
                            data.resize_state = ResizeState::WaitingForCommit(resize_data);
                        }
                    }
                });
            }
        }
    }
}

pub struct PointerResizeSurfaceGrab<BackendData: Backend + 'static> {
    pub start_data: PointerGrabStartData<AnvilState<BackendData>>,
    pub resize: ResizeGrabState,
}

impl<BackendData: Backend> PointerGrab<AnvilState<BackendData>> for PointerResizeSurfaceGrab<BackendData> {
    fn motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus.
        handle.motion(data, None, event);
        // Only remember the pointer; the resize is processed once per frame.
        self.resize.update_pointer(event.location);
    }

    fn relative_motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            // No more buttons are pressed. Release the grab and hand over to the
            // final ack/commit state machine.
            handle.unset_grab(self, data, event.serial, event.time, true);
            self.resize.finish(data, event.serial);
        }
    }

    fn axis(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
    ) {
        handle.frame(data);
        if !self.resize.on_frame(data) {
            // The gesture is no longer ongoing.
            handle.unset_grab(self, data, SCOUNTER.next_serial(), InputTime::now(), true);
        }
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<AnvilState<BackendData>> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut AnvilState<BackendData>) {}
}

pub struct TouchResizeSurfaceGrab<BackendData: Backend + 'static> {
    pub start_data: TouchGrabStartData<AnvilState<BackendData>>,
    pub resize: ResizeGrabState,
}

impl<BackendData: Backend> TouchGrab<AnvilState<BackendData>> for TouchResizeSurfaceGrab<BackendData> {
    fn down(
        &mut self,
        _data: &mut AnvilState<BackendData>,
        _handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(
            <AnvilState<BackendData> as smithay::input::SeatHandler>::TouchFocus,
            Point<f64, Logical>,
        )>,
        _event: &smithay::input::touch::DownEvent,
    ) {
    }

    fn up(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::UpEvent,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }
        handle.unset_grab(self, data);
        self.resize.finish(data, event.serial);
    }

    fn motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(
            <AnvilState<BackendData> as smithay::input::SeatHandler>::TouchFocus,
            Point<f64, Logical>,
        )>,
        event: &smithay::input::touch::MotionEvent,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }
        handle.motion(data, None, event);
        self.resize.update_pointer(event.location);
    }

    fn frame(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
    ) {
        handle.frame(data);
        if !self.resize.on_frame(data) {
            handle.unset_grab(self, data);
        }
    }

    fn cancel(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
    ) {
        handle.cancel(data);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::ShapeEvent,
    ) {
        handle.shape(data, event);
    }

    fn orientation(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::OrientationEvent,
    ) {
        handle.orientation(data, event);
    }

    fn start_data(&self) -> &smithay::input::touch::GrabStartData<AnvilState<BackendData>> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut AnvilState<BackendData>) {}
}
