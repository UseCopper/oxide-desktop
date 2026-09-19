use std::{
    borrow::Cow,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        input::{ButtonState, InputTime},
        renderer::{
            ImportAll, ImportMem, Renderer, Texture,
            element::{
                AsRenderElements, Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::SolidColorRenderElement,
                surface::WaylandSurfaceRenderElement,
                texture::{TextureBuffer, TextureRenderElement},
                utils::RescaleRenderElement,
            },
        },
    },
    desktop::{
        Window, WindowSurface, WindowSurfaceType, space::SpaceElement, utils::OutputPresentationFeedback,
    },
    input::{
        Seat,
        pointer::{
            AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, GestureHoldBeginEvent,
            GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent,
            PointerTarget, RelativeMotionEvent,
        },
        tablet::tool::TabletToolTarget,
        touch::{FrameMarker, TouchTarget},
    },
    output::Output,
    reexports::{
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    render_elements,
    utils::{
        IsAlive, Logical, Physical, Point, Rectangle, Scale, Serial, Size, user_data::UserDataMap,
    },
    wayland::{
        compositor::{SurfaceData as WlSurfaceData, with_states},
        dmabuf::DmabufFeedback,
        seat::WaylandFocus,
        shell::xdg::XdgToplevelSurfaceData,
    },
};

use super::ssd::{
    BTN_LEFT, BUTTON_WIDTH, BORDER_WIDTH, HEADER_BAR_HEIGHT, RESIZE_MARGIN, TITLE_PADDING,
    icon_offset, content_offset, fullscreen_content_offset, resize_cursor,
};
use crate::{AnvilState, focus::PointerFocusTarget, state::Backend};

#[derive(Debug, Clone, PartialEq)]
pub struct WindowElement(pub Window);

impl WindowElement {
    pub fn surface_under(
        &self,
        location: Point<f64, Logical>,
        window_type: WindowSurfaceType,
    ) -> Option<(PointerFocusTarget, Point<i32, Logical>)> {
        if self.is_ghosting() {
            return None;
        }
        let is_ssd = self.decoration_state().is_ssd;
        if is_ssd {
            let size = self.geometry().size;
            let in_header = location.y < HEADER_BAR_HEIGHT as f64
                && location.x >= 0.0
                && location.x < size.w as f64;
            let edge = self.resize_edge_at(location);
            if in_header || edge.is_some() {
                return Some((PointerFocusTarget::SSD(SSD(self.clone())), Point::default()));
            }
        } else if self.resize_edge_at(location).is_some() {
            // A client-decorated window in a snap group has an invisible
            // compositor edge band; route it to the SSD path so a drag starts
            // the unified group resize.
            return Some((PointerFocusTarget::SSD(SSD(self.clone())), Point::default()));
        }
        let offset = if is_ssd {
            if self.decoration_state().header_bar.fullscreen {
                fullscreen_content_offset()
            } else {
                content_offset()
            }
        } else {
            Point::default()
        };

        let surface_under = self.0.surface_under(location - offset.to_f64(), window_type);
        let (under, loc) = match self.0.underlying_surface() {
            WindowSurface::Wayland(_) => {
                surface_under.map(|(surface, loc)| (PointerFocusTarget::WlSurface(surface), loc))
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(s) => {
                surface_under.map(|(_, loc)| (PointerFocusTarget::X11Surface(s.clone()), loc))
            }
        }?;
        Some((under, loc + offset))
    }

    pub fn with_surfaces<F>(&self, processor: F)
    where
        F: FnMut(&WlSurface, &WlSurfaceData),
    {
        if self.is_ghosting() {
            return;
        }
        self.0.with_surfaces(processor);
    }

    pub fn send_frame<T, F>(
        &self,
        output: &Output,
        time: T,
        throttle: Option<Duration>,
        primary_scan_out_output: F,
    ) where
        T: Into<Duration>,
        F: FnMut(&WlSurface, &WlSurfaceData) -> Option<Output> + Copy,
    {
        if self.is_ghosting() {
            return;
        }
        self.0.send_frame(output, time, throttle, primary_scan_out_output)
    }

    pub fn send_dmabuf_feedback<'a, P, F>(
        &self,
        output: &Output,
        primary_scan_out_output: P,
        select_dmabuf_feedback: F,
    ) where
        P: FnMut(&WlSurface, &WlSurfaceData) -> Option<Output> + Copy,
        F: Fn(&WlSurface, &WlSurfaceData) -> &'a DmabufFeedback + Copy,
    {
        if self.is_ghosting() {
            return;
        }
        self.0
            .send_dmabuf_feedback(output, primary_scan_out_output, select_dmabuf_feedback)
    }

