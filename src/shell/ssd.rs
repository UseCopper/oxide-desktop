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
            utils::Buffer as RenderBuffer,
        },
    },
    desktop::{WindowSurface, space::SpaceElement},
    input::{
        Seat,
        pointer::{CursorIcon, CursorImageStatus, Focus, GrabStartData as PointerGrabStartData},
        touch::GrabStartData as TouchGrabStartData,
    },
    output::WeakOutput,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Serial, Size, Transform},
    wayland::compositor::with_states,
};

use std::{
    cell::{Cell, RefCell, RefMut},
    time::{Duration, Instant},
};

use crate::{AnvilState, state::Backend};

use super::{
    SnapGrid, SnapTarget, SnapZone, SurfaceData, VisibilityAnimation, WindowAnimation,
    WindowElement,
    grabs::{
        PointerResizeSurfaceGrab, ResizeData, ResizeEdge, ResizeGrabState, ResizeState,
        TouchResizeSurfaceGrab,
    },
};

pub struct WindowState {
    pub is_ssd: bool,
    /// Where to restore the window after fullscreen, as a fraction of the work
    /// area of the output it was fullscreened on. A *weak* output handle is
    /// stored so the window and the output's fullscreen state can't keep each
    /// other alive (and because the output may be unplugged while fullscreen).
    pub fullscreen_restore: Option<(WeakOutput, RelativeGeometry)>,
    pub maximize_restore: Option<RelativeGeometry>,
    /// Position and size of the window as fractions of its output's work area
    /// (0.0..=1.0, with 0.5,0.5 being the middle). Captured before an output
    /// resize and reapplied against the new work area so floating windows keep
    /// their relative placement across resolution changes and monitor layouts.
    pub relative: Option<RelativeGeometry>,
    /// An in-flight maximize/unmaximize transition, if any. While set, the
    /// window is drawn at the sampled geometry instead of its committed size.
    pub animation: Option<WindowAnimation>,
    /// The window's open/close fade-and-scale transition.
    pub visibility: VisibilityState,
    /// The last buffer the client committed. Holding Smithay's `Buffer` (an
    /// `Arc`) keeps the pixels alive after the client destroys its surface, so an
    /// app-triggered close can be rendered from it. Cheap: no GPU work until the
    /// ghost actually renders.
    pub last_frame: Option<LastFrame>,
    /// Stable identifier used by the panel to refer to this window. Assigned
    /// lazily the first time the window list is published.
    pub panel_id: Option<u64>,
    /// The snap zone this window is tiled into, while it remains snapped.
    pub snap_zone: Option<SnapZone>,
    /// The divider grid this window's snap group is laid out on. Shared by the
    /// group, updated when a resize moves a divider.
    pub snap_grid: Option<SnapGrid>,
    pub header_bar: HeaderBar,
}

/// How the pointer is anchored to a window during a client-initiated move, so
/// the window can be re-placed under the pointer for any committed size: the
/// horizontal fraction of the width and the vertical offset from the top stay
/// fixed, matching [`super::restore_drag_location`].
#[derive(Debug, Clone, Copy)]
pub struct DragAnchor {
    /// Horizontal fraction of the window width the pointer grabbed at.
    pub fraction: f64,
    /// Vertical offset (from the window top) the pointer grabbed at.
    pub rel_y: f64,
}

impl DragAnchor {
    /// Capture the anchor from a grab on a window of `decorated_size` at
    /// `window_loc`.
    pub fn capture(
        window_loc: Point<i32, Logical>,
        decorated_size: Size<i32, Logical>,
        grab: Point<f64, Logical>,
    ) -> Self {
        let rel_x = grab.x - window_loc.x as f64;
        let rel_y = grab.y - window_loc.y as f64;
        let width = decorated_size.w as f64;
        Self {
            fraction: if width > 0.0 { rel_x / width } else { 0.5 },
            rel_y,
        }
    }

    /// The window origin that keeps the anchored titlebar spot under the
    /// pointer for a window of `decorated` size. `pointer` is the current
    /// pointer position, so the window follows it while adapting to any size
    /// the client committed.
    pub fn locate(
        &self,
        pointer: Point<f64, Logical>,
        decorated: Size<i32, Logical>,
    ) -> Point<i32, Logical> {
        Point::from((
            (pointer.x - self.fraction * decorated.w as f64).round() as i32,
            (pointer.y - self.rel_y).round() as i32,
        ))
    }
}

