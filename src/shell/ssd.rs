use smithay::{
    backend::{
        allocator::Fourcc,
        input::TouchSlot,
        renderer::{
            Renderer,
            element::{
                AsRenderElements, Kind,
                memory::MemoryRenderBuffer,
                solid::{SolidColorBuffer, SolidColorRenderElement},
            },
        },
    },
    desktop::{WindowSurface, space::SpaceElement},
    input::{
        Seat,
        pointer::{CursorIcon, CursorImageStatus, Focus, GrabStartData as PointerGrabStartData},
        touch::GrabStartData as TouchGrabStartData,
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{IsAlive, Logical, Physical, Point, Scale, Serial, Size, Transform},
    wayland::compositor::with_states,
};

use std::cell::{RefCell, RefMut};

use crate::{AnvilState, state::Backend};

use super::{
    SurfaceData, WindowElement,
    grabs::{
        PointerResizeSurfaceGrab, ResizeData, ResizeEdge, ResizeGrabState, ResizeState,
        TouchResizeSurfaceGrab,
    },
};

pub struct WindowState {
    pub is_ssd: bool,
    pub fullscreen_restore: Option<(Point<i32, Logical>, Size<i32, Logical>)>,
    pub header_bar: HeaderBar,
}

#[derive(Debug, Clone)]
pub struct SSDDrag {
    /// The window being dragged
    pub window: WindowElement,
    /// Pointer position in the global compositor space at the start of the drag
    pub start_global: Point<f64, Logical>,
    /// Window position in the global compositor space at the start of the drag
    pub start_origin: Point<i32, Logical>,
}

#[derive(Debug, Clone)]
pub struct HeaderBar {
    pub pointer_loc: Option<Point<f64, Logical>>,
    pub width: u32,
    pub fullscreen: bool,
    pub close_button_hover: bool,
    pub maximize_button_hover: bool,
    pub minimize_button_hover: bool,
    pub background: SolidColorBuffer,
    pub close_button: SolidColorBuffer,
    pub maximize_button: SolidColorBuffer,
    pub minimize_button: SolidColorBuffer,
    pub borders: Borders,
    pub close_icon: MemoryRenderBuffer,
    pub maximize_icon: MemoryRenderBuffer,
    pub minimize_icon: MemoryRenderBuffer,
    pub restore_icon: MemoryRenderBuffer,
}

#[derive(Debug, Clone)]
pub struct Borders {
    pub bottom: SolidColorBuffer,
    pub left: SolidColorBuffer,
    pub right: SolidColorBuffer,
    pub width: u32,
    pub height: u32,
}

const BG_COLOR: [f32; 4] = [0.13, 0.13, 0.15, 1.0];
const ICON_COLOR: [f32; 4] = [0.8, 0.8, 0.84, 1.0];
const BUTTON_HOVER_COLOR: [f32; 4] = [0.3, 0.3, 0.34, 1.0];

pub const HEADER_BAR_HEIGHT: i32 = 30;
const BUTTON_HEIGHT: u32 = HEADER_BAR_HEIGHT as u32;
pub const BUTTON_WIDTH: u32 = 30;
pub const ICON_SIZE: i32 = 10;
pub const BORDER_WIDTH: i32 = 2;
/// Width of the resize grab band just outside the decorated window.
pub const RESIZE_MARGIN: i32 = 4;
/// evdev code for the left mouse button, used to guard interactive resizes.
pub const BTN_LEFT: u32 = 0x110;

/// Map a resize edge to the cursor shape shown while hovering it.
pub fn resize_cursor(edges: ResizeEdge) -> CursorIcon {
    let horizontal = edges.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT);
    let vertical = edges.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM);
    match (horizontal, vertical) {
        (true, false) => {
            if edges.intersects(ResizeEdge::LEFT) {
                CursorIcon::WResize
            } else {
                CursorIcon::EResize
            }
        }
        (false, true) => {
            if edges.intersects(ResizeEdge::TOP) {
                CursorIcon::NResize
            } else {
                CursorIcon::SResize
            }
        }
        (true, true) => match (edges.intersects(ResizeEdge::LEFT), edges.intersects(ResizeEdge::TOP)) {
            (true, true) => CursorIcon::NwResize,
            (false, true) => CursorIcon::NeResize,
            (true, false) => CursorIcon::SwResize,
            (false, false) => CursorIcon::SeResize,
        },
        (false, false) => CursorIcon::Default,
    }
}