    pub fn take_presentation_feedback<F1, F2>(
        &self,
        output_feedback: &mut OutputPresentationFeedback,
        primary_scan_out_output: F1,
        presentation_feedback_flags: F2,
    ) where
        F1: FnMut(&WlSurface, &WlSurfaceData) -> Option<Output> + Copy,
        F2: FnMut(&WlSurface, &WlSurfaceData) -> wp_presentation_feedback::Kind + Copy,
    {
        if self.is_ghosting() {
            return;
        }
        self.0.take_presentation_feedback(
            output_feedback,
            primary_scan_out_output,
            presentation_feedback_flags,
        )
    }

    #[cfg(feature = "xwayland")]
    #[inline]
    pub fn is_x11(&self) -> bool {
        self.0.is_x11()
    }

    #[inline]
    pub fn is_wayland(&self) -> bool {
        self.0.is_wayland()
    }

    #[inline]
    pub fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        if self.is_ghosting() {
            return None;
        }
        self.0.wl_surface()
    }

    /// The window's app ID (Wayland) or WM_CLASS class (X11), used to pick an
    /// icon in the panel.
    pub fn app_id(&self) -> Option<String> {
        match self.0.underlying_surface() {
            WindowSurface::Wayland(_) => {
                let surface = self.wl_surface()?;
                with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .and_then(|data| data.lock().ok()?.app_id.clone())
                        .filter(|app_id| !app_id.is_empty())
                })
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(surface) => {
                let class = surface.class();
                (!class.is_empty()).then_some(class)
            }
        }
    }

    /// The window's title, if the client set one.
    pub fn title(&self) -> Option<String> {
        match self.0.underlying_surface() {
            WindowSurface::Wayland(_) => {
                let surface = self.wl_surface()?;
                with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .and_then(|data| data.lock().ok()?.title.clone())
                        .filter(|title| !title.is_empty())
                })
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(surface) => {
                let title = surface.title();
                (!title.is_empty()).then_some(title)
            }
        }
    }

    #[inline]
    pub fn user_data(&self) -> &UserDataMap {
        self.0.user_data()
    }
}

impl IsAlive for WindowElement {
    #[inline]
    fn alive(&self) -> bool {
        // A ghost outlives its client so `Space` keeps it while the close
        // transition plays from its cached frame.
        self.is_ghosting() || self.0.alive()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SSD(WindowElement);

impl IsAlive for SSD {
    #[inline]
    fn alive(&self) -> bool {
        self.0.alive()
    }
}

impl WaylandFocus for SSD {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        self.0.wl_surface()
    }
}

impl SSD {
    /// The window this decoration belongs to.
    pub fn window(&self) -> WindowElement {
        self.0.clone()
    }