/// The last committed frame of a window: every mapped surface in its tree, so
/// both the client content and any subsurfaces (e.g. Firefox's content) are
/// kept. Holding Smithay's `Buffer` (an `Arc`) keeps the pixels alive after the
/// client destroys its surface, and costs no GPU work until the ghost renders.
#[derive(Debug, Clone)]
pub struct LastFrame {
    pub surfaces: Vec<SurfaceFrame>,
    /// The client-declared window geometry: its offset within the root buffer
    /// (the CSD shadow padding) and the content size. Lets the ghost line the
    /// content up exactly where the live window drew it.
    pub geometry: Rectangle<i32, Logical>,
}

/// One mapped surface of a window, at its position relative to the window's
/// top-left.
#[derive(Debug, Clone)]
pub struct SurfaceFrame {
    pub buffer: RenderBuffer,
    pub location: Point<i32, Logical>,
    /// Logical destination size.
    pub size: Size<i32, Logical>,
    pub scale: i32,
    pub transform: Transform,
    /// Viewport source crop, if the surface uses a viewport.
    pub src: Option<Rectangle<f64, Logical>>,
}

/// Per-window state for a window whose client is gone but whose close
/// transition is still playing from its cached frame.
///
/// Kept in its own `Cell`-based struct (not `WindowState`) so `IsAlive::alive`
/// and geometry hit tests can read it without borrowing the decoration state,
/// which would re-enter and panic.
#[derive(Debug, Default)]
pub struct GhostState {
    active: Cell<bool>,
    size: Cell<Size<i32, Logical>>,
}

impl GhostState {
    pub fn is_active(&self) -> bool {
        self.active.get()
    }

    pub fn size(&self) -> Option<Size<i32, Logical>> {
        self.active.get().then(|| self.size.get())
    }

    pub fn begin(&self, size: Size<i32, Logical>) {
        self.size.set(size);
        self.active.set(true);
    }

    pub fn end(&self) {
        self.active.set(false);
    }
}

/// State backing a window's open/close transition.
#[derive(Debug, Clone, Default)]
pub struct VisibilityState {
    /// The in-flight transition, if any. Kept set at the end of a close so the
    /// window stays hidden until the client actually destroys it.
    pub animation: Option<VisibilityAnimation>,
    /// A newly mapped window waits for its first buffer before the open
    /// transition starts, so a slow client doesn't waste the animation.
    pub open_pending: bool,
    /// Set as soon as a close is requested: the window stops taking input and
    /// the client is told to close once the transition finishes.
    pub closing: bool,
    /// When the close request was actually sent to the client, used to give up
    /// waiting and un-hide a client that ignores the request.
    pub close_sent_at: Option<Instant>,
}

/// How long to wait for a client to honour a close request before un-hiding it.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// A window's geometry expressed relative to its output's work area.
#[derive(Debug, Clone, Copy, Default)]
pub struct RelativeGeometry {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone)]
pub struct SSDDrag {
    /// The window being dragged
    pub window: WindowElement,
    /// Pointer position in the global compositor space at the start of the drag
    pub start_global: Point<f64, Logical>,
    /// Window position in the global compositor space at the start of the drag
    pub start_origin: Point<i32, Logical>,
    /// The snap target the pointer is currently over, resolved on each motion
    /// and applied when the drag ends.
    pub snap_target: Option<SnapTarget>,
}

#[derive(Debug, Clone)]
pub struct HeaderBar {
    pub pointer_loc: Option<Point<f64, Logical>>,
    pub width: u32,
    pub fullscreen: bool,
    /// The floating geometry to restore when a snapped window is dragged back
    /// out of its zone. Set when a snap is applied and taken on drag start.
    /// Lives here (rather than on [`WindowState`]) so `start_drag` can take it
    /// while the decoration state is already borrowed.
    pub snap_restore: Option<RelativeGeometry>,
    pub focused: bool,
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
    /// The window title, cached so it is only re-rasterized when it or the
    /// available width changes.
    pub title: String,
    pub title_buffer: MemoryRenderBuffer,
    /// Logical width of `title_buffer`; `0` means there is nothing to draw.
    pub title_width: i32,
    /// The width the title was last laid out for, to detect relayouts.
    pub title_max_width: i32,
}