pub fn content_offset() -> Point<i32, Logical> {
    Point::from((BORDER_WIDTH, HEADER_BAR_HEIGHT))
}

pub fn fullscreen_content_offset() -> Point<i32, Logical> {
    Point::from((0, HEADER_BAR_HEIGHT))
}

const CLOSE_ICON: [u8; 20] = [
    0x03, 0xFF, 0x87, 0xFF, 0xCE, 0xFD, 0xFC, 0xFC, 0x78, 0xFC, 0x78, 0xFC, 0xFC, 0xFC, 0xCE, 0xFD,
    0x87, 0xFF, 0x03, 0xFF,
];

const MAXIMIZE_ICON: [u8; 20] = [
    0xFF, 0x03, 0xFF, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03,
    0xFF, 0x03, 0xFF, 0x03,
];

const MINIMIZE_ICON: [u8; 20] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xFF, 0x03, 0xFF, 0x03,
];

const RESTORE_ICON: [u8; 20] = [
    0xFC, 0xFF, 0xFC, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3, 0xFF, 0xC3, 0xFF, 0xC3, 0xFF, 0xC3, 0xFF,
    0xFF, 0xFC, 0xFF, 0xFC,
];

fn xbm_icon(pattern: &[u8], width: u32, height: u32, color: [f32; 4]) -> MemoryRenderBuffer {
    let bytes_per_row = (width as usize + 7) / 8;
    let color = color.map(|c| (c * 255.0) as u8);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let byte = pattern[y as usize * bytes_per_row + (x as usize) / 8];
            let bit = (byte >> (x % 8)) & 1;
            if bit == 1 {
                rgba.extend_from_slice(&[color[0], color[1], color[2], 255]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    MemoryRenderBuffer::from_slice(
        &rgba,
        Fourcc::Abgr8888,
        Size::from((width as i32, height as i32)),
        1,
        Transform::Normal,
        None,
    )
}

pub fn icon_offset() -> Point<i32, Logical> {
    Point::from((
        (BUTTON_WIDTH as i32 - ICON_SIZE) / 2,
        (HEADER_BAR_HEIGHT - ICON_SIZE) / 2,
    ))
}

impl HeaderBar {
    fn pointer_is_in_header(&self) -> bool {
        self.pointer_loc
            .map(|loc| {
                loc.x >= 0.0
                    && loc.x <= self.width as f64
                    && loc.y >= 0.0
                    && loc.y < HEADER_BAR_HEIGHT as f64
            })
            .unwrap_or(false)
    }

    /// Determine which edges a window-relative point starts a resize on.
    ///
    /// `size` is the full decorated size of the window (content plus borders and
    /// header bar). The grab band is [`RESIZE_MARGIN`] pixels wide and includes
    /// the visible SSD border, so it runs from `BORDER_WIDTH - RESIZE_MARGIN` to
    /// `BORDER_WIDTH` inside each outer edge (and symmetrically on the far side).
    pub fn resize_edge(
        &self,
        pointer: Point<f64, Logical>,
        size: Size<i32, Logical>,
    ) -> Option<ResizeEdge> {
        if self.fullscreen || size.w <= 0 || size.h <= 0 {
            return None;
        }

        let margin = RESIZE_MARGIN as f64;
        let border = BORDER_WIDTH as f64;
        let (w, h) = (size.w as f64, size.h as f64);
        let mut edges = ResizeEdge::NONE;

        if pointer.x < border {
            if pointer.x < border - margin {
                return None;
            }
            edges |= ResizeEdge::LEFT;
        } else if pointer.x >= w - border {
            if pointer.x >= w - border + margin {
                return None;
            }
            edges |= ResizeEdge::RIGHT;
        }

        if pointer.y < border {
            if pointer.y < border - margin {
                return None;
            }
            edges |= ResizeEdge::TOP;
        } else if pointer.y >= h - border {
            if pointer.y >= h - border + margin {
                return None;
            }
            edges |= ResizeEdge::BOTTOM;
        }

        (!edges.is_empty()).then_some(edges)
    }

    pub fn pointer_enter(&mut self, loc: Point<f64, Logical>) {
        self.pointer_loc = Some(loc);
    }

    pub fn pointer_leave(&mut self) {
        self.pointer_loc = None;
    }

    /// Start dragging `window` from its title bar.
    ///
    /// Records the pointer's position in the global compositor space; each following
    /// motion moves the window by exactly the delta the pointer moved since here.
    ///
    /// The drag lives on [`AnvilState`] and is driven from the compositor's input
    /// path with the pointer's true global position, so it keeps tracking the
    /// pointer no matter which surface is under the cursor, and only ends when
    /// the pointer button is released.
    pub fn start_drag<B: Backend>(
        &mut self,
        state: &mut AnvilState<B>,
        window: &WindowElement,
        window_origin: Point<i32, Logical>,
    ) {
        let Some(start_pointer) = self.pointer_loc else {
            tracing::debug!(?window_origin, "SSD drag start skipped: no pointer location");
            return;
        };
        let start_global = window_origin.to_f64() + start_pointer;
        tracing::debug!(
            ?window_origin,
            ?start_pointer,
            ?start_global,
            "SSD drag started"
        );
        state.ssd_drag = Some(SSDDrag {
            window: window.clone(),
            start_global,
            start_origin: window_origin,
        });
    }

    pub fn clicked<BackendData: Backend>(
        &mut self,
        seat: &Seat<AnvilState<BackendData>>,
        state: &mut AnvilState<BackendData>,
        window: &WindowElement,
        serial: Serial,
    ) {
        if !self.pointer_is_in_header() {
            return;
        }
        match self.pointer_loc.as_ref() {
            Some(loc) if loc.x >= (self.width - BUTTON_WIDTH) as f64 => {
                state.close_window(window.clone());
            }
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 2)) as f64 => {
                let fullscreen = !self.fullscreen;
                let window = window.clone();
                match window.0.underlying_surface() {
                    WindowSurface::Wayland(_) => {
                        state.handle.insert_idle(move |data| {
                            if fullscreen {
                                data.fullscreen_window(window.clone());
                            } else {
                                data.unfullscreen_window(window.clone());
                            }
                        });
                    }
                    #[cfg(feature = "xwayland")]
                    WindowSurface::X11(_) => {
                        state.handle.insert_idle(move |data| {
                            data.maximize_window(window.clone());
                        });
                    }
                };
            }
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 3)) as f64 => {
                state.minimize_request(window.clone());
            }
            Some(_) => {
                match window.0.underlying_surface() {
                    WindowSurface::Wayland(w) => {
                        let maximized =
                            w.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Maximized));
                        if maximized {
                            let seat = seat.clone();
                            let toplevel = w.clone();
                            state
                                .handle
                                .insert_idle(move |data| data.move_request_xdg(&toplevel, &seat, serial));
                        } else if let Some(origin) = state.space.element_location(window) {
                            self.start_drag(state, window, origin);
                        }
                    }
                    #[cfg(feature = "xwayland")]
                    WindowSurface::X11(w) => {
                        if w.is_maximized() {
                            let window = w.clone();
                            state
                                .handle
                                .insert_idle(move |data| data.move_request_x11(&window));
                        } else if let Some(origin) = state.space.element_location(window) {
                            self.start_drag(state, window, origin);
                        }
                    }
                };
            }
            _ => {}
        };
    }

    pub fn touch_down<BackendData: Backend>(
        &mut self,
        _seat: &Seat<AnvilState<BackendData>>,
        state: &mut AnvilState<BackendData>,
        window: &WindowElement,
        _serial: Serial,
    ) {
        match self.pointer_loc.as_ref() {
            Some(loc) if loc.x >= (self.width - BUTTON_WIDTH) as f64 => {}
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 2)) as f64 => {}
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 3)) as f64 => {}
            Some(_) => {
                if let Some(origin) = state.space.element_location(window) {
                    self.start_drag(state, window, origin);
                }
            }
            _ => {}
        };
    }

    pub fn touch_up<BackendData: Backend>(
        &mut self,
        _seat: &Seat<AnvilState<BackendData>>,
        state: &mut AnvilState<BackendData>,
        window: &WindowElement,
    ) {
        state.end_ssd_drag();
        if !self.pointer_is_in_header() {
            return;
        }
        match self.pointer_loc.as_ref() {
            Some(loc) if loc.x >= (self.width - BUTTON_WIDTH) as f64 => {
                state.close_window(window.clone());
            }
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 2)) as f64 => {
                let fullscreen = !self.fullscreen;
                let window = window.clone();
                match window.0.underlying_surface() {
                    WindowSurface::Wayland(_) => {
                        state.handle.insert_idle(move |data| {
                            if fullscreen {
                                data.fullscreen_window(window.clone());
                            } else {
                                data.unfullscreen_window(window.clone());
                            }
                        });
                    }
                    #[cfg(feature = "xwayland")]
                    WindowSurface::X11(_) => {
                        state.handle.insert_idle(move |data| {
                            data.maximize_window(window.clone());
                        });
                    }
                };
            }
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 3)) as f64 => {
                state.minimize_request(window.clone());
            }
            _ => {}
        };
    }

    pub fn redraw(&mut self, width: u32, content_size: Size<i32, Logical>) {
        if width == 0 {
            self.width = 0;
            return;
        }

        self.background
            .update((width as i32, HEADER_BAR_HEIGHT), BG_COLOR);

        let content_width = content_size.w.max(0) as u32;
        let content_height = content_size.h.max(0) as u32;
        self.borders.width = content_width;
        self.borders.height = content_height;
        let ring_w = (content_width + 2 * BORDER_WIDTH as u32) as i32;
        let ring_h = content_height as i32;
        self.borders
            .bottom
            .update((ring_w, BORDER_WIDTH), BG_COLOR);
        self.borders
            .left
            .update((BORDER_WIDTH, ring_h), BG_COLOR);
        self.borders
            .right
            .update((BORDER_WIDTH, ring_h), BG_COLOR);

        let mut needs_redraw_buttons = false;
        if width != self.width {
            needs_redraw_buttons = true;
            self.width = width;
        }

        if self
            .pointer_loc
            .map(|l| {
                l.x < 0.0
                    || l.x > self.width as f64
                    || l.y < 0.0
                    || l.y >= HEADER_BAR_HEIGHT as f64
            })
            .unwrap_or(false)
        {
            self.pointer_loc = None;
        }

        if self
            .pointer_loc
            .as_ref()
            .map(|l| l.x >= (width - BUTTON_WIDTH) as f64)
            .unwrap_or(false)
            && (needs_redraw_buttons || !self.close_button_hover)
        {
            self.close_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), BUTTON_HOVER_COLOR);
            self.close_button_hover = true;
        } else if !self
            .pointer_loc
            .as_ref()
            .map(|l| l.x >= (width - BUTTON_WIDTH) as f64)
            .unwrap_or(false)
            && (needs_redraw_buttons || self.close_button_hover)
        {
            self.close_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), BG_COLOR);
            self.close_button_hover = false;
        }

        if self
            .pointer_loc
            .as_ref()
            .map(|l| {
                l.x < (width - BUTTON_WIDTH) as f64 && l.x >= (width - BUTTON_WIDTH * 2) as f64
            })
            .unwrap_or(false)
            && (needs_redraw_buttons || !self.maximize_button_hover)
        {
            self.maximize_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), BUTTON_HOVER_COLOR);
            self.maximize_button_hover = true;
        } else if !self
            .pointer_loc
            .as_ref()
            .map(|l| {
                l.x < (width - BUTTON_WIDTH) as f64 && l.x >= (width - BUTTON_WIDTH * 2) as f64
            })
            .unwrap_or(false)
            && (needs_redraw_buttons || self.maximize_button_hover)
        {
            self.maximize_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), BG_COLOR);
            self.maximize_button_hover = false;
        }

        if self
            .pointer_loc
            .as_ref()
            .map(|l| {
                l.x < (width - BUTTON_WIDTH * 2) as f64 && l.x >= (width - BUTTON_WIDTH * 3) as f64
            })
            .unwrap_or(false)
            && (needs_redraw_buttons || !self.minimize_button_hover)
        {
            self.minimize_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), BUTTON_HOVER_COLOR);
            self.minimize_button_hover = true;
        } else if !self
            .pointer_loc
            .as_ref()
            .map(|l| {
                l.x < (width - BUTTON_WIDTH * 2) as f64 && l.x >= (width - BUTTON_WIDTH * 3) as f64
            })
            .unwrap_or(false)
            && (needs_redraw_buttons || self.minimize_button_hover)
        {
            self.minimize_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), BG_COLOR);
            self.minimize_button_hover = false;
        }
    }
}