    /// Track the pointer over the decoration and show a resize cursor on edges.
    ///
    /// `location` is window-relative: Smithay already subtracts the focus offset
    /// (the window origin) before invoking the pointer target.
    fn update_hover<BackendData: Backend>(
        &self,
        data: &mut AnvilState<BackendData>,
        location: Point<f64, Logical>,
    ) {
        let is_ssd = self.0.decoration_state().is_ssd;
        let snapped = self.0.decoration_state().is_snapped();
        // Use the exact same hit test as pointer focus and button handling so
        // the cursor, focus and click always agree. A CSD snapped window also
        // has an (invisible) resize edge band, so it needs the resize cursor.
        let status = if is_ssd || snapped {
            CursorImageStatus::Named(
                self.0
                    .resize_edge_at(location)
                    .map(resize_cursor)
                    .unwrap_or(CursorIcon::Default),
            )
        } else {
            CursorImageStatus::default_named()
        };
        let mut state = self.0.decoration_state();
        if state.is_ssd {
            state.header_bar.pointer_enter(location);
        }
        if data.cursor_status != status {
            data.cursor_status = status;
        }
    }
}

impl<BackendData: Backend> PointerTarget<AnvilState<BackendData>> for SSD {
    fn enter(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        data: &mut AnvilState<BackendData>,
        event: &MotionEvent,
    ) {
        self.update_hover(data, event.location);
    }
    fn motion(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        data: &mut AnvilState<BackendData>,
        event: &MotionEvent,
    ) {
        self.update_hover(data, event.location);
    }
    fn relative_motion(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &RelativeMotionEvent,
    ) {
    }
    fn button(
        &self,
        seat: &Seat<AnvilState<BackendData>>,
        data: &mut AnvilState<BackendData>,
        event: &ButtonEvent,
    ) {
        if !self.0.decoration_state().is_ssd {
            return;
        }
        if event.state == ButtonState::Pressed {
            // Resolve the edge exactly like pointer focus does, before taking a
            // mutable borrow on the decoration state.
            let pointer_loc = self.0.decoration_state().header_bar.pointer_loc;
            if event.button == BTN_LEFT
                && let Some(pointer) = pointer_loc
                && let Some(edges) = self.0.resize_edge_at(pointer)
            {
                data.start_ssd_resize(self.0.clone(), edges, event.serial, event.button, pointer);
                return;
            }
            self.0
                .decoration_state()
                .header_bar
                .clicked(seat, data, &self.0, event.serial);
        } else if event.state == ButtonState::Released {
            data.end_ssd_drag();
        }
    }
    fn axis(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _frame: AxisFrame,
    ) {
    }
    fn frame(&self, _seat: &Seat<AnvilState<BackendData>>, _data: &mut AnvilState<BackendData>) {}
    fn leave(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        data: &mut AnvilState<BackendData>,
        _serial: Serial,
        _time: InputTime,
    ) {
        let mut state = self.0.decoration_state();
        if state.is_ssd {
            state.header_bar.pointer_leave();
        }
        data.cursor_status = CursorImageStatus::default_named();
    }
    fn gesture_swipe_begin(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &GestureSwipeBeginEvent,
    ) {
    }
    fn gesture_swipe_update(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &GestureSwipeUpdateEvent,
    ) {
    }
    fn gesture_swipe_end(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &GestureSwipeEndEvent,
    ) {
    }
    fn gesture_pinch_begin(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &GesturePinchBeginEvent,
    ) {
    }
    fn gesture_pinch_update(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &GesturePinchUpdateEvent,
    ) {
    }
    fn gesture_pinch_end(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &GesturePinchEndEvent,
    ) {
    }
    fn gesture_hold_begin(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &GestureHoldBeginEvent,
    ) {
    }
    fn gesture_hold_end(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &GestureHoldEndEvent,
    ) {
    }
}

impl<BackendData: Backend> TouchTarget<AnvilState<BackendData>> for SSD {
    fn down(
        &self,
        seat: &Seat<AnvilState<BackendData>>,
        data: &mut AnvilState<BackendData>,
        event: &smithay::input::touch::DownEvent,
    ) {
        // Same as pointer: touch handling already subtracts the focus offset.
        if !self.0.decoration_state().is_ssd {
            return;
        }
        if let Some(edges) = self.0.resize_edge_at(event.location) {
            data.start_ssd_touch_resize(
                self.0.clone(),
                edges,
                event.serial,
                event.slot,
                event.location,
            );
            return;
        }
        self.0
            .decoration_state()
            .header_bar
            .touch_down(seat, data, &self.0, event.serial);
    }

    fn up(
        &self,
        seat: &Seat<AnvilState<BackendData>>,
        data: &mut AnvilState<BackendData>,
        _event: &smithay::input::touch::UpEvent,
    ) {
        let mut state = self.0.decoration_state();
        if state.is_ssd {
            state.header_bar.touch_up(seat, data, &self.0);
        }
    }

    fn motion(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        event: &smithay::input::touch::MotionEvent,
    ) {
        let mut state = self.0.decoration_state();
        if state.is_ssd {
            state.header_bar.pointer_enter(event.location);
        }
    }

    fn frame(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _marker: FrameMarker,
    ) {
    }

    fn cancel(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _marker: FrameMarker,
    ) {
    }

    fn shape(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &smithay::input::touch::ShapeEvent,
    ) {
    }

    fn orientation(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _event: &smithay::input::touch::OrientationEvent,
    ) {
    }

    fn last_frame(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
    ) -> Option<FrameMarker> {
        // It would be more correct to store the marker on frame and cancel,
        // but since we're ignoring those anyway, no need for the added complexity.
        None
    }
}

impl<BackendData: Backend> TabletToolTarget<AnvilState<BackendData>> for SSD {
    fn proximity_in(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _tool_descriptor: &smithay::backend::input::TabletToolDescriptor,
        _tablet: &smithay::input::tablet::Tablet,
        _serial: Serial,
    ) {
    }

    fn proximity_out(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _tool_descriptor: &smithay::backend::input::TabletToolDescriptor,
    ) {
        let mut state = self.0.decoration_state();
        if state.is_ssd {
            state.header_bar.pointer_leave();
        }
    }

    fn down(
        &self,
        seat: &Seat<AnvilState<BackendData>>,
        data: &mut AnvilState<BackendData>,
        _tool_descriptor: &smithay::backend::input::TabletToolDescriptor,
        event: &smithay::input::tablet::tool::DownEvent,
    ) {
        let mut state = self.0.decoration_state();
        if state.is_ssd {
            state.header_bar.touch_down(seat, data, &self.0, event.serial);
        }
    }