#[derive(Debug, Clone)]
pub struct Borders {
    pub bottom: SolidColorBuffer,
    pub left: SolidColorBuffer,
    pub right: SolidColorBuffer,
    pub width: u32,
    pub height: u32,
}

const BG_COLOR: [f32; 4] = [33.0 / 255.0, 33.0 / 255.0, 33.0 / 255.0, 1.0];
const BG_COLOR_FOCUSED: [f32; 4] = [43.0 / 255.0, 43.0 / 255.0, 43.0 / 255.0, 1.0];
const ICON_COLOR: [f32; 4] = [0.8, 0.8, 0.8, 1.0];
const TITLE_COLOR: [f32; 4] = [0.85, 0.85, 0.87, 1.0];
const BUTTON_HOVER_COLOR: [f32; 4] = [0.3, 0.3, 0.3, 1.0];

pub const HEADER_BAR_HEIGHT: i32 = 30;
const BUTTON_HEIGHT: u32 = HEADER_BAR_HEIGHT as u32;
pub const BUTTON_WIDTH: u32 = 30;
/// Space between the title and the window edge / buttons.
pub const TITLE_PADDING: i32 = 8;
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
}

/// The resize edge for a point on a client-decorated window, using an invisible
/// band just inside its outer bounds. `size` is the window's content size.
fn resize_edge_band(point: Point<f64, Logical>, size: Size<i32, Logical>) -> Option<ResizeEdge> {
    if size.w <= 0 || size.h <= 0 {
        return None;
    }
    let band = RESIZE_MARGIN as f64;
    let (w, h) = (size.w as f64, size.h as f64);
    // Only accept points inside the window's bounds; the band is inside the
    // edge so dragging the very border is still the app's, not ours.
    if point.x < 0.0 || point.y < 0.0 || point.x > w || point.y > h {
        return None;
    }
    let mut edges = ResizeEdge::NONE;
    if point.x <= band {
        edges |= ResizeEdge::LEFT;
    } else if point.x >= w - band {
        edges |= ResizeEdge::RIGHT;
    }
    if point.y <= band {
        edges |= ResizeEdge::TOP;
    } else if point.y >= h - band {
        edges |= ResizeEdge::BOTTOM;
    }
    (!edges.is_empty()).then_some(edges)
}