impl<R: Renderer> AsRenderElements<R> for HeaderBar {
    type RenderElement = SolidColorRenderElement;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        _renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        let button_offset: Point<i32, Logical> = Point::from((BUTTON_WIDTH as i32, 0));

        vec![
            SolidColorRenderElement::from_buffer(
                &self.close_button,
                location + (Point::from((self.width as i32, 0)) - button_offset)
                    .to_physical_precise_round(scale),
                scale,
                alpha,
                Kind::Unspecified,
            )
            .into(),
            SolidColorRenderElement::from_buffer(
                &self.maximize_button,
                location + (Point::from((self.width as i32, 0)) - button_offset.upscale(2))
                    .to_physical_precise_round(scale),
                scale,
                alpha,
                Kind::Unspecified,
            )
            .into(),
            SolidColorRenderElement::from_buffer(
                &self.minimize_button,
                location + (Point::from((self.width as i32, 0)) - button_offset.upscale(3))
                    .to_physical_precise_round(scale),
                scale,
                alpha,
                Kind::Unspecified,
            )
            .into(),
            SolidColorRenderElement::from_buffer(&self.background, location, scale, alpha, Kind::Unspecified)
                .into(),
        ]
    }
}

impl<R: Renderer> AsRenderElements<R> for Borders {
    type RenderElement = SolidColorRenderElement;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        _renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        let cw = self.width as i32;
        let ch = self.height as i32;
        let h = HEADER_BAR_HEIGHT;