    fn up(
        &self,
        seat: &Seat<AnvilState<BackendData>>,
        data: &mut AnvilState<BackendData>,
        _tool_descriptor: &smithay::backend::input::TabletToolDescriptor,
        _event: &smithay::input::tablet::tool::UpEvent,
    ) {
        let mut state = self.0.decoration_state();
        if state.is_ssd {
            state.header_bar.touch_up(seat, data, &self.0);
        }
    }

    fn motion(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _tool_descriptor: &smithay::backend::input::TabletToolDescriptor,
        event: &smithay::input::tablet::tool::MotionEvent,
    ) {
        // Same as pointer: tablet handling already subtracts the focus offset.
        let mut state = self.0.decoration_state();
        if state.is_ssd {
            state.header_bar.pointer_enter(event.location);
        }
    }

    fn button(
        &self,
        seat: &Seat<AnvilState<BackendData>>,
        data: &mut AnvilState<BackendData>,
        _tool_descriptor: &smithay::backend::input::TabletToolDescriptor,
        event: &smithay::input::tablet::tool::ButtonEvent,
    ) {
        let mut state = self.0.decoration_state();
        if state.is_ssd && event.state == ButtonState::Pressed {
            state.header_bar.clicked(seat, data, &self.0, event.serial);
        }
    }

    fn axis(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _tool_descriptor: &smithay::backend::input::TabletToolDescriptor,
        _frame: smithay::input::tablet::tool::AxisFrame,
    ) {
    }

    fn frame(
        &self,
        _seat: &Seat<AnvilState<BackendData>>,
        _data: &mut AnvilState<BackendData>,
        _tool_descriptor: &smithay::backend::input::TabletToolDescriptor,
        _time: InputTime,
    ) {
    }
}

impl SpaceElement for WindowElement {
    fn geometry(&self) -> Rectangle<i32, Logical> {
        // A ghost has no live surface; report the cached decorated size.
        if let Some(size) = self.ghost_size() {
            return Rectangle::from_size(size);
        }
        let mut geo = SpaceElement::geometry(&self.0);
        let (is_ssd, animation) = {
            let state = self.decoration_state();
            (
                state.is_ssd,
                state.animation.as_ref().map(|anim| anim.sample(Instant::now()).0),
            )
        };
        // While animating, report the interpolated (decorated) size so damage
        // tracking and hit testing follow the visual window.
        if let Some(rect) = animation {
            geo.size = rect.decorated(is_ssd).to_i32_round();
            return geo;
        }
        if is_ssd {
            // Match the size used for rendering and resize math.
            geo.size = self.resize_content_size();
            geo.size.w += 2 * BORDER_WIDTH;
            geo.size.h += HEADER_BAR_HEIGHT + BORDER_WIDTH;
        }
        geo
    }
    fn bbox(&self) -> Rectangle<i32, Logical> {
        if let Some(size) = self.ghost_size() {
            return Rectangle::from_size(size);
        }
        let (is_ssd, animation) = {
            let state = self.decoration_state();
            (
                state.is_ssd,
                state.animation.as_ref().map(|anim| anim.sample(Instant::now()).0),
            )
        };
        let mut bbox = SpaceElement::bbox(&self.0);
        if is_ssd {
            if let Some(rect) = animation {
                bbox.size = rect.decorated(true).to_i32_round();
            } else {
                bbox.size.w += 2 * BORDER_WIDTH;
                bbox.size.h += HEADER_BAR_HEIGHT + BORDER_WIDTH;
            }
            // Include the outside resize band so `Space` hit testing considers
            // points just beyond the decorated window.
            bbox.loc -= Point::from((RESIZE_MARGIN, RESIZE_MARGIN));
            bbox.size.w += 2 * RESIZE_MARGIN;
            bbox.size.h += 2 * RESIZE_MARGIN;
        } else if let Some(rect) = animation {
            bbox.size = rect.decorated(false).to_i32_round();
        }
        bbox
    }
    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        // A ghost (and any window fading out) no longer accepts input.
        if self.is_ghosting() || self.is_closing() {
            return false;
        }
        if self.decoration_state().is_ssd {
            let size = self.geometry().size;
            let edge = self.resize_edge_at(*point);
            if point.y < HEADER_BAR_HEIGHT as f64
                && point.x >= 0.0
                && point.x < size.w as f64
            {
                return true;
            }
            // The resize grip is compositor-owned, so accept input there even
            // though it sits outside the client surface.
            if edge.is_some() {
                return true;
            }
            let state = self.decoration_state();
            let offset = if state.header_bar.fullscreen {
                fullscreen_content_offset()
            } else {
                content_offset()
            };
            SpaceElement::is_in_input_region(&self.0, &(*point - offset.to_f64()))
        } else {
            SpaceElement::is_in_input_region(&self.0, point)
        }
    }
    fn z_index(&self) -> u8 {
        SpaceElement::z_index(&self.0)
    }

