use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Renderer,
            element::{
                AsRenderElements, Kind,
                memory::MemoryRenderBuffer,
                solid::{SolidColorBuffer, SolidColorRenderElement},
            },
        },
    },
    desktop::WindowSurface,
    input::Seat,
    utils::{Logical, Physical, Point, Scale, Serial, Size, Transform},
    wayland::shell::xdg::XdgShellHandler,
};

use std::cell::{RefCell, RefMut};

use crate::{AnvilState, state::Backend};

use super::WindowElement;

pub struct WindowState {
    pub is_ssd: bool,
    pub fullscreen_restore: Option<(Point<i32, Logical>, Size<i32, Logical>)>,
    pub header_bar: HeaderBar,
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

    pub fn pointer_enter(&mut self, loc: Point<f64, Logical>) {
        self.pointer_loc = Some(loc);
    }

    pub fn pointer_leave(&mut self) {
        self.pointer_loc = None;
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
                match window.0.underlying_surface() {
                    WindowSurface::Wayland(w) => w.send_close(),
                    #[cfg(feature = "xwayland")]
                    WindowSurface::X11(w) => {
                        let _ = w.close();
                    }
                };
            }
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 2)) as f64 => {
                match window.0.underlying_surface() {
                    WindowSurface::Wayland(w) => {
                        let fullscreen = !self.fullscreen;
                        let surface = w.clone();
                        state
                            .handle
                            .insert_idle(move |data| {
                                if fullscreen {
                                    data.fullscreen_request(surface.clone(), None);
                                } else {
                                    data.unfullscreen_request(surface.clone());
                                }
                            });
                    }
                    #[cfg(feature = "xwayland")]
                    WindowSurface::X11(w) => {
                        let surface = w.clone();
                        state
                            .handle
                            .insert_idle(move |data| data.maximize_request_x11(&surface));
                    }
                };
            }
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 3)) as f64 => {
                state.minimize_request(window.clone());
            }
            Some(_) => {
                match window.0.underlying_surface() {
                    WindowSurface::Wayland(w) => {
                        let seat = seat.clone();
                        let toplevel = w.clone();
                        state
                            .handle
                            .insert_idle(move |data| data.move_request_xdg(&toplevel, &seat, serial));
                    }
                    #[cfg(feature = "xwayland")]
                    WindowSurface::X11(w) => {
                        let window = w.clone();
                        state
                            .handle
                            .insert_idle(move |data| data.move_request_x11(&window));
                    }
                };
            }
            _ => {}
        };
    }

    pub fn touch_down<BackendData: Backend>(
        &mut self,
        seat: &Seat<AnvilState<BackendData>>,
        state: &mut AnvilState<BackendData>,
        window: &WindowElement,
        serial: Serial,
    ) {
        match self.pointer_loc.as_ref() {
            Some(loc) if loc.x >= (self.width - BUTTON_WIDTH) as f64 => {}
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 2)) as f64 => {}
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 3)) as f64 => {}
            Some(_) => {
                match window.0.underlying_surface() {
                    WindowSurface::Wayland(w) => {
                        let seat = seat.clone();
                        let toplevel = w.clone();
                        state
                            .handle
                            .insert_idle(move |data| data.move_request_xdg(&toplevel, &seat, serial));
                    }
                    #[cfg(feature = "xwayland")]
                    WindowSurface::X11(w) => {
                        let window = w.clone();
                        state
                            .handle
                            .insert_idle(move |data| data.move_request_x11(&window));
                    }
                };
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
        if !self.pointer_is_in_header() {
            return;
        }
        match self.pointer_loc.as_ref() {
            Some(loc) if loc.x >= (self.width - BUTTON_WIDTH) as f64 => {
                match window.0.underlying_surface() {
                    WindowSurface::Wayland(w) => w.send_close(),
                    #[cfg(feature = "xwayland")]
                    WindowSurface::X11(w) => {
                        let _ = w.close();
                    }
                };
            }
            Some(loc) if loc.x >= (self.width - (BUTTON_WIDTH * 2)) as f64 => {
                match window.0.underlying_surface() {
                    WindowSurface::Wayland(w) => {
                        let fullscreen = !self.fullscreen;
                        let surface = w.clone();
                        state
                            .handle
                            .insert_idle(move |data| {
                                if fullscreen {
                                    data.fullscreen_request(surface.clone(), None);
                                } else {
                                    data.unfullscreen_request(surface.clone());
                                }
                            });
                    }
                    #[cfg(feature = "xwayland")]
                    WindowSurface::X11(w) => {
                        let surface = w.clone();
                        state
                            .handle
                            .insert_idle(move |data| data.maximize_request_x11(&surface));
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
}