        vec![
            SolidColorRenderElement::from_buffer(
                &self.left,
                location + Point::from((0, h)).to_physical_precise_round(scale),
                scale,
                alpha,
                Kind::Unspecified,
            )
            .into(),
            SolidColorRenderElement::from_buffer(
                &self.right,
                location + Point::from((cw + BORDER_WIDTH, h)).to_physical_precise_round(scale),
                scale,
                alpha,
                Kind::Unspecified,
            )
            .into(),
            SolidColorRenderElement::from_buffer(
                &self.bottom,
                location + Point::from((0, h + ch)).to_physical_precise_round(scale),
                scale,
                alpha,
                Kind::Unspecified,
            )
            .into(),
        ]
    }
}

impl WindowElement {
    pub fn decoration_state(&self) -> RefMut<'_, WindowState> {
        self.user_data().insert_if_missing(|| {
            RefCell::new(WindowState {
                is_ssd: false,
                fullscreen_restore: None,
                header_bar: HeaderBar {
                    pointer_loc: None,
                    width: 0,
                    fullscreen: false,
                    close_button_hover: false,
                    maximize_button_hover: false,
                    minimize_button_hover: false,
                    background: SolidColorBuffer::default(),
                    close_button: SolidColorBuffer::default(),
                    maximize_button: SolidColorBuffer::default(),
                    minimize_button: SolidColorBuffer::default(),
                    borders: Borders {
                        bottom: SolidColorBuffer::default(),
                        left: SolidColorBuffer::default(),
                        right: SolidColorBuffer::default(),
                        width: 0,
                        height: 0,
                    },
                    close_icon: xbm_icon(&CLOSE_ICON, ICON_SIZE as u32, ICON_SIZE as u32, ICON_COLOR),
                    maximize_icon: xbm_icon(
                        &MAXIMIZE_ICON,
                        ICON_SIZE as u32,
                        ICON_SIZE as u32,
                        ICON_COLOR,
                    ),
                    minimize_icon: xbm_icon(
                        &MINIMIZE_ICON,
                        ICON_SIZE as u32,
                        ICON_SIZE as u32,
                        ICON_COLOR,
                    ),
                    restore_icon: xbm_icon(
                        &RESTORE_ICON,
                        ICON_SIZE as u32,
                        ICON_SIZE as u32,
                        ICON_COLOR,
                    ),
                },
            })
        });