    fn set_activate(&self, activated: bool) {
        if self.is_ghosting() {
            return;
        }
        SpaceElement::set_activate(&self.0, activated);
    }
    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        if self.is_ghosting() {
            return;
        }
        SpaceElement::output_enter(&self.0, output, overlap);
    }
    fn output_leave(&self, output: &Output) {
        if self.is_ghosting() {
            return;
        }
        SpaceElement::output_leave(&self.0, output);
    }
    #[profiling::function]
    fn refresh(&self) {
        if self.is_ghosting() {
            return;
        }
        SpaceElement::refresh(&self.0);
    }
}

render_elements!(
    pub WindowRenderElement<R> where R: ImportAll + ImportMem;
    Window=WaylandSurfaceRenderElement<R>,
    Decoration=SolidColorRenderElement,
    Icon=MemoryRenderBufferRenderElement<R>,
    Scaled=RescaleRenderElement<WaylandSurfaceRenderElement<R>>,
    Snapshot=RescaleRenderElement<MemoryRenderBufferRenderElement<R>>,
    ScaledDecoration=RescaleRenderElement<SolidColorRenderElement>,
    ScaledScaled=RescaleRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<R>>>,
    ScaledSnapshot=RescaleRenderElement<RescaleRenderElement<MemoryRenderBufferRenderElement<R>>>,
    Texture=TextureRenderElement<R::TextureId>,
    ScaledTexture=RescaleRenderElement<TextureRenderElement<R::TextureId>>,
);

impl<R: Renderer> std::fmt::Debug for WindowRenderElement<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Window(arg0) => f.debug_tuple("Window").field(arg0).finish(),
            Self::Decoration(arg0) => f.debug_tuple("Decoration").field(arg0).finish(),
            Self::Icon(arg0) => f.debug_tuple("Icon").field(arg0).finish(),
            Self::Scaled(arg0) => f.debug_tuple("Scaled").field(arg0).finish(),
            Self::Snapshot(arg0) => f.debug_tuple("Snapshot").field(arg0).finish(),
            Self::ScaledDecoration(arg0) => f.debug_tuple("ScaledDecoration").field(arg0).finish(),
            Self::ScaledScaled(arg0) => f.debug_tuple("ScaledScaled").field(arg0).finish(),
            Self::ScaledSnapshot(arg0) => f.debug_tuple("ScaledSnapshot").field(arg0).finish(),
            Self::Texture(arg0) => f.debug_tuple("Texture").field(arg0).finish(),
            Self::ScaledTexture(arg0) => f.debug_tuple("ScaledTexture").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