impl HeaderBar {
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
        // The drag owns the window's position, so any restore animation that
        // starts on the first motion only animates its size. Reset any stale
        // preview so the dwell starts fresh from the first motion.
        state.dragging_window = Some(window.clone());
        state.clear_snap_preview();
        state.ssd_drag = Some(SSDDrag {
            window: window.clone(),
            start_global,
            start_origin: window_origin,
            snap_target: None,
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
            Some(loc) if loc.x >= (self.width.saturating_sub(BUTTON_WIDTH)) as f64 => {
                // Deferred: the caller holds the decoration state borrowed, and
                // `close_window` mutates it.
                let window = window.clone();
                state.handle.insert_idle(move |data| data.close_window(window));
            }
            Some(loc) if loc.x >= (self.width.saturating_sub(BUTTON_WIDTH * 2)) as f64 => {
                // Deferred: the caller holds the decoration state borrowed, and
                // `toggle_maximize` mutates it.
                let window = window.clone();
                state
                    .handle
                    .insert_idle(move |data| data.toggle_maximize(window));
            }
            Some(loc) if loc.x >= (self.width.saturating_sub(BUTTON_WIDTH * 3)) as f64 => {
                // Deferred: the caller holds the decoration state borrowed, and
                // `minimize_request` mutates it.
                let window = window.clone();
                state.handle.insert_idle(move |data| data.minimize_request(window));
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
            Some(loc) if loc.x >= (self.width.saturating_sub(BUTTON_WIDTH)) as f64 => {}
            Some(loc) if loc.x >= (self.width.saturating_sub(BUTTON_WIDTH * 2)) as f64 => {}
            Some(loc) if loc.x >= (self.width.saturating_sub(BUTTON_WIDTH * 3)) as f64 => {}
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
            Some(loc) if loc.x >= (self.width.saturating_sub(BUTTON_WIDTH)) as f64 => {
                // Deferred: the caller holds the decoration state borrowed, and
                // `close_window` mutates it.
                let window = window.clone();
                state.handle.insert_idle(move |data| data.close_window(window));
            }
            Some(loc) if loc.x >= (self.width.saturating_sub(BUTTON_WIDTH * 2)) as f64 => {
                // Deferred: the caller holds the decoration state borrowed, and
                // `toggle_maximize` mutates it.
                let window = window.clone();
                state
                    .handle
                    .insert_idle(move |data| data.toggle_maximize(window));
            }
            Some(loc) if loc.x >= (self.width.saturating_sub(BUTTON_WIDTH * 3)) as f64 => {
                // Deferred: the caller holds the decoration state borrowed, and
                // `minimize_request` mutates it.
                let window = window.clone();
                state.handle.insert_idle(move |data| data.minimize_request(window));
            }
            _ => {}
        };
    }

    pub fn redraw(
        &mut self,
        width: u32,
        content_size: Size<i32, Logical>,
        focused: bool,
        title: Option<&str>,
    ) {
        if width == 0 {
            self.width = 0;
            return;
        }

        // The title sits on the left, in the space the three buttons leave.
        let max_title_width =
            (width as i32 - BUTTON_WIDTH as i32 * 3 - 2 * TITLE_PADDING).max(0);
        if let Some(title) = title
            && (title != self.title || max_title_width != self.title_max_width)
        {
            self.title.clear();
            self.title.push_str(title);
            self.title_max_width = max_title_width;
            match crate::text::rasterize(title, max_title_width, HEADER_BAR_HEIGHT, TITLE_COLOR) {
                Some((buffer, size)) => {
                    self.title_buffer = buffer;
                    self.title_width = size.w;
                }
                None => {
                    self.title_width = 0;
                }
            }
        }

        let bg = if focused { BG_COLOR_FOCUSED } else { BG_COLOR };

        self.background
            .update((width as i32, HEADER_BAR_HEIGHT), bg);

        let content_width = content_size.w.max(0) as u32;
        let content_height = content_size.h.max(0) as u32;
        self.borders.width = content_width;
        self.borders.height = content_height;
        let ring_w = (content_width + 2 * BORDER_WIDTH as u32) as i32;
        let ring_h = content_height as i32;
        self.borders.bottom.update((ring_w, BORDER_WIDTH), bg);
        self.borders.left.update((BORDER_WIDTH, ring_h), bg);
        self.borders.right.update((BORDER_WIDTH, ring_h), bg);

        let mut needs_redraw_buttons = false;
        if width != self.width {
            needs_redraw_buttons = true;
            self.width = width;
        }
        if focused != self.focused {
            needs_redraw_buttons = true;
            self.focused = focused;
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
            .map(|l| l.x >= (width.saturating_sub(BUTTON_WIDTH)) as f64)
            .unwrap_or(false)
            && (needs_redraw_buttons || !self.close_button_hover)
        {
            self.close_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), BUTTON_HOVER_COLOR);
            self.close_button_hover = true;
        } else if !self
            .pointer_loc
            .as_ref()
            .map(|l| l.x >= (width.saturating_sub(BUTTON_WIDTH)) as f64)
            .unwrap_or(false)
            && (needs_redraw_buttons || self.close_button_hover)
        {
            self.close_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), bg);
            self.close_button_hover = false;
        }

        if self
            .pointer_loc
            .as_ref()
            .map(|l| {
                l.x < (width.saturating_sub(BUTTON_WIDTH)) as f64 && l.x >= (width.saturating_sub(BUTTON_WIDTH * 2)) as f64
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
                l.x < (width.saturating_sub(BUTTON_WIDTH)) as f64 && l.x >= (width.saturating_sub(BUTTON_WIDTH * 2)) as f64
            })
            .unwrap_or(false)
            && (needs_redraw_buttons || self.maximize_button_hover)
        {
            self.maximize_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), bg);
            self.maximize_button_hover = false;
        }

        if self
            .pointer_loc
            .as_ref()
            .map(|l| {
                l.x < (width.saturating_sub(BUTTON_WIDTH * 2)) as f64 && l.x >= (width.saturating_sub(BUTTON_WIDTH * 3)) as f64
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
                l.x < (width.saturating_sub(BUTTON_WIDTH * 2)) as f64 && l.x >= (width.saturating_sub(BUTTON_WIDTH * 3)) as f64
            })
            .unwrap_or(false)
            && (needs_redraw_buttons || self.minimize_button_hover)
        {
            self.minimize_button
                .update((BUTTON_WIDTH as i32, BUTTON_HEIGHT as i32), bg);
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

        // Only draw a button's background while it is hovered: otherwise it is
        // the same color as the bar and drawing it as a separate opaque rect
        // makes it show through as a rectangle whenever the whole frame is faded
        // out (each element is alpha-composited independently).
        let mut vec: Vec<C> = Vec::new();
        if self.close_button_hover {
            vec.push(
                SolidColorRenderElement::from_buffer(
                    &self.close_button,
                    location + (Point::from((self.width as i32, 0)) - button_offset)
                        .to_physical_precise_round(scale),
                    scale,
                    alpha,
                    Kind::Unspecified,
                )
                .into(),
            );
        }
        if self.maximize_button_hover {
            vec.push(
                SolidColorRenderElement::from_buffer(
                    &self.maximize_button,
                    location + (Point::from((self.width as i32, 0)) - button_offset.upscale(2))
                        .to_physical_precise_round(scale),
                    scale,
                    alpha,
                    Kind::Unspecified,
                )
                .into(),
            );
        }
        if self.minimize_button_hover {
            vec.push(
                SolidColorRenderElement::from_buffer(
                    &self.minimize_button,
                    location + (Point::from((self.width as i32, 0)) - button_offset.upscale(3))
                        .to_physical_precise_round(scale),
                    scale,
                    alpha,
                    Kind::Unspecified,
                )
                .into(),
            );
        }
        vec.push(
            SolidColorRenderElement::from_buffer(&self.background, location, scale, alpha, Kind::Unspecified)
                .into(),
        );
        vec
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
                maximize_restore: None,
                relative: None,
                animation: None,
                visibility: VisibilityState::default(),
                last_frame: None,
                panel_id: None,
                snap_zone: None,
                snap_grid: None,
                header_bar: HeaderBar {
                    pointer_loc: None,
                    width: 0,
                    fullscreen: false,
                    snap_restore: None,
                    focused: false,
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
                    title: String::new(),
                    title_buffer: MemoryRenderBuffer::default(),
                    title_width: 0,
                    title_max_width: -1,
                },
            })
        });

        self.user_data()
            .insert_if_missing(GhostState::default);

        self.user_data()
            .get::<RefCell<WindowState>>()
            .unwrap()
            .borrow_mut()
    }

    /// The ghost tracking for this window, if its decoration state was ever set
    /// up. Returns `None` before the first `decoration_state()` call.
    fn ghost_state(&self) -> Option<&GhostState> {
        self.user_data().get::<GhostState>()
    }

    /// Whether the client is gone and the window is being drawn from its cached
    /// frame for the remainder of its close transition.
    pub fn is_ghosting(&self) -> bool {
        self.ghost_state().is_some_and(GhostState::is_active)
    }

    /// The cached frame size, while ghosting.
    pub fn ghost_size(&self) -> Option<Size<i32, Logical>> {
        self.ghost_state().and_then(GhostState::size)
    }

    /// Turn a window whose client is gone into a self-contained closing ghost.
    /// The window stays in the space (via `IsAlive`) until the transition ends.
    pub fn begin_ghost(&self, size: Size<i32, Logical>) {
        self.user_data().insert_if_missing(GhostState::default);
        self.user_data().get::<GhostState>().unwrap().begin(size);
    }

    /// Stop ghosting; the window is no longer retained by `IsAlive`.
    pub fn end_ghost(&self) {
        if let Some(ghost) = self.ghost_state() {
            ghost.end();
        }
    }

    pub fn set_ssd(&self, ssd: bool) {
        self.decoration_state().is_ssd = ssd;
    }

    pub fn is_ssd(&self) -> bool {
        self.decoration_state().is_ssd
    }

    /// Mark a freshly mapped window so its open transition starts as soon as it
    /// has content to show.
    pub fn begin_open(&self) {
        let mut state = self.decoration_state();
        if state.visibility.animation.is_none() && !state.visibility.closing {
            state.visibility.open_pending = true;
        }
    }

    /// Request that the window close: it fades and grows out, and the client is
    /// told to close once the transition completes.
    pub fn begin_close(&self) {
        let mut state = self.decoration_state();
        if state.visibility.closing {
            return;
        }
        state.visibility.closing = true;
        state.visibility.open_pending = false;
        state.visibility.animation = Some(VisibilityAnimation::close());
        // Drop the titlebar hover highlight so it doesn't linger (and fade as a
        // rectangle) while the window closes.
        state.header_bar.pointer_loc = None;
    }

    /// Whether a close has been requested for this window.
    pub fn is_closing(&self) -> bool {
        self.decoration_state().visibility.closing
    }

    /// The opacity and center scale the window should be drawn with right now.
    /// Returns `None` when no open/close transition is active.
    pub fn visibility(&self) -> Option<(f32, f64)> {
        let state = self.decoration_state();
        state
            .visibility
            .animation
            .as_ref()
            .map(|animation| animation.sample(Instant::now()))
    }

    /// The last committed buffer of the whole window, if any.
    pub fn last_frame(&self) -> Option<LastFrame> {
        self.decoration_state().last_frame.clone()
    }

    /// Record the buffer the client just committed, keeping it alive so an
    /// app-triggered close can be rendered from it.
    pub fn set_last_frame(&self, frame: LastFrame) {
        self.decoration_state().last_frame = Some(frame);
    }

    pub fn is_maximized(&self) -> bool {
        if self.is_ghosting() {
            return false;
        }
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

    /// Whether this window currently holds the seat's focus, used to tint the
    /// decorations. The active element is tracked by [`Space`] via
    /// `SpaceElement::set_activate`.
    pub fn is_activated(&self) -> bool {
        if self.is_ghosting() {
            return false;
        }
        match self.0.underlying_surface() {
            WindowSurface::Wayland(w) => {
                w.with_pending_state(|state| state.states.contains(xdg_toplevel::State::Activated))
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(w) => w.is_activated(),
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
        // A ghost has no live surface to query.
        if let Some(size) = self.ghost_size() {
            return size;
        }
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
        // Resolve everything that reads the decoration state before borrowing
        // it, so the two borrows can't overlap.
        let is_ssd = self.is_ssd();
        let size = self.geometry().size;
        let maximized = self.is_maximized();
        let state = self.decoration_state();
        if state.header_bar.fullscreen {
            return None;
        }
        // A plain maximized window (not part of a snap group) has no grip. A
        // snapped window keeps its grip, because dragging it is what drives the
        // compositor-owned group resize.
        if maximized && state.snap_zone.is_none() {
            return None;
        }
        if is_ssd {
            state.header_bar.resize_edge(point, size)
        } else {
            // CSD windows in a snap group get an invisible compositor edge band
            // so a drag on their border starts the unified group resize (the
            // client, told it is maximized, won't resize itself).
            state.snap_zone?;
            resize_edge_band(point, size)
        }
    }
}

impl<B: Backend> AnvilState<B> {
    pub fn end_ssd_drag(&mut self) {
        if let Some(drag) = self.ssd_drag.take() {
            // Keep any in-flight restore transition at the drop position, then
            // hand the window back to the animation before tiling.
            self.pin_window_animation(&drag.window);
            self.dragging_window = None;
            // The preview is only meaningful while the button is held.
            self.clear_snap_preview();
            if let Some(target) = drag.snap_target {
                // Deferred: this may run while the window's decoration state is
                // borrowed (e.g. from the touch/tablet target handlers), and
                // `apply_snap` needs to borrow it.
                let window = drag.window;
                self.handle
                    .insert_idle(move |data| data.apply_snap(&window, &target));
            }
            tracing::debug!("SSD drag ended");
        }
    }

    /// Move the window being dragged so it tracks the pointer's current global
    /// position. `global` is the authoritative pointer position in compositor
    /// space, so this keeps working no matter which surface the cursor is over.
    pub fn update_ssd_drag_position(&mut self, global: Point<f64, Logical>) {
        let Some(mut drag) = self.ssd_drag.clone() else {
            return;
        };
        if self.space.element_location(&drag.window).is_none() {
            self.ssd_drag = None;
            self.clear_snap_preview();
            return;
        }

        // A tiled window pops back to its floating size as soon as the drag
        // actually moves. Done here (not at drag start) because `start_drag`
        // runs while the window's decoration state is already borrowed.
        let snap_restore = drag.window.decoration_state().header_bar.snap_restore.take();
        if let Some(rel) = snap_restore
            && let Some((_, content)) = super::absolute_geometry(&self.space, &drag.window, rel)
        {
            let decorated_size = self
                .space
                .element_geometry(&drag.window)
                .map(|geo| geo.size)
                .unwrap_or_default();
            let restored = super::xdg::restore_drag_location(
                drag.start_origin,
                decorated_size,
                drag.start_global,
                Some(content),
                drag.window.is_ssd(),
            );
            self.restore_snapped(&drag.window, restored, content);
            drag.start_origin = restored;
            if let Some(active) = self.ssd_drag.as_mut() {
                active.start_origin = restored;
            }
        }

        let delta = global - drag.start_global;
        let new_origin = drag.start_origin + delta.to_i32_round();
        let new_origin =
            super::clamp_window_position(&self.space, &drag.window, global, new_origin);
        self.space.map_element(drag.window.clone(), new_origin, true);

        // Track the snap target under the pointer and show its preview.
        let target = self.snap_zone_at(global, Some(&drag.window));
        self.note_snap_target(target.clone());
        if let Some(active) = self.ssd_drag.as_mut() {
            active.snap_target = target;
        }

        tracing::trace!(?global, ?new_origin, "SSD drag moved window");
    }

    /// Record the start of an SSD resize and return the initial window location,
    /// content size, and global pointer location used to drive the grab.
    fn prepare_ssd_resize(
        &mut self,
        window: &WindowElement,
        edges: ResizeEdge,
        window_relative_location: Point<f64, Logical>,
    ) -> Option<(
        Point<i32, Logical>,
        Size<i32, Logical>,
        Point<f64, Logical>,
        Option<Rectangle<i32, Logical>>,
    )> {
        if !window.alive() || edges.is_empty() {
            return None;
        }
        // An interactive resize takes over from any in-flight transition. A
        // snapped window stays in its group while resized, so `snap_restore`
        // (and the restore icon) is kept; only moving breaks the snap.
        window.decoration_state().animation = None;
        let snap_area = self.snap_area_for(window);
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

        Some((
            initial_window_location,
            initial_window_size,
            pointer_location,
            snap_area,
        ))
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
        let Some((initial_window_location, initial_window_size, pointer_location, snap_area)) =
            self.prepare_ssd_resize(&window, edges, window_relative_location)
        else {
            return;
        };

        let mut resize = ResizeGrabState::new(
            window,
            edges,
            initial_window_location,
            initial_window_size,
            pointer_location,
        );
        resize.set_snap_area(snap_area);
        let grab = PointerResizeSurfaceGrab {
            start_data: PointerGrabStartData {
                focus: None,
                button,
                location: pointer_location,
            },
            resize,
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
        let Some((initial_window_location, initial_window_size, pointer_location, snap_area)) =
            self.prepare_ssd_resize(&window, edges, window_relative_location)
        else {
            return;
        };

        let mut resize = ResizeGrabState::new(
            window,
            edges,
            initial_window_location,
            initial_window_size,
            pointer_location,
        );
        resize.set_snap_area(snap_area);
        let grab = TouchResizeSurfaceGrab {
            start_data: TouchGrabStartData {
                focus: None,
                slot,
                location: pointer_location,
            },
            resize,
        };

        let seat = self.seat.clone();
        self.handle.insert_idle(move |data| {
            if let Some(touch) = seat.get_touch() {
                touch.set_grab(data, grab, serial);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_anchor_adapts_to_committed_size() {
        // Grabbing the titlebar at 1/4 of a 1000px-wide maximized window, 15px
        // down from the top.
        let loc = Point::from((100, 50));
        let size = Size::from((1000, 600));
        let grab = Point::from((350.0, 65.0));
        let anchor = DragAnchor::capture(loc, size, grab);
        assert!((anchor.fraction - 0.25).abs() < 1e-9);
        assert!((anchor.rel_y - 15.0).abs() < 1e-9);

        // The client chose half the width; the pointer must stay at 25% of it.
        let located = anchor.locate(grab, Size::from((500, 400)));
        assert_eq!(located, Point::from((225, 50)));
        assert!((grab.x - (located.x as f64 + 0.25 * 500.0)).abs() < 1.0);
        assert!((grab.y - (located.y as f64 + 15.0)).abs() < 1.0);

        // The window keeps following the pointer, so the anchored spot stays
        // under it as it moves.
        let moved = Point::from((450.0, 165.0));
        let located = anchor.locate(moved, Size::from((500, 400)));
        assert_eq!(located, Point::from((325, 150)));
    }

    #[test]
    fn drag_anchor_zero_width_falls_back_to_center() {
        let anchor = DragAnchor::capture(
            Point::from((0, 0)),
            Size::from((0, 0)),
            Point::from((10.0, 10.0)),
        );
        assert!((anchor.fraction - 0.5).abs() < 1e-9);
    }
}