        self.user_data()
            .get::<RefCell<WindowState>>()
            .unwrap()
            .borrow_mut()
    }

    pub fn set_ssd(&self, ssd: bool) {
        self.decoration_state().is_ssd = ssd;
    }

    pub fn is_ssd(&self) -> bool {
        self.decoration_state().is_ssd
    }

    pub fn is_maximized(&self) -> bool {
        match self.0.underlying_surface() {
            WindowSurface::Wayland(w) => {
                w.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Maximized))
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(w) => w.is_maximized(),
            #[cfg(not(feature = "xwayland"))]
            _ => false,
        }
    }

    /// The committed content size from the surface state. For server-decorated
    /// windows the surface bbox is used (it tracks the committed buffer); for
    /// client-decorated windows the declared geometry is used.
    pub fn committed_content_size(&self) -> Size<i32, Logical> {
        if self.is_ssd() {
            self.0.bbox().size
        } else {
            SpaceElement::geometry(&self.0).size
        }
    }

    /// The content size that should be displayed.
    ///
    /// During an interactive resize this is the snapshot captured by the last
    /// commit that produced the displayed buffer, never the requested size. The
    /// pointer only drives configure requests; it can never advance what is
    /// drawn. This keeps the SSD frame and the client buffer as one visual
    /// state.
    pub fn resize_content_size(&self) -> Size<i32, Logical> {
        if let Some(size) = self.resize_committed_size() {
            return size;
        }
        self.committed_content_size()
    }

    fn resize_committed_size(&self) -> Option<Size<i32, Logical>> {
        // Only Wayland toplevels commit through `handle_toplevel_commit`, where
        // the snapshot is captured. X11 has no such hook, so keep using its live
        // surface extent.
        #[cfg(feature = "xwayland")]
        if matches!(self.0.underlying_surface(), WindowSurface::X11(_)) {
            return None;
        }
        let surface = self.wl_surface()?;
        with_states(&surface, |states| {
            let data = states.data_map.get::<RefCell<SurfaceData>>()?;
            let data = data.borrow();
            match &data.resize_state {
                ResizeState::Resizing(resize)
                | ResizeState::WaitingForFinalAck(resize, _)
                | ResizeState::WaitingForCommit(resize) => Some(resize.committed_size),
                ResizeState::NotResizing => None,
            }
        })
    }

    /// Returns the resize edge for a window-relative point, if the window can be
    /// interactively resized there.
    pub fn resize_edge_at(&self, point: Point<f64, Logical>) -> Option<ResizeEdge> {
        if !self.is_ssd() || self.is_maximized() {
            return None;
        }
        // Resolve the size before borrowing the decoration state: `geometry`
        // itself reads that state and the `RefCell` borrow would overlap.
        let size = self.geometry().size;
        let state = self.decoration_state();
        if state.header_bar.fullscreen {
            return None;
        }
        state.header_bar.resize_edge(point, size)
    }
}