impl<R> AsRenderElements<R> for WindowElement
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + Send + 'static,
{
    type RenderElement = WindowRenderElement<R>;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut R,
        mut location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        // A ghost's client is gone; draw its frozen frame instead of a surface
        // that no longer exists.
        if self.is_ghosting() {
            return ghost_render_elements(self, renderer, location, scale, alpha);
        }

        let window_bbox = SpaceElement::bbox(&self.0);
        // The window's undecorated top-left, before `location` is advanced past
        // the header bar; used as the pivot for the open/close scale.
        let window_origin = location;

        // A window opening or closing fades and scales as one piece. The alpha
        // is folded into every sub-element; the scale is applied to the finished
        // list, about the center of the decorated window.
        let (visibility_alpha, visibility_scale) = self.visibility().unwrap_or((1.0, 1.0));
        let alpha = alpha * visibility_alpha;

        // While a maximize/unmaximize transition is running the window is drawn
        // at the interpolated (animated) size. The client's content is
        // crossfaded from a frozen snapshot of its pre-transition pixels to the
        // live content, while the SSD frame is re-laid out at the new size.
        let (animation_rect, progress, snapshot) = {
            let state = self.decoration_state();
            let animation = state.animation.as_ref();
            let sample = animation.map(|anim| anim.sample(Instant::now()));
            (
                sample.map(|(rect, _)| rect),
                sample.map(|(_, progress)| progress as f32).unwrap_or(1.0),
                animation.and_then(|anim| anim.snapshot()).map(|(b, s)| (b.clone(), s)),
            )
        };

        if self.decoration_state().is_ssd && !window_bbox.is_empty() {
            // The size of the buffer we can actually draw right now.
            let content_size = self.resize_content_size();
            // The size the frame should occupy on screen (the animation may
            // stretch the buffer into this while the client catches up).
            let display_size = animation_rect
                .map(|rect| rect.content.to_i32_round())
                .unwrap_or(content_size);

            // Computed before borrowing the decoration state: the maximize
            // button shows "restore" while the window is maximized or snapped,
            // and the decorations are tinted while the window is focused.
            let maximized = self.is_maximized();
            let focused = self.is_activated();
            // `None` (no title set) becomes an empty title so clearing a title
            // actually clears it; the ghost path passes `None` to retain it.
            let title = self.title().unwrap_or_default();
            let mut state = self.decoration_state();
            let fullscreen = state.header_bar.fullscreen;
            // A snapped (tiled) window is "restorable" too, so it shows the
            // restore icon and un-snaps when the button is pressed.
            let tiled = state.is_snapped();
            let width = display_size.w + if fullscreen { 0 } else { 2 * BORDER_WIDTH };
            state
                .header_bar
                .redraw(width.max(0) as u32, display_size, focused, Some(title.as_str()));

            let mut vec: Vec<WindowRenderElement<R>> = Vec::new();

            let icon_off = icon_offset();
            let base = state.header_bar.width as i32;
            let maximize_icon = if fullscreen || maximized || tiled {
                &state.header_bar.restore_icon
            } else {
                &state.header_bar.maximize_icon
            };
            let icon_locations = [
                (base - BUTTON_WIDTH as i32 + icon_off.x, &state.header_bar.close_icon),
                (base - BUTTON_WIDTH as i32 * 2 + icon_off.x, maximize_icon),
                (base - BUTTON_WIDTH as i32 * 3 + icon_off.x, &state.header_bar.minimize_icon),
            ];
            for (icon_x, icon) in icon_locations {
                let icon_pos: Point<i32, Logical> = Point::from((icon_x, icon_off.y));
                let icon_physical = (location + icon_pos.to_physical_precise_round(scale)).to_f64();
                vec.push(
                    MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        icon_physical,
                        &icon,
                        Some(alpha),
                        None,
                        None,
                        Kind::Unspecified,
                    )
                    .expect("failed to import window icon")
                    .into(),
                );
            }

            // The title sits behind the icons but in front of the bar's
            // background, so push it before the header bar's own elements.
            if state.header_bar.title_width > 0 {
                let title_pos: Point<i32, Logical> = Point::from((TITLE_PADDING, 0));
                let title_physical = (location + title_pos.to_physical_precise_round(scale)).to_f64();
                vec.push(
                    MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        title_physical,
                        &state.header_bar.title_buffer,
                        Some(alpha),
                        None,
                        None,
                        Kind::Unspecified,
                    )
                    .expect("failed to import window title")
                    .into(),
                );
            }

            vec.extend(AsRenderElements::<R>::render_elements::<WindowRenderElement<R>>(
                &state.header_bar,
                renderer,
                location,
                scale,
                alpha,
            ));

            if !fullscreen {
                vec.extend(AsRenderElements::<R>::render_elements::<WindowRenderElement<R>>(
                    &state.header_bar.borders,
                    renderer,
                    location,
                    scale,
                    alpha,
                ));
            }

            location += if fullscreen {
                fullscreen_content_offset()
            } else {
                content_offset()
            }
            .to_physical_precise_round(scale);

            // Elements are ordered front-to-back and drawn back-to-front, so the
            // snapshot must be pushed *before* the live content to land on top
            // of it. It is stretched to the same animated content rect as the
            // live content, so both frames crossfade in place.
            push_crossfade_snapshot(
                &mut vec,
                renderer,
                &snapshot,
                location,
                display_size,
                alpha,
                progress,
            );
            let window_elements: Vec<WaylandSurfaceRenderElement<R>> =
                AsRenderElements::render_elements(&self.0, renderer, location, scale, alpha);
            extend_scaled(&mut vec, window_elements, location, content_size, display_size);

            let decorated_size: Size<i32, Logical> = if fullscreen {
                Size::from((display_size.w, HEADER_BAR_HEIGHT + display_size.h))
            } else {
                Size::from((
                    display_size.w + 2 * BORDER_WIDTH,
                    display_size.h + HEADER_BAR_HEIGHT + BORDER_WIDTH,
                ))
            };
            let center = window_origin + decorated_center(decorated_size, scale);
            vec = scale_elements_about(vec, center, visibility_scale);
            vec.into_iter().map(C::from).collect()
        } else {
            let content_size = self.resize_content_size();
            let display_size = animation_rect
                .map(|rect| rect.content.to_i32_round())
                .unwrap_or(content_size);

            let mut vec: Vec<WindowRenderElement<R>> = Vec::new();
            push_crossfade_snapshot(
                &mut vec,
                renderer,
                &snapshot,
                location,
                display_size,
                alpha,
                progress,
            );
            let window_elements: Vec<WaylandSurfaceRenderElement<R>> =
                AsRenderElements::render_elements(&self.0, renderer, location, scale, alpha);
            extend_scaled(&mut vec, window_elements, location, content_size, display_size);
            let center = window_origin + decorated_center(display_size, scale);
            vec = scale_elements_about(vec, center, visibility_scale);
            vec.into_iter().map(C::from).collect()
        }
    }
}