impl<B: Backend> AnvilState<B> {
    pub fn end_ssd_drag(&mut self) {
        if self.ssd_drag.is_some() {
            tracing::debug!("SSD drag ended");
            self.ssd_drag = None;
        }
    }

    /// Move the window being dragged so it tracks the pointer's current global
    /// position. `global` is the authoritative pointer position in compositor
    /// space, so this keeps working no matter which surface the cursor is over.
    pub fn update_ssd_drag_position(&mut self, global: Point<f64, Logical>) {
        let Some(drag) = self.ssd_drag.clone() else {
            return;
        };
        if self.space.element_location(&drag.window).is_none() {
            self.ssd_drag = None;
            return;
        }
        let delta = global - drag.start_global;
        let new_origin = drag.start_origin + delta.to_i32_round();
        self.space.map_element(drag.window, new_origin, true);
        tracing::trace!(?global, ?new_origin, "SSD drag moved window");
    }

    /// Record the start of an SSD resize and return the initial window location,
    /// content size, and global pointer location used to drive the grab.
    fn prepare_ssd_resize(
        &mut self,
        window: &WindowElement,
        edges: ResizeEdge,
        window_relative_location: Point<f64, Logical>,
    ) -> Option<(Point<i32, Logical>, Size<i32, Logical>, Point<f64, Logical>)> {
        if !window.alive() || edges.is_empty() {
            return None;
        }
        let initial_window_location = self.space.element_location(window)?;
        // Resizing works in surface (content) coordinates, so ignore the
        // decoration bounds that `SpaceElement::geometry` adds.
        let initial_window_size = window.resize_content_size();
        let pointer_location = initial_window_location.to_f64() + window_relative_location;

        if let Some(surface) = window.wl_surface() {
            with_states(&surface, |states| {
                if let Some(data) = states.data_map.get::<RefCell<SurfaceData>>() {
                    data.borrow_mut().resize_state = ResizeState::Resizing(ResizeData::new(
                        edges,
                        initial_window_location,
                        initial_window_size,
                    ));
                }
            });
        }

        Some((initial_window_location, initial_window_size, pointer_location))
    }