/// Draw the frozen frame of a window whose client is gone, faded and scaled by
/// its close transition.
fn ghost_render_elements<R, C>(
    window: &WindowElement,
    renderer: &mut R,
    location: Point<i32, Physical>,
    scale: Scale<f64>,
    alpha: f32,
) -> Vec<C>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + Send + 'static,
    C: From<WindowRenderElement<R>>,
{
    let state = window.decoration_state();
    let Some(frame) = state.last_frame.as_ref() else {
        return Vec::new();
    };
    let (visibility_alpha, visibility_scale) = state
        .visibility
        .animation
        .as_ref()
        .map(|animation| animation.sample(Instant::now()))
        .unwrap_or((1.0, 1.0));
    let surfaces = frame.surfaces.clone();
    let geometry = frame.geometry;
    let content_size = geometry.size;
    let is_ssd = state.is_ssd;
    let fullscreen = state.header_bar.fullscreen;
    drop(state);

    let alpha = alpha * visibility_alpha;

    let window_origin = location;
    let mut location = location;
    let mut vec: Vec<WindowRenderElement<R>> = Vec::new();

    if is_ssd {
        let mut state = window.decoration_state();
        let width = content_size.w + if fullscreen { 0 } else { 2 * BORDER_WIDTH };
        // No hover state on a closing window.
        state.header_bar.pointer_loc = None;
        // Keep the title the live window last showed.
        state
            .header_bar
            .redraw(width.max(0) as u32, content_size, false, None);

        let icon_off = icon_offset();
        let base = state.header_bar.width as i32;
        let icon_locations = [
            (
                base - BUTTON_WIDTH as i32 + icon_off.x,
                &state.header_bar.close_icon,
            ),
            (
                base - BUTTON_WIDTH as i32 * 2 + icon_off.x,
                &state.header_bar.maximize_icon,
            ),
            (
                base - BUTTON_WIDTH as i32 * 3 + icon_off.x,
                &state.header_bar.minimize_icon,
            ),
        ];
        for (icon_x, icon) in icon_locations {
            let icon_pos: Point<i32, Logical> = Point::from((icon_x, icon_off.y));
            let icon_physical = (location + icon_pos.to_physical_precise_round(scale)).to_f64();
            vec.push(
                MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    icon_physical,
                    icon,
                    Some(alpha),
                    None,
                    None,
                    Kind::Unspecified,
                )
                .expect("failed to import window icon")
                .into(),
            );
        }

        if state.header_bar.title_width > 0 {
            let title_pos: Point<i32, Logical> = Point::from((TITLE_PADDING, 0));
            let title_physical = (location + title_pos.to_physical_precise_round(scale)).to_f64();
            vec.push(
                MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    title_physical,
                    &state.header_bar.title_buffer,
                    Some(alpha),
                    None,
                    None,
                    Kind::Unspecified,
                )
                .expect("failed to import window title")
                .into(),
            );
        }

        vec.extend(AsRenderElements::<R>::render_elements::<WindowRenderElement<R>>(
            &state.header_bar,
            renderer,
            location,
            scale,
            alpha,
        ));
        if !fullscreen {
            vec.extend(AsRenderElements::<R>::render_elements::<WindowRenderElement<R>>(
                &state.header_bar.borders,
                renderer,
                location,
                scale,
                alpha,
            ));
        }

        location += if fullscreen {
            fullscreen_content_offset()
        } else {
            content_offset()
        }
        .to_physical_precise_round(scale);
    }

    // Draw every held surface (root plus subsurfaces) at its offset within the
    // window, so e.g. Firefox's content subsurface is included. For a
    // client-decorated window the buffer has a shadow margin, so shift it up by
    // the window geometry offset to line the content up with the window.
    let surface_origin = if is_ssd {
        location
    } else {
        window_origin - geometry.loc.to_physical_precise_round(scale)
    };
    for surface in &surfaces {
        let Some(Ok(texture)) = renderer.import_buffer(&surface.buffer, None, &[]) else {
            continue;
        };
        let texture_buffer =
            TextureBuffer::from_texture(renderer, texture, surface.scale, surface.transform, None);
        let surface_location = surface_origin + surface.location.to_physical_precise_round(scale);
        vec.push(WindowRenderElement::Texture(
            TextureRenderElement::from_texture_buffer(
                surface_location.to_f64(),
                &texture_buffer,
                Some(alpha),
                surface.src,
                Some(surface.size),
                Kind::Unspecified,
            ),
        ));
    }

    let decorated_size: Size<i32, Logical> = if is_ssd {
        if fullscreen {
            Size::from((content_size.w, HEADER_BAR_HEIGHT + content_size.h))
        } else {
            Size::from((
                content_size.w + 2 * BORDER_WIDTH,
                content_size.h + HEADER_BAR_HEIGHT + BORDER_WIDTH,
            ))
        }
    } else {
        content_size
    };
    let center = window_origin + decorated_center(decorated_size, scale);
    scale_elements_about(vec, center, visibility_scale)
        .into_iter()
        .map(C::from)
        .collect()
}