    /// Begin an interactive pointer resize of an SSD window from one of its edges.
    ///
    /// The grab is installed from an idle callback because button handling runs
    /// while the pointer's internal lock is held; calling `set_grab` directly
    /// from the button callback would deadlock.
    pub fn start_ssd_resize(
        &mut self,
        window: WindowElement,
        edges: ResizeEdge,
        serial: Serial,
        button: u32,
        window_relative_location: Point<f64, Logical>,
    ) {
        let Some((initial_window_location, initial_window_size, pointer_location)) =
            self.prepare_ssd_resize(&window, edges, window_relative_location)
        else {
            return;
        };

        let grab = PointerResizeSurfaceGrab {
            start_data: PointerGrabStartData {
                focus: None,
                button,
                location: pointer_location,
            },
            resize: ResizeGrabState::new(
                window,
                edges,
                initial_window_location,
                initial_window_size,
                pointer_location,
            ),
        };

        // `Focus::Clear` resets the cursor to the default arrow, so restore the
        // resize cursor afterwards and hold it until the button is released.
        let cursor = CursorImageStatus::Named(resize_cursor(edges));
        let seat = self.seat.clone();
        self.handle.insert_idle(move |data| {
            if let Some(pointer) = seat.get_pointer() {
                pointer.set_grab(data, grab, serial, Focus::Clear);
                data.cursor_status = cursor;
            }
        });
    }

    /// Begin an interactive touch resize of an SSD window from one of its edges.
    ///
    /// Like the pointer variant, the grab is installed from an idle callback to
    /// avoid deadlocking on the touch handle's internal lock.
    pub fn start_ssd_touch_resize(
        &mut self,
        window: WindowElement,
        edges: ResizeEdge,
        serial: Serial,
        slot: TouchSlot,
        window_relative_location: Point<f64, Logical>,
    ) {
        let Some((initial_window_location, initial_window_size, pointer_location)) =
            self.prepare_ssd_resize(&window, edges, window_relative_location)
        else {
            return;
        };

        let grab = TouchResizeSurfaceGrab {
            start_data: TouchGrabStartData {
                focus: None,
                slot,
                location: pointer_location,
            },
            resize: ResizeGrabState::new(
                window,
                edges,
                initial_window_location,
                initial_window_size,
                pointer_location,
            ),
        };

        let seat = self.seat.clone();
        self.handle.insert_idle(move |data| {
            if let Some(touch) = seat.get_touch() {
                touch.set_grab(data, grab, serial);
            }
        });
    }
}