/// The offset from a window's top-left to the center of its `decorated_size`,
/// in physical pixels.
fn decorated_center(decorated_size: Size<i32, Logical>, scale: Scale<f64>) -> Point<i32, Physical> {
    Point::<f64, Physical>::from((
        decorated_size.w as f64 / 2.0 * scale.x,
        decorated_size.h as f64 / 2.0 * scale.y,
    ))
    .to_i32_round()
}

/// Re-scale every element of a window about `origin` by `scale`, used for the
/// open/close transition. Already-scaled elements are wrapped one level deeper
/// so the transforms compose.
fn scale_elements_about<R>(
    elements: Vec<WindowRenderElement<R>>,
    origin: Point<i32, Physical>,
    scale: f64,
) -> Vec<WindowRenderElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + Send + 'static,
{
    if (scale - 1.0).abs() < f64::EPSILON {
        return elements;
    }
    elements
        .into_iter()
        .map(|element| match element {
            WindowRenderElement::Window(e) => WindowRenderElement::Scaled(
                RescaleRenderElement::from_element(e, origin, scale),
            ),
            WindowRenderElement::Decoration(e) => WindowRenderElement::ScaledDecoration(
                RescaleRenderElement::from_element(e, origin, scale),
            ),
            WindowRenderElement::Icon(e) => WindowRenderElement::Snapshot(
                RescaleRenderElement::from_element(e, origin, scale),
            ),
            WindowRenderElement::Scaled(e) => WindowRenderElement::ScaledScaled(
                RescaleRenderElement::from_element(e, origin, scale),
            ),
            WindowRenderElement::Snapshot(e) => WindowRenderElement::ScaledSnapshot(
                RescaleRenderElement::from_element(e, origin, scale),
            ),
            WindowRenderElement::Texture(e) => WindowRenderElement::ScaledTexture(
                RescaleRenderElement::from_element(e, origin, scale),
            ),
            other => other,
        })
        .collect()
}

/// Draw the frozen pre-transition frame stretched into the animated content
/// area, fading out as the live frame shows through. No-op without a snapshot.
/// Pushed *before* the live content, since elements are front-to-back.
#[allow(clippy::too_many_arguments)]
fn push_crossfade_snapshot<R>(
    out: &mut Vec<WindowRenderElement<R>>,
    renderer: &mut R,
    snapshot: &Option<(MemoryRenderBuffer, Size<i32, Logical>)>,
    origin: Point<i32, Physical>,
    display_size: Size<i32, Logical>,
    alpha: f32,
    progress: f32,
) where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let Some((buffer, native)) = snapshot else {
        return;
    };
    // Let the element use the buffer's native size as its source, then stretch
    // it to the animated size. Overriding `from_buffer`'s `size` instead would
    // make it sample a source rect larger than the captured texture.
    let element = MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        origin.to_f64(),
        buffer,
        Some(alpha * (1.0 - progress)),
        None,
        None,
        Kind::Unspecified,
    )
    .expect("failed to import window snapshot");
    let scale = Scale::from((
        display_size.w as f64 / native.w.max(1) as f64,
        display_size.h as f64 / native.h.max(1) as f64,
    ));
    out.push(WindowRenderElement::Snapshot(RescaleRenderElement::from_element(
        element, origin, scale,
    )));
}

/// Append the client content, stretching it from the committed size to the
/// displayed (animated) size when they differ. `origin` is the physical
/// top-left the content is anchored to.
fn extend_scaled<R>(
    out: &mut Vec<WindowRenderElement<R>>,
    elements: Vec<WaylandSurfaceRenderElement<R>>,
    origin: Point<i32, Physical>,
    committed: Size<i32, Logical>,
    display: Size<i32, Logical>,
) where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + Send + 'static,
{
    if display == committed || committed.w <= 0 || committed.h <= 0 {
        out.extend(elements.into_iter().map(WindowRenderElement::from));
        return;
    }
    let scale = Scale::from((
        display.w as f64 / committed.w as f64,
        display.h as f64 / committed.h as f64,
    ));
    out.extend(elements.into_iter().map(|element| {
        WindowRenderElement::Scaled(RescaleRenderElement::from_element(element, origin, scale))
    }));
}
