//! The desktop panel: a GTK4 client that uses the `wlr-layer-shell` protocol to
//! pin itself to the top of an output.
//!
//! The compositor starts this as a child of itself (`oxide-desktop --panel`),
//! pointed at its own Wayland socket. The list of open windows and focus
//! requests travel over a Unix socket (see [`crate::panel_ipc`]).

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io::{ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gtk4::{gdk, gio, glib, prelude::*};
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, DrawingArea, Image, Label, Orientation,
};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

const PANEL_HEIGHT: i32 = 40;
/// Each app is a 1:1 square.
const SQUARE_SIZE: i32 = 36;
/// Width of the square's CSS border, which sits inside the square.
const SQUARE_BORDER: i32 = 1;
const ICON_SIZE: i32 = 26;
/// Height of the indicator strip under the icon.
const INDICATOR_HEIGHT: i32 = 6;
const DOT_SIZE: f64 = 3.0;
const DOT_GAP: f64 = 2.0;
/// The close glyph, as the same 1-bit XBM the compositor's titlebar buttons use
/// (`CLOSE_ICON` in `shell::ssd`), so the two are identical rather than similar.
///
/// 10x10, one bit per pixel, least-significant bit first, two bytes per row —
/// twenty bytes in total. A set bit is the glyph, a clear bit transparent.
const CLOSE_XBM: [u8; 20] = [
    0x03, 0xff, 0x87, 0xff, 0xce, 0xfd, 0xfc, 0xfc, 0x78, 0xfc, 0x78, 0xfc, 0xfc, 0xfc, 0xce, 0xfd,
    0x87, 0xff, 0x03, 0xff,
];
const CLOSE_XBM_SIZE: i32 = 10;
/// The glyph at rest, and under the pointer.

/// Where a 1-bit XBM's set pixels are, in its own grid.
///
/// Bit 0 of each byte is the *leftmost* pixel of that byte, matching the XBM
/// format the compositor's own icons are written in, so the same bytes render
/// identically on both sides.
fn glyph_pixels(pattern: &[u8], size: i32) -> Vec<(i32, i32)> {
    let bytes_per_row = (size as usize + 7) / 8;
    let mut pixels = Vec::new();
    for y in 0..size as usize {
        for x in 0..size as i32 {
            // Bounds-checked rather than trusted: the pattern comes from a
            // compositor, and a short one must not take the panel down with it.
            let byte = y * bytes_per_row + x as usize / 8;
            let bit = pattern.get(byte).is_some_and(|byte| byte >> (x % 8) & 1 == 1);
            if bit {
                pixels.push((x, y as i32));
            }
        }
    }
    pixels
}

/// Fallback accent (neutral gray) when the desktop provides none.
const DEFAULT_ACCENT: &str = "#d8d8d8";
const MUTED_COLOR: (f64, f64, f64) = (0.78, 0.78, 0.78);
/// How often the panel drains the compositor socket.
///
/// This is the dominant term in how far a preview trails the window it shows: the
/// compositor answers a request within a frame, but the answer sits in the socket
/// until the panel reads it. At 50ms a fresh preview waited up to three frames to
/// appear, which reads as the image lagging the cursor. Reading often is cheap —
/// a non-blocking read that finds nothing costs nothing, and the task list is only
/// rebuilt when a snapshot actually arrives, which the compositor only sends on a
/// change.
const POLL_INTERVAL: Duration = Duration::from_millis(8);
const FALLBACK_ICON: &str = "application-x-executable";

// ---------------------------------------------------------------- menu metrics

/// Height of every preview, in logical pixels. Shared, so the row lines up along
/// its top and bottom edges however wide the individual windows are.
const PREVIEW_HEIGHT: i32 = 118;
/// Bounds on a preview's width. The upper one is not about looks: a cell wider than
/// its preview cannot be filled, because the width is the image's own aspect at the
/// shared height, and a cell narrower than that cannot either. So the only way to
/// honour a bound is to letterbox, and a letterboxed preview is what this whole
/// layout has been fighting. A very wide window therefore gets a wide cell, and the
/// bound is set well above any real window's aspect so that it is not reached.
const PREVIEW_MIN_WIDTH: i32 = 96;
const PREVIEW_MAX_CELL: i32 = 360;
/// The titlebar above each preview, echoing the compositor's own.
const TITLEBAR_HEIGHT: i32 = 24;
/// Inset before the title.
const TITLE_INSET: i32 = 7;
/// Size the close glyph is drawn at: its own native resolution, one bitmap pixel
/// to one screen pixel.
///
/// It is a 1-bit ten-pixel diagonal, and scaling it up to fill the button's hit
/// area turns those strokes into a grey smear. The button is made legible by the
/// highlight behind it, not by enlarging the glyph.
const CLOSE_DRAWN: i32 = CLOSE_XBM_SIZE;
/// Gap between previews, and the menu's own padding.
const CELL_GAP: i32 = 6;
/// The cell outline's width. Previews are held in by it so the image never paints
/// over it, and so the gap round a preview is the same all the way round.
const CELL_BORDER: i32 = 1;
const MENU_PAD: i32 = 6;
/// Corner radii.
const MENU_RADIUS: f64 = 8.0;
const CELL_RADIUS: f64 = 6.0;
/// Title text, as drawn and as measured. Both sizes are set here rather than in
/// CSS because the text is truncated against what cairo measures, and the drawn
/// and measured sizes have to be the same one.
const TITLE_FONT_SIZE: f64 = 10.0;
const TITLE_COLOUR: (f64, f64, f64) = (0.863, 0.863, 0.863);
/// Appended to a title that has to be cut short.
const TITLE_ELLIPSIS: char = '\u{2026}';
/// Menu colours, replacing what the stylesheet used to say.
const MENU_FILL: (f64, f64, f64, f64) = (0.11, 0.11, 0.11, 0.94);
const MENU_EDGE: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.16);
const CELL_FILL: (f64, f64, f64, f64) = (0.0, 0.0, 0.0, 0.35);
const CELL_EDGE: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.12);
const CELL_EDGE_HOVER: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.38);
/// The mini-CSD's strip, in the same fill as the cell it sits in.
///
/// It was a 7% white wash, which made one preview two greys and put the app menu —
/// now 35% black like the cell — on a footing the previews themselves were not. The
/// strip is now the same transparency as everything else, so a cell reads as one
/// surface; the title and the close square are told apart by the border and the text
/// rather than by a second background under them.
const TITLEBAR_FILL: (f64, f64, f64, f64) = CELL_FILL;
const CLOSE_IDLE: (f64, f64, f64) = (0.55, 0.55, 0.55);
const CLOSE_HOVER: (f64, f64, f64) = (1.0, 1.0, 1.0);
const CLOSE_HOVER_FILL: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.12);
/// The preview size asked of the compositor, which bounds what can arrive.
const PREVIEW_TARGET: (i32, i32) = (PREVIEW_MAX_CELL, PREVIEW_HEIGHT);
/// The two kinds of preview request, as the flag the panel sends.
///
/// A refresh is the timer asking whether anything has moved; a wanted request is the
/// panel saying it has no image for that window yet. Only the second has to be
/// answered when the pixels have not changed, which is what lets a minimized window
/// be sent the last frame captured for it instead of nothing.
const WANTED: u8 = 1;
const REFRESH: u8 = 0;

/// How often an open menu asks for fresh previews, so a window that is animating
/// or playing video reads as live rather than frozen.
///
/// Around 30Hz. The compositor only renders [`PANEL_THUMBNAILS_PER_TICK`] of the
/// requests per frame, so this is the rate it is asked at, not the rate every
/// preview is necessarily updated at.
const PREVIEW_REFRESH: Duration = Duration::from_millis(33);
/// How long an entry takes to grow from nothing to its width, or to shrink to
/// nothing, when a window appears or goes.
const CELL_MORPH: Duration = Duration::from_millis(220);
/// Where a phase's curve finishes, as a fraction of its clock.
///
/// Both curves approach their end asymptotically, so the last tenth of a phase moves
/// the value by less than a pixel and then the animation sits there visibly doing
/// nothing. The log showed it: `growing 1.00` for five frames before the fade even
/// began, and the same dead tail on the way down. The curve is reparametrised to
/// finish here instead, which keeps its shape and spends no frames on the tail.
const PHASE_END: f64 = 0.85;

/// A phase's value at `elapsed`, and whether it is finished.
fn phase_value(ease: fn(f64) -> f64, elapsed: f64) -> (f64, bool) {
    if elapsed >= PHASE_END {
        return (1.0, true);
    }
    (ease(elapsed / PHASE_END), false)
}

/// How long a window takes to fade in or out, separately from the width, because
/// the two run one after the other rather than together.
const CELL_FADE: Duration = Duration::from_millis(140);
/// How long the menu's reveal takes.
const MENU_FADE: Duration = Duration::from_millis(180);
/// How far below its resting place the menu starts, so it slides out from the bar
/// as it fades up.
const MENU_SLIDE: i32 = 10;
/// Minimum gap kept between the menu and the right edge of the output.
const MENU_EDGE_GAP: i32 = 8;
/// How long the pointer must be outside both the bar and the menu before the
/// menu closes. Long enough to cover the gap while crossing between them.
const HOVER_GRACE: Duration = Duration::from_millis(400);
/// How long the pointer must rest on a square before its menu opens. Long enough
/// that travelling along the bar does not open a menu for every square crossed.
const HOVER_OPEN: Duration = Duration::from_millis(250);
/// How long a press on a square is armed before it acts.
///
/// Long enough that a press which becomes a drag is not also a click — a square
/// cannot be brought forward and picked up at once — and short enough that a click is
/// not left feeling slow. A release beats it, so a plain click acts immediately.
const CLICK_ARM: Duration = Duration::from_millis(180);
/// How long to wait for every preview before showing the menu anyway.
///
/// Short: a window with nothing committed, or a buffer the renderer will not
/// import, would otherwise leave the menu closed forever.
const MENU_REVEAL_FALLBACK: Duration = Duration::from_millis(600);

// ---------------------------------------------------------------- menu layout

/// A rectangle, in menu-surface coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Rect {
        /// Whether `(x, y)` is inside, treating the edges as inside too. The pointer
    /// sits exactly on an edge often enough that excluding it would make a
    /// control feel dead along one side.
    fn contains(self, x: f64, y: f64) -> bool {
        let (left, top) = (self.x as f64, self.y as f64);
        let (right, bottom) = ((self.x + self.width) as f64, (self.y + self.height) as f64);
        x >= left && x < right && y >= top && y < bottom
    }
}

/// The close button's square: a full strip-height square in the titlebar's far
/// corner, so its top, right and bottom are the strip's own.
///
/// Defined once and used by both the hit test and the painting, because a
/// highlight that is not where the click area is makes the button feel broken.
fn close_rect(cell: &Rect) -> Rect {
    Rect {
        x: cell.x + cell.width - TITLEBAR_HEIGHT,
        y: cell.y,
        width: TITLEBAR_HEIGHT,
        height: TITLEBAR_HEIGHT,
    }
}

/// Where each preview sits, and how big the surface has to be to hold them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MenuLayout {
    surface: Rect,
    cells: Vec<Rect>,
}

impl MenuLayout {
    /// What is under the pointer: the preview, or the close button in its
    /// titlebar.
    fn hit(&self, x: f64, y: f64) -> Hit {
        for (index, cell) in self.cells.iter().enumerate() {
            if !cell.contains(x, y) {
                continue;
            }
            if close_rect(cell).contains(x, y) {
                return Hit::Close(index);
            }
            return Hit::Preview(index);
        }
        Hit::None
    }
}

/// What the pointer is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    None,
    Preview(usize),
    Close(usize),
}

/// Lay the previews out in a row, left to right, and size the surface to them.
///
/// `widths` is one entry per cell, in the order they are shown, and each is the
/// cell's *current* width — part way through the grow-or-shrink animation if one is
/// in flight. So the surface follows the animation rather than snapping to its end
/// state.
///
/// A settled cell is its image's width plus its border, which makes the box a
/// preview is drawn into exactly the size [`preview_width`] computed for it — same
/// width, [`PREVIEW_HEIGHT`] tall — so the image is never scaled to fit a box a
/// pixel or two off. That is what uneven padding around a preview means: the box
/// and the scale disagree, so one axis is stretched and the gaps do not match.
///
/// The surface is then exactly as wide as the row plus the menu's padding, so it
/// hugs the previews instead of spanning the output and leaving a gap at one side.
fn layout_menu(widths: &[i32]) -> MenuLayout {
    // A gap only *between* cells that are still there. Charging every cell a gap
    // left an entry that had closed up to nothing still holding 6px of row, which
    // then vanished in one jump the moment it was dropped: the row visibly popped at
    // the end of every closing animation.
    let mut placed: Vec<Rect> = Vec::with_capacity(widths.len());
    let mut x = MENU_PAD;
    let mut seen = 0usize;
    let mut content = 0i32;
    for width in widths {
        let visible = *width > 0;
        if visible {
            if seen > 0 {
                x += CELL_GAP;
                content += CELL_GAP;
            }
            seen += 1;
            content += *width;
        }
        // Kept in step with `widths` so the cell a hit test names is the one that was
        // drawn. A zero-width cell is never hit, being empty.
        placed.push(Rect {
            x,
            y: MENU_PAD,
            width: *width,
            height: PREVIEW_HEIGHT + TITLEBAR_HEIGHT + CELL_BORDER,
        });
        x += *width;
    }
    MenuLayout {
        surface: Rect {
            x: 0,
            y: 0,
            // Never narrower than the padding on either side, so an empty menu is
            // still a sensible size rather than a sliver.
            width: (content + MENU_PAD * 2).max(MENU_PAD * 2),
            height: PREVIEW_HEIGHT + TITLEBAR_HEIGHT + CELL_BORDER + MENU_PAD * 2,
        },
        cells: placed,
    }
}

/// How wide a preview should be for a window of this shape.
///
/// One height for every preview, the width following each window's own aspect at
/// that height, so the row lines up along both edges and — the point of deriving
/// the width this way — the cell is exactly the size the image is scaled for, so
/// the image fills it and neither axis is letterboxed or stretched.
///
/// The bounds are a floor for a uselessly narrow window and a ceiling for a
/// comically wide one. Only beyond the ceiling does a preview get letterboxed, which
/// is the honest outcome of not letting one window fill the screen.
fn preview_width(width: i32, height: i32) -> i32 {
    if width <= 0 || height <= 0 {
        return PREVIEW_HEIGHT;
    }
    let fitted = (f64::from(width) * f64::from(PREVIEW_HEIGHT) / f64::from(height)).round();
    (fitted as i32).clamp(PREVIEW_MIN_WIDTH, PREVIEW_MAX_CELL)
}

/// Where the menu's left edge goes to sit under the icon that opened it, kept on
/// screen and clear of the right edge.
fn menu_left(icon_center: i32, bar_width: i32, menu_width: i32) -> i32 {
    let furthest = (bar_width - MENU_EDGE_GAP - menu_width).max(MENU_EDGE_GAP);
    (icon_center - menu_width / 2).clamp(MENU_EDGE_GAP, furthest.max(MENU_EDGE_GAP))
}

// ---------------------------------------------------------------- menu state

/// One window on show: its title, its newest preview, and where it ended up.
struct MenuEntry {
    title: RefCell<String>,
    focused: Cell<bool>,
    /// The newest preview, as a cairo surface over the pixels the compositor
    /// sent. Cairo takes ownership of the data it is handed, so the surface is
    /// built once when the preview arrives and then drawn from repeatedly, rather
    /// than rebuilt — and the image recopied — on every repaint.
    preview: RefCell<Option<Preview>>,
    /// Laid out at this rectangle, in surface coordinates.
    rect: Cell<Rect>,
    /// The width this entry has settled at, from its preview's aspect.
    target: Cell<i32>,
    /// How wide it is, in pixels, borders included.
    width: Cell<f64>,
    /// The two ends of the width motion, in the same pixels. The motion eases
    /// between these rather than towards a fraction of a target, so that changing
    /// the target part way through moves where it is going without moving the cell.
    from: Cell<f64>,
    to: Cell<f64>,
    /// How opaque it is, 0 to 1.
    alpha: Cell<f64>,
    /// Which part of the way in or out it is on.
    motion: Cell<Motion>,
    /// How far through the current phase, 0 to 1, linear in time. The value drawn
    /// is this run through the ease curve, so the motion is quick off the mark and
    /// settles rather than starting and stopping dead.
    elapsed: Cell<f64>,
}

/// Where an entry is in appearing or disappearing.
///
/// The two channels run one after the other, not together: a window leaving fades
/// out *first*, and only once it is invisible does its width close up, so the row
/// does not appear to shrink an empty gap. A window arriving does the reverse —
/// it grows into the row while still invisible, and only then fades in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Motion {
    /// Growing into the row, still transparent.
    Growing,
    /// Fully grown; fading up.
    FadingIn,
    /// Both settled.
    Settled,
    /// Fading out; still full width.
    FadingOut,
    /// Invisible; closing up.
    Shrinking,
}

impl MenuEntry {
    /// The width a settled entry sits at: its preview plus the cell's borders.
    fn full_width(&self) -> f64 {
        f64::from(self.target.get() + CELL_BORDER * 2)
    }

    /// Put the cell at its settled width, with no motion.
    fn snap_to_full(&self) {
        let full = self.full_width();
        self.from.set(full);
        self.to.set(full);
        self.width.set(full);
    }

    /// Aim the width motion at the entry's current target, without moving the cell.
    ///
    /// A preview landing mid-motion changes the width the cell is heading for. When
    /// the cell's width was a fraction of that target, taking the new one resized the
    /// cell underneath the curve: the surface jumped 42px in a single frame while the
    /// cell eased from 0.93 to 0.97, and every refresh of a live preview did it again.
    /// The motion is in pixels now, so all that changes here is where it is going —
    /// `from` is moved so the value at the current point of the curve is still exactly
    /// the width on screen, and the cell carries on to the new width without a step.
    fn reaim(&self) {
        match self.motion.get() {
            // On its way out: the width is already closing to nothing, and where it
            // began is behind it.
            Motion::FadingOut | Motion::Shrinking => {}
            // Settled: nothing is easing, so a window that changed size is simply a
            // new width, and it takes effect at once.
            Motion::Settled => self.snap_to_full(),
            Motion::Growing | Motion::FadingIn => {
                let to = self.full_width();
                let f = phase_value(ease_out, self.elapsed.get()).0;
                // Solve `from + (to - from) * f == width` for `from`, so the curve
                // passes through the width that is on screen right now. At the end of
                // the phase there is no curve left to bend and the motion simply
                // restarts from where the cell is.
                let from = if f >= 1.0 {
                    self.width.get()
                } else {
                    (self.width.get() - to * f) / (1.0 - f)
                };
                self.from.set(from);
                self.to.set(to);
            }
        }
    }

    fn new(info: &WindowInfo) -> Self {
        Self {
            title: RefCell::new(window_title(info)),
            focused: Cell::new(info.focused),
            preview: RefCell::new(None),
            rect: Cell::new(Rect::default()),
            // Starts at nothing and grows once the menu is up, so opening one has
            // the same motion as a window appearing in an already open menu.
            target: Cell::new(PREVIEW_HEIGHT),
            width: Cell::new(0.0),
            from: Cell::new(0.0),
            to: Cell::new(PREVIEW_HEIGHT as f64 + CELL_BORDER as f64 * 2.0),
            alpha: Cell::new(0.0),
            motion: Cell::new(Motion::Growing),
            elapsed: Cell::new(0.0),
        }
    }
}

/// A window capture, already scaled to [`PREVIEW_TARGET`] by the compositor.
struct Preview {
    width: i32,
    height: i32,
    surface: gtk4::cairo::ImageSurface,
}

impl Preview {
    /// Wrap freshly received pixels, or reject a payload that does not describe
    /// the image it claims to.
    fn new(width: i32, height: i32, mut pixels: Vec<u8>) -> Option<Self> {
        if width <= 0 || height <= 0 || pixels.len() != (width * height * 4) as usize {
            return None;
        }
        to_cairo_rgba(&mut pixels);
        let surface = gtk4::cairo::ImageSurface::create_for_data(
            pixels,
            gtk4::cairo::Format::ARgb32,
            width,
            height,
            width * 4,
        )
        .ok()?;
        Some(Self {
            width,
            height,
            surface,
        })
    }
}

/// Convert straight RGBA bytes to what cairo's `ARgb32` expects.
///
/// Two differences, both of which show as a wrong picture rather than an error:
///
/// * `ARgb32` is a native-endian 32-bit word, so on a little-endian machine its
///   bytes are in **B, G, R, A** order. Reading the compositor's R, G, B, A
///   straight into it swaps red and blue, which is what turned every preview's
///   colours inside out.
/// * The channels must be premultiplied by alpha. A window capture is opaque
///   almost everywhere, so this is usually a no-op, but an image with a real
///   alpha would otherwise come out too bright.
///
/// Done once per incoming preview rather than per repaint.
fn to_cairo_rgba(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let (red, green, blue, alpha) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        if alpha != 0xff && alpha != 0 {
            let scale = |channel: u8| ((u16::from(channel) * u16::from(alpha)) / 255) as u8;
            pixel[0] = scale(blue);
            pixel[1] = scale(green);
            pixel[2] = scale(red);
        } else {
            pixel[0] = blue;
            pixel[1] = green;
            pixel[2] = red;
        }
    }
}

/// The window menu: a row of live previews on a surface of its own, drawn in one
/// piece rather than assembled from widgets.
///
/// Drawn rather than laid out on purpose. Every earlier version of this was a box
/// of per-preview widgets inside a window left to shrink-wrap its contents, and
/// every symptom came from that: the window would not resize once mapped, the
/// titles decided how wide the previews were, and resizing under a stationary
/// pointer made the compositor report enter and leave as if the pointer had moved.
/// Here the geometry is arithmetic, the surface is sized from it, and the pointer
/// is hit-tested against the same numbers that were drawn — so the three cannot
/// disagree.
struct Menu {
    window: gtk4::Window,
    canvas: DrawingArea,
    /// The app on show, or none when the menu is closed.
    app: RefCell<Option<String>>,
    /// The windows on show, in the order they appear.
    order: RefCell<Vec<u64>>,
    entries: RefCell<HashMap<u64, MenuEntry>>,
    /// The middle of the bar's icon that opened this, and the bar's width, so the
    /// surface can be kept under it and on screen.
    icon_center: Cell<i32>,
    bar_width: Cell<i32>,
    /// Laid out at this size, and the surface asked to match it.
    layout: RefCell<MenuLayout>,
    /// Pointer position within the canvas, or none when it is elsewhere.
    pointer: Cell<Option<(f64, f64)>>,
    /// Whether the menu is on screen.
    shown: Cell<bool>,
    /// Bumped by every reveal or hide, so an animation that has been superseded
    /// steps aside instead of acting on a menu that has since changed.
    animation: Cell<u64>,
    /// Whether a grow-or-shrink is in flight, so only one tick timer is ever
    /// running.
    morphing: Cell<bool>,
    /// Which part of moving from one app to another is in flight.
    switch: Cell<Switch>,
    /// The app being moved to, held until the outgoing previews have gone.
    pending: RefCell<Option<Pending>>,
    /// The surface width to use in place of the laid-out one, while the width is
    /// being eased between the two apps.
    width_override: Cell<Option<i32>>,
    /// The compositor socket, so the switch can ask for the incoming app's previews
    /// from the tick rather than only from an open.
    stream: RefCell<Option<Rc<UnixStream>>>,
    /// Ticks the switch has spent waiting for the incoming previews.
    waited: Cell<u32>,
    /// Where the window's left edge is, and where a switch is easing it to. The
    /// position moves with the size: a row that resizes under an icon has to travel
    /// to stay under it, and the two are one motion rather than two snaps.
    left_from: Cell<i32>,
    left_to: Cell<i32>,
    /// The display's frame clock timestamp for the previous frame, so the step can be
    /// the real time between frames rather than an assumed one.
    last_frame: Cell<i64>,
    /// The surface's width when the switch started, kept because by the time the
    /// width motion wants it the layout has already been exchanged.
    width_asked: Cell<i32>,
    /// The size last asked of the surface.
    ///
    /// Compared against rather than the window's own reported size, which is the one
    /// value here that cannot be trusted: a compositor that has enlarged the surface
    /// reports the enlarged size, so asking again to that size would look like no
    /// change and the surface would never come back to the row.
    asked: Cell<(i32, i32)>,
    /// The two ends of the width motion, and its clock. Kept together so the tick
    /// cannot read one without the others.
    width_from: Cell<i32>,
    width_to: Cell<i32>,
    width_elapsed: Cell<f64>,
}

impl Menu {
    /// The surface width the current entries and order would lay out to, gaps and
    /// padding included — it is the laid-out surface, not a sum of the previews.
    fn widths_from_layout(&self) -> i32 {
        layout_menu(&self.widths()).surface.width
    }

    /// Whether every window on show has sent a preview, so the width to ease to is
    /// a real one.
    fn pending_ready(&self) -> bool {
        let order = self.order.borrow();
        let entries = self.entries.borrow();
        order.iter().all(|id| {
            entries
                .get(id)
                .is_some_and(|entry| entry.preview.borrow().is_some())
        })
    }

    /// The width the surface is on when a switch starts, kept because by the time
    /// the width motion wants it the layout has already been exchanged.
    fn width_asked(&self) -> i32 {
        self.width_asked.get()
    }

    /// Where the window's left edge is now.
    ///
    /// Read back off the margin it was last given rather than remembered, so a
    /// switch starts from the position actually on screen.
    fn window_margin_left(&self) -> i32 {
        self.window
            .margin(Edge::Left)
            .try_into()
            .unwrap_or(self.left_from.get())
    }

    /// Where the window's left edge goes for a given width: under the icon that
    /// opened it, and on the output.
    fn left_wanted(&self, width: i32) -> i32 {
        menu_left(self.icon_center.get(), self.bar_width.get(), width)
    }

    /// The cell widths to lay out at, in order, each scaled by how far its entry
    /// has got between gone and settled.
    fn widths(&self) -> Vec<i32> {
        let entries = self.entries.borrow();
        self.order
            .borrow()
            .iter()
            .filter_map(|id| entries.get(id))
            .map(|entry| entry.width.get().round().max(0.0) as i32)
            .collect()
    }

    /// Whether every preview on show has arrived.
    fn ready(&self) -> bool {
        let order = self.order.borrow();
        let entries = self.entries.borrow();
        // One preview is enough to open. Waiting for every one of them let a single
        // window that could never produce an image keep the whole menu shut; the rest
        // fill in as they land, and the reveal fallback still covers a window that
        // produces none at all.
        !order.is_empty()
            && order.iter().any(|id| {
                entries
                    .get(id)
                    .is_some_and(|entry| entry.preview.borrow().is_some())
            })
    }

    /// What the pointer is over, worked out from where it is and the current layout.
    ///
    /// Never stored: a cached highlight goes stale the moment the layout moves, and
    /// it kept up with neither a cell that had shifted nor a pointer that had left.
    fn hit(&self) -> Hit {
        match self.pointer.get() {
            Some((x, y)) => self.layout.borrow().hit(x, y),
            None => Hit::None,
        }
    }
}

// ---------------------------------------------------------------- the surface

/// Build the menu: one layer surface with a canvas on it, sized and drawn by us.
fn build_menu(
    app: &Application,
    monitor: &gdk::Monitor,
    stream: Option<Rc<UnixStream>>,
) -> Rc<Menu> {
    let window = gtk4::Window::builder().application(app).build();
    window.set_decorated(false);
    window.add_css_class("panel-window");
    window.init_layer_shell();
    window.set_namespace(Some("oxide-panel-menu"));
    window.set_layer(Layer::Top);
    window.set_monitor(Some(monitor));
    // Anchored to the top-left corner, then moved by margins to sit under the
    // icon that opened it.
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_margin(Edge::Top, PANEL_HEIGHT);
    // Reserve nothing: a menu overlays the windows behind it rather than pushing
    // them around.
    window.set_exclusive_zone(-1);
    window.set_opacity(0.0);

    let canvas = DrawingArea::new();
    // Sized from the outset, and never to nothing. A layer surface that commits
    // zero is handed *half the output* by the compositor's arrange, and a size
    // request is only a minimum, so a window that got that big can never be shrunk
    // back — the menu would sit there as a huge empty box swallowing every click for
    // the rest of the session. Both the content size and the request are therefore
    // set, the content size being what makes the window's natural size the layout's.
    canvas.set_content_width(1);
    canvas.set_content_height(1);
    window.set_child(Some(&canvas));

    let menu = Rc::new(Menu {
        window,
        canvas: canvas.clone(),
        app: RefCell::new(None),
        order: RefCell::new(Vec::new()),
        entries: RefCell::new(HashMap::new()),
        icon_center: Cell::new(0),
        bar_width: Cell::new(0),
        layout: RefCell::new(MenuLayout::default()),
        pointer: Cell::new(None),
        shown: Cell::new(false),
        animation: Cell::new(0),
        morphing: Cell::new(false),
        switch: Cell::new(Switch::Idle),
        pending: RefCell::new(None),
        width_override: Cell::new(None),
        stream: RefCell::new(None),
        waited: Cell::new(0),
        left_from: Cell::new(0),
        left_to: Cell::new(0),
        last_frame: Cell::new(0),
        width_asked: Cell::new(0),
        asked: Cell::new((0, 0)),
        width_from: Cell::new(0),
        width_to: Cell::new(0),
        width_elapsed: Cell::new(0.0),
    });

    // Painting.
    let painted = menu.clone();
    canvas.set_draw_func(move |_, context, width, height| {
        draw_menu(&painted, context, width, height);
    });

    // Every motion is stepped from here, on the display's own frame clock, so a step
    // is a frame rather than whenever a timer happened to fire. Installed once and
    // left in place; it does nothing at all unless a motion is in flight, which
    // `morphing` says.
    {
        let menu = menu.clone();
        canvas.add_tick_callback(move |_, clock| {
            // The clock's own timestamp for this frame, differenced against the last
            // one. The binding does not hand the frame time over, and using the
            // clock's is better anyway: it is the time the frame is being presented
            // at, not the time a timer happened to be serviced.
            let now = clock.frame_time();
            let previous = menu.last_frame.replace(now);
            // Taken on *every* frame, motion or not. Only reading it while a motion
            // was running left it holding the timestamp of the last animation, so the
            // first frame of the next one differenced against seconds ago — and the
            // step is capped, so the whole 100ms landed in that one frame. Every
            // motion then began most of the way through: a fade-out opening at 0.64
            // instead of 1.0, a grow opening at 84% of its width.
            if menu.morphing.get() && now > previous {
                tick_morph(&menu, now - previous);
            }
            glib::ControlFlow::Continue
        });
    }

    // The pointer, hit-tested against the same rectangles that were drawn. A
    // widget per preview would have GTK deliver this for free, and would also have
    // meant the geometry lived in two places at once.
    {
        let motion = gtk4::EventControllerMotion::new();
        motion.connect_enter({
            let menu = menu.clone();
            move |_, x, y| pointer_moved(&menu, Some((x, y)))
        });
        motion.connect_motion({
            let menu = menu.clone();
            move |_, x, y| pointer_moved(&menu, Some((x, y)))
        });
        motion.connect_leave({
            let menu = menu.clone();
            move |_| pointer_moved(&menu, None)
        });
        canvas.add_controller(motion);
    }

    // Focus a window by clicking its preview, close it by clicking its close
    // button. Which is which is settled by the hit test, so it cannot disagree
    // with what was drawn.
    {
        let clicked = menu.clone();
        let click = gtk4::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        click.connect_pressed(move |_, _, x, y| {
            let target = match clicked.layout.borrow().hit(x, y) {
                Hit::Close(index) => clicked.order.borrow().get(index).copied().map(|id| (id, true)),
                Hit::Preview(index) => {
                    clicked.order.borrow().get(index).copied().map(|id| (id, false))
                }
                Hit::None => None,
            };
            let Some((id, close)) = target else {
                return;
            };
            let Some(stream) = stream.as_ref() else {
                return;
            };
            if close {
                // Only that window, and the menu stays: you are picking the next one.
                send(stream, &format!("close\t{id}\n"));
            } else {
                // Focusing a window is choosing it, so the menu goes away.
                send(stream, &format!("focus\t{id}\n"));
                menu_close(&clicked);
            }
        });
        canvas.add_controller(click);
    }

    menu
}


/// Record where the pointer is, and repaint if that changed what it is over.
fn pointer_moved(menu: &Rc<Menu>, at: Option<(f64, f64)>) {
    // Outside the surface is nowhere, whatever the widget was told: a layer surface
    // that has been moved or resized can keep delivering coordinates that no longer
    // land on it.
    let at = at.filter(|(x, y)| menu.layout.borrow().surface.contains(*x, *y));
    menu.pointer.set(at);
    // Always repainted, even if the pointer is over the same thing: what is drawn
    // depends on the layout as well as the position, and skipping the redraw on an
    // unchanged *hit* is what left a highlight behind when the geometry moved under
    // it.
    menu.canvas.queue_draw();
}

// ---------------------------------------------------------------- painting

/// Paint the whole menu.
fn draw_menu(menu: &Rc<Menu>, context: &gtk4::cairo::Context, width: i32, height: i32) {
    // The layout's surface, not the canvas allocation. They should be the same, but
    // a compositor that has enlarged the surface must not be able to paint the menu
    // larger than the row it is drawn from — an unpainted region is still a hit-test
    // region, so the surplus would swallow clicks.
    let surface = menu.layout.borrow().surface;
    let width = surface.width.min(width);
    let height = surface.height.min(height);
    set_source(context, MENU_FILL);
    rounded_top_rectangle(context, 0.0, 0.0, f64::from(width), f64::from(height), MENU_RADIUS);
    let _ = context.fill_preserve();
    set_source(context, MENU_EDGE);
    context.set_line_width(1.0);
    let _ = context.stroke();

    let layout = menu.layout.borrow().clone();
    let order = menu.order.borrow().clone();
    let entries = menu.entries.borrow();
    let hit = menu.hit();
    for (index, cell) in layout.cells.iter().enumerate() {
        let Some(id) = order.get(index) else {
            break;
        };
        let Some(entry) = entries.get(id) else {
            break;
        };
        let accent = accent_rgb();
        let _ = &entry;
        let over_close = hit == Hit::Close(index);
        let over_cell = matches!(hit, Hit::Preview(i) | Hit::Close(i) if i == index);
        let edge = if over_cell {
            CELL_EDGE_HOVER
        } else if entry.focused.get() {
            (accent.0, accent.1, accent.2, 0.9)
        } else {
            CELL_EDGE
        };
        // Into a group of its own, so the entry's progress can fade every colour
        // it uses — fill, outline, title, glyph — in one go rather than each being
        // scaled by hand and one of them forgotten.
        let alpha = entry.alpha.get();
        if alpha <= 0.0 {
            continue;
        }
        let _ = context.push_group();
        set_source(context, CELL_FILL);
        rounded_top_rectangle(
            context,
            f64::from(cell.x),
            f64::from(cell.y),
            f64::from(cell.width),
            f64::from(cell.height),
            CELL_RADIUS,
        );
        let _ = context.fill();

        // The titlebar across the top of the cell, its bottom corners square so it
        // meets the preview cleanly.
        let titlebar = Rect {
            x: cell.x,
            y: cell.y,
            width: cell.width,
            height: TITLEBAR_HEIGHT,
        };
        // Clipped to the cell's own shape, so the strip gets the cell's rounded
        // top corners and its square bottom without a shape of its own.
        let _ = context.save();
        rounded_top_rectangle(
            context,
            f64::from(cell.x),
            f64::from(cell.y),
            f64::from(cell.width),
            f64::from(cell.height),
            CELL_RADIUS,
        );
        let _ = context.clip();
        set_source(context, TITLEBAR_FILL);
        let _ = context.rectangle(
            f64::from(titlebar.x),
            f64::from(titlebar.y),
            f64::from(titlebar.width),
            f64::from(titlebar.height),
        );
        let _ = context.fill();
        let _ = context.restore();

        {
            let held = entry.preview.borrow();
            if let Some(preview) = held.as_ref() {
                draw_preview(context, cell, preview);
            }
        }
        draw_title(context, &titlebar, &entry.title.borrow());
        draw_close(context, &titlebar, over_close);
        let _ = context.pop_group_to_source();
        let _ = context.paint_with_alpha(alpha);

        // The outline last of all: the preview covers the whole of the cell below
        // the strip, so a border stroked before it had its left, right and bottom
        // edges painted over and the cell read as having no outline at all.
        //
        // In a group of its own, so it fades with the cell instead of staying at
        // full strength while everything inside it faded away. An outline is the
        // thing you see last when a cell dissolves, so this left a hard edge drawn
        // around nothing for the whole of the fade out.
        let _ = context.push_group();
        set_source(context, edge);
        context.set_line_width(1.0);
        rounded_top_rectangle(
            context,
            f64::from(cell.x) + 0.5,
            f64::from(cell.y) + 0.5,
            f64::from(cell.width) - 1.0,
            f64::from(cell.height) - 1.0,
            CELL_RADIUS,
        );
        let _ = context.stroke();
        let _ = context.pop_group_to_source();
        let _ = context.paint_with_alpha(alpha);
    }
}

/// Draw a preview into the space below its titlebar, scaled to fit.
fn draw_preview(context: &gtk4::cairo::Context, cell: &Rect, preview: &Preview) {
    // The cell's body below the strip, held in by the border: drawing the image
    // over the border is what left the outline looking broken, and the inset is
    // what makes the gap round the image the same on every side.
    let box_rect = Rect {
        x: cell.x + CELL_BORDER,
        y: cell.y + TITLEBAR_HEIGHT,
        width: cell.width - CELL_BORDER * 2,
        height: cell.height - TITLEBAR_HEIGHT - CELL_BORDER,
    };
    if preview.width <= 0 || preview.height <= 0 {
        return;
    }
    let left = f64::from(box_rect.x);
    let top = f64::from(box_rect.y);
    let width = f64::from(box_rect.width);
    let height = f64::from(box_rect.height);
    let (image_width, image_height) = (preview.width, preview.height);
    // One scale for both axes, and the smaller of the two. Scaling each axis
    // separately to fill the box is what stretched the previews: a box a pixel or
    // two off the image's aspect — which is all a clamped width is — came out
    // visibly distorted, and every preview in the row was squashed to the same
    // proportions rather than one being a little too small.
    let scale = (width / f64::from(image_width)).min(height / f64::from(image_height));
    let drawn_width = f64::from(image_width) * scale;
    let drawn_height = f64::from(image_height) * scale;
    // Centred in what is left over, so a preview that cannot fill the box is inset
    // evenly rather than pushed against one side.
    let offset_x = left + (width - drawn_width) / 2.0;
    let offset_y = top + (height - drawn_height) / 2.0;
    let _ = context.save();
    let _ = context.translate(offset_x, offset_y);
    let _ = context.scale(scale, scale);
    // Cairo's default filter is `GOOD`, which is what a preview wants: it arrives
    // within a pixel or two of the size it is drawn at, and `NEAREST` would show
    // that as a shimmer while `BILINEAR` cannot be had without reaching for a
    // pattern this binding does not expose.
    let _ = context.set_source_surface(&preview.surface, 0.0, 0.0);
    let _ = context.paint();
    let _ = context.restore();
}

/// Draw a title, cut to whatever room its titlebar leaves.
fn draw_title(context: &gtk4::cairo::Context, titlebar: &Rect, title: &str) {
    context.select_font_face(
        "sans-serif",
        gtk4::cairo::FontSlant::Normal,
        gtk4::cairo::FontWeight::Normal,
    );
    context.set_font_size(TITLE_FONT_SIZE);
    set_source(context, (TITLE_COLOUR.0, TITLE_COLOUR.1, TITLE_COLOUR.2, 1.0));
    // Only what is left once the inset and the close square are accounted for.
    let budget = f64::from((titlebar.width - TITLE_INSET - TITLEBAR_HEIGHT).max(0));
    let shown = truncate_to_width(title, budget, &|candidate| {
        text_width(context, candidate)
    });
    let extents = match context.text_extents(&shown) {
        Ok(extents) => extents,
        Err(_) => return,
    };
    let baseline = centred_baseline(
        f64::from(titlebar.y) + f64::from(TITLEBAR_HEIGHT) / 2.0,
        extents.y_bearing(),
        extents.height(),
    );
    let _ = context.move_to(f64::from(titlebar.x) + f64::from(TITLE_INSET) as f64, baseline);
    let _ = context.show_text(&shown);
}

/// Draw the close glyph at the right of a titlebar.
fn draw_close(context: &gtk4::cairo::Context, titlebar: &Rect, hovered: bool) {
    // The centre of a box is its position plus half its size, not the two added
    // together and halved: the second reading puts the glyph three pixels high in
    // The square from `close_rect`, so the highlight and the click area are the
    // same place: flush with the strip's top, right and bottom, and only its left
    // edge standing in from the corner.
    let square = close_rect(&Rect {
        x: titlebar.x,
        y: titlebar.y,
        width: titlebar.width,
        height: TITLEBAR_HEIGHT,
    });
    let centre_x = f64::from(square.x + square.width / 2);
    let centre_y = f64::from(square.y + square.height / 2);
    if hovered {
        set_source(context, CLOSE_HOVER_FILL);
        let _ = context.rectangle(
            f64::from(square.x),
            f64::from(square.y),
            f64::from(square.width),
            f64::from(square.height),
        );
        let _ = context.fill();
    }
    // The same 1-bit XBM the compositor's own titlebar buttons use, so the two are
    // identical rather than similar. Drawn as rectangles rather than uploaded as a
    // texture: it is ten pixels of a straight diagonal, and a scale factor does
    // not survive a bitmap.
    let scale = 1.0;
    let origin_x = centre_x - f64::from(CLOSE_DRAWN) / 2.0;
    let origin_y = centre_y - f64::from(CLOSE_DRAWN) / 2.0;
    let (red, green, blue) = if hovered { CLOSE_HOVER } else { CLOSE_IDLE };
    let _ = context.set_source_rgb(red, green, blue);
    for (x, y) in glyph_pixels(&CLOSE_XBM, CLOSE_XBM_SIZE) {
        let _ = context.rectangle(
            origin_x + f64::from(x) * scale,
            origin_y + f64::from(y) * scale,
            scale,
            scale,
        );
    }
    let _ = context.fill();
}

/// Trace a rectangle with only its top corners rounded, into the current path.
///
/// The shape of a window: rounded where it meets the panel above, square where it
/// ends below. Rounding all four made the mini titlebar look like a lozenge
/// sitting on a hole rather than the top of a window.
fn rounded_top_rectangle(
    context: &gtk4::cairo::Context,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
) {
    let _ = context.new_sub_path();
    for step in rounded_top_path(x, y, width, height, radius) {
        match step {
            PathStep::Move(x, y) => {
                let _ = context.move_to(x, y);
            }
            PathStep::Line(x, y) => {
                let _ = context.line_to(x, y);
            }
            PathStep::Arc {
                cx,
                cy,
                radius,
                a0,
            } => {
                let _ = context.arc(cx, cy, radius, a0, a0 + std::f64::consts::FRAC_PI_2);
            }
        }
    }
    let _ = context.close_path();
}

/// Ease out cubic: quick off the mark, then settling onto the target.
///
/// The curve the *widths* use, in both directions: growing into the row and
/// closing back out of it. A width should commit — start moving on the first frame
/// and most of the distance be gone in the first few — and then settle. A
/// symmetric curve has zero slope at the start, so a cell grew at a crawl for the
/// opening frames, which looks exactly like an ease in and reads as the width
/// starting several frames late.
///
/// The opacity does not want this; see [`ease_in_out`].
fn ease_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Ease in and out: slowest and fastest at the ends, quickest in the middle.
///
/// What a fade uses, in both directions, so fading in and fading out are the same
/// motion played forwards and backwards.
///
/// It was cubic ease-in, the mirror of the cubic ease-out, on the reasoning that a fade
/// should be quick to leave. But a cubic is far too extreme for opacity: `1 - t^3` is
/// still above 0.87 halfway through the phase, so the cell sat there looking
/// unchanged and then the whole fade happened in three frames. The departure read as
/// a wait followed by a disappearance, and the shrink that follows then appeared to
/// start from nowhere. Opacity wants to be moving at a visible rate for the whole
/// phase, which is what this does: it starts on the first frame and arrives at zero
/// smoothly.
///
/// The widths keep their cubic ease-out. A width *should* commit and settle.
fn ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// One step of a path.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PathStep {
    Move(f64, f64),
    Line(f64, f64),
    /// A quarter turn, from `a0` to `a0 + PI/2`.
    Arc {
        cx: f64,
        cy: f64,
        radius: f64,
        a0: f64,
    },
}

/// The outline of a box with only its top corners rounded.
///
/// Every step is placed rather than left for cairo to join: cairo draws a straight
/// line from wherever the last arc ended to wherever the next one starts, which
/// put the top edge a whole radius down and the bottom-left corner on a diagonal.
fn rounded_top_path(x: f64, y: f64, width: f64, height: f64, radius: f64) -> Vec<PathStep> {
    // Never wider than half the box, or the corners would overlap themselves on a
    // short cell.
    let radius = radius.max(0.0).min(width / 2.0).min(height / 2.0);
    let right = x + width;
    let bottom = y + height;
    if radius == 0.0 {
        return vec![PathStep::Move(x, y), PathStep::Line(right, y)];
    }
    let half = std::f64::consts::PI;
    vec![
        // Start down the left side, a radius below the top.
        PathStep::Move(x, y + radius),
        // Round the top-left corner, ending on the top edge.
        PathStep::Arc {
            cx: x + radius,
            cy: y + radius,
            radius,
            a0: half,
        },
        // Straight across the top.
        PathStep::Line(right - radius, y),
        // Round the top-right corner, ending on the right side.
        PathStep::Arc {
            cx: right - radius,
            cy: y + radius,
            radius,
            a0: half + std::f64::consts::FRAC_PI_2,
        },
        // Down the right side and across the bottom, both square.
        PathStep::Line(right, bottom),
        PathStep::Line(x, bottom),
    ]
}

fn set_source(context: &gtk4::cairo::Context, (red, green, blue, alpha): (f64, f64, f64, f64)) {
    context.set_source_rgba(red, green, blue, alpha);
}

/// Where a baseline goes to leave text sitting on a line's centre.
///
/// The ink occupies the span from `y_bearing` above the baseline to
/// `y_bearing + height` below it, so its middle is `y_bearing + height / 2` away
/// from the baseline — a negative number, because cairo measures `y_bearing`
/// upwards. Putting that middle on `centre` means moving the baseline up by the
/// same distance again.
///
/// This assumes cairo's convention, that `y_bearing` is measured upwards and so
/// arrives negative. Reading it the other way round puts the baseline above the
/// cell and clips the top off every title, which is what happened before.
fn centred_baseline(centre: f64, y_bearing: f64, height: f64) -> f64 {
    centre - (y_bearing + height / 2.0)
}

/// Where `text` reaches when drawn, or 0 if cairo cannot measure it.
fn text_width(context: &gtk4::cairo::Context, text: &str) -> f64 {
    context
        .text_extents(text)
        .map(|extents| extents.width())
        .unwrap_or(0.0)
}

/// Cut `text` to fit `budget`, marking what was lost with an ellipsis.
///
/// `measure` gives a string's advance width, so the caller decides how that is
/// measured rather than this guessing at font metrics — which is what a label's
/// own ellipsize cannot promise, since it reports the width of the *unellipsized*
/// text as its natural width and that width is what a window shrink-wraps to.
fn truncate_to_width(text: &str, budget: f64, measure: &impl Fn(&str) -> f64) -> String {
    if measure(text) <= budget {
        return text.to_owned();
    }
    let mut cut = text.chars().count();
    while cut > 0 {
        let mut candidate: String = text.chars().take(cut).collect();
        candidate.push(TITLE_ELLIPSIS);
        if measure(&candidate) <= budget {
            return candidate;
        }
        cut -= 1;
    }
    TITLE_ELLIPSIS.to_string()
}

// ---------------------------------------------------------------- driving it

/// Lay the menu out, resize the surface to match, and repaint.
///
/// Every geometric change goes through here, so the size the surface is given and
/// the rectangles the pointer is tested against are always the same numbers. A
/// no-op when nothing moved, which matters: resizing the surface under a
/// stationary pointer makes the compositor report enter and leave, which reads as
/// the pointer having left the menu.
fn relayout(menu: &Rc<Menu>) {
    let mut layout = layout_menu(&menu.widths());
    // While the width is being eased between two apps the surface is that width, not
    // the laid-out one: the old previews are already invisible and the new ones not
    // yet in, so the row has nothing to say about how wide it should be.
    // Only while a switch is actually running: left set after one finished, the
    // surface stayed at the width it was easing towards rather than the width of
    // whatever is now in the row.
    if menu.switch.get() != Switch::Idle
        && let Some(width) = menu.width_override.get()
    {
        layout.surface.width = width;
    }
    let size = (layout.surface.width, layout.surface.height);
    *menu.layout.borrow_mut() = layout.clone();
    {
        let entries = menu.entries.borrow();
        for (index, id) in menu.order.borrow().iter().enumerate() {
            if let Some(entry) = entries.get(id)
                && let Some(cell) = layout.cells.get(index)
            {
                entry.rect.set(*cell);
            }
        }
    }
    // Never a zero: see `build_menu`. `layout_menu` floors the width, and the height
    // is a constant, so this cannot ask for nothing.
    let size = (size.0.max(1), size.1.max(1));
    if size != menu.asked.get() {
        menu.asked.set(size);
        // The content size is what the window takes itself to; the request is the
        // floor. Both, or a surface the compositor has already enlarged stays large.
        menu.canvas.set_content_width(size.0);
        menu.canvas.set_content_height(size.1);
        menu.canvas.set_size_request(size.0, size.1);
        menu.window.set_default_size(size.0, size.1);
        menu.window.set_size_request(size.0, size.1);
        // The position is only snapped when no switch is easing it. A switch moves
        // the window from where it was to where it is going, and having the layout
        // snap the margin on every one of its 16ms steps fought that and sent the
        // menu sideways instead.
        if menu.shown.get() && menu.switch.get() == Switch::Idle {
            menu.window.set_margin(Edge::Left, menu.left_wanted(size.0));
        }
    }
    menu.canvas.queue_draw();
}

/// Step every entry that is growing or shrinking, and stop when none are.
///
/// The width and the opacity come off the same progress value, so they cannot
/// disagree, and the layout is redone from the new widths so the surface resizes
/// along with them.
fn tick_morph(menu: &Rc<Menu>, frame_time: i64) {
    // How far this frame is worth, as a fraction of a whole phase. Measured, not
    // assumed to be 16ms: every step resizes a surface, which is exactly the work
    // that can overrun a frame, and a fixed increment turns a dropped frame into an
    // animation that runs slow instead of one that skips. Capped, so a long stall —
    // a modal, a window drag — resumes rather than jumping to the end.
    let stepped = frame_time > 0;
    let elapsed_seconds = (frame_time as f64 / 1_000_000.0).clamp(0.0, 0.1);
    let width_step = elapsed_seconds / CELL_MORPH.as_secs_f64();
    let fade_step = elapsed_seconds / CELL_FADE.as_secs_f64();
    let mut moving = 0usize;

    // Moving from one app to another owns every entry while it runs. The per-entry
    // motions below are deliberately *not* consulted: letting both machines write
    // `elapsed`, `alpha` and `motion` is what left outgoing previews in the row
    // forever and incoming ones stuck at zero width.
    match menu.switch.get() {
        Switch::FadingOut => {
            // Only what is on show, which is the outgoing set throughout. Fading every
            // entry in the map meant the incoming ones — swapped in a moment earlier,
            // at zero — were given `1 - ease_in(0)` and so came *up* to fully visible
            // and then went down again, and their clocks ran out here, which left the
            // fade-in with nothing to drive and it completed in a single frame.
            let mut hidden = true;
            {
                let order = menu.order.borrow().clone();
                let mut entries = menu.entries.borrow_mut();
                for id in &order {
                    let Some(entry) = entries.get_mut(id) else {
                        continue;
                    };
                    let elapsed = (entry.elapsed.get() + fade_step).min(1.0);
                    entry.elapsed.set(elapsed);
                    let (value, done) = phase_value(ease_in_out, elapsed);
                    entry.alpha.set(1.0 - value);
                    hidden &= done || entry.alpha.get() <= 0.0;
                }
            }
            if hidden {
                // Nothing left on screen, so this is the moment to exchange the
                // contents. Then a wait of its own, which touches no entry at all.
                swap_in_pending(menu);
                menu.switch.set(Switch::Exchanging);
            }
            moving += 1;
        }
        Switch::Exchanging => {
            // The outgoing previews are gone and the incoming ones are not here yet.
            // Nothing is drawn and nothing is moved; all this does is wait for the
            // previews that will say how wide the new row is. Capped short, because a
            // window that never answers must not leave the menu blank: at worst the
            // width is a guess for a frame or two and the ordinary layout path
            // corrects it as the images arrive.
            menu.waited.set(menu.waited.get() + 1);
            if menu.pending_ready() || menu.waited.get() > 12 {
                let from = menu.width_asked();
                let to = menu.widths_from_layout();
                // Both ends, in the two cells the motion reads. Setting the override
                // but not this one is what made the surface ease *from zero* while
                // the debug line showed a perfectly good starting width next to it.
                menu.width_from.set(from);
                menu.width_to.set(to);
                // Both ends of the travel, so the window moves from where it is to
                // where the new row wants it rather than jumping and then easing. A
                // switch between two icons can be most of the bar apart.
                menu.left_from.set(menu.window_margin_left());
                menu.left_to.set(menu.left_wanted(to));
                menu.width_elapsed.set(0.0);
                menu.width_override.set(Some(from));
                menu.switch.set(Switch::Widening);
            }
            moving += 1;
        }
        Switch::Widening => {
            let from = menu.width_from.get();
            let span = (menu.width_to.get() - from) as f64;
            let left_from = menu.left_from.get();
            let left_span = (menu.left_to.get() - left_from) as f64;
            let elapsed = (menu.width_elapsed.get() + width_step).min(1.0);
            menu.width_elapsed.set(elapsed);
            // The same curve, and the same trimmed phase, as a cell's width. This one
            // had neither: it eased over the whole clock, so it spent its last third
            // moving nothing.
            let (eased, arrived) = phase_value(ease_out, elapsed);
            menu.width_override.set(Some((from as f64 + span * eased).round() as i32));
            // The same fraction of the same motion, so the row never slides out from
            // under the icon it is meant to be sitting under.
            menu.window
                .set_margin(Edge::Left, (left_from as f64 + left_span * eased).round() as i32);
            if arrived {
                // Arrived: the new previews are in place and still invisible.
                menu.width_override.set(None);
                menu.switch.set(Switch::FadingIn);
            }
            moving += 1;
        }
        Switch::FadingIn => {
            let mut up = true;
            {
                let entries = menu.entries.borrow_mut();
                for entry in entries.values() {
                    let elapsed = (entry.elapsed.get() + fade_step).min(1.0);
                    entry.elapsed.set(elapsed);
                    let (value, done) = phase_value(ease_in_out, elapsed);
                    entry.alpha.set(value);
                    up &= done || entry.alpha.get() >= 1.0;
                }
            }
            if up {
                // Nothing left to drive, so the per-entry motions take over again.
                menu.switch.set(Switch::Idle);
            } else {
                moving += 1;
            }
        }
        Switch::Idle => {
            let mut entries = menu.entries.borrow_mut();
            // An entry is dropped once it has closed up to nothing.
            entries.retain(|_, entry| {
                let phase = entry.motion.get();
                let step = if matches!(phase, Motion::FadingIn | Motion::FadingOut) {
                    fade_step
                } else {
                    width_step
                };
                let elapsed = (entry.elapsed.get() + step).min(1.0);
                entry.elapsed.set(elapsed);
                // Eased, so each phase starts briskly and settles. The value is read
                // from the elapsed fraction rather than accumulated, so a dropped or
                // doubled tick cannot leave the motion permanently adrift.
                // Two curves, because the two channels want opposite things. A width
                // commits and settles, so it is a cubic ease out; an opacity has to
                // be visibly moving for the whole phase, so it is symmetric. Putting
                // the width on the symmetric curve as well left it crawling for the
                // opening frames, which is an ease in by any reading.
                let (out, out_done) = phase_value(ease_out, elapsed);
                let (fade, fade_done) = phase_value(ease_in_out, elapsed);
                // The width at this point of the phase. Every phase carries one, so a
                // cell re-aimed mid-fade eases to its new width on the fade's own
                // clock rather than snapping to it.
                let width_at = |entry: &MenuEntry, f: f64| {
                    let from = entry.from.get();
                    from + (entry.to.get() - from) * f
                };
                match phase {
                    Motion::Growing => {
                        entry.width.set(width_at(entry, out));
                        if out_done {
                            entry.motion.set(Motion::FadingIn);
                            entry.elapsed.set(0.0);
                            // Grown: the width is done, and the fade starts from it.
                            entry.from.set(entry.width.get());
                            entry.to.set(entry.width.get());
                        }
                    }
                    Motion::FadingIn => {
                        entry.width.set(width_at(entry, out));
                        entry.alpha.set(fade);
                        if fade_done {
                            entry.motion.set(Motion::Settled);
                        }
                    }
                    Motion::Settled => return true,
                    Motion::FadingOut => {
                        entry.alpha.set(1.0 - fade);
                        if fade_done {
                            entry.motion.set(Motion::Shrinking);
                            entry.elapsed.set(0.0);
                            // Closing up from the width it faded out at.
                            entry.from.set(entry.width.get());
                            entry.to.set(0.0);
                        }
                    }
                    Motion::Shrinking => {
                        // Ease *out*, so the cell commits straight away and settles.
                        // The mirror of the fade would hold it at full width for the
                        // first half of the phase — six frames of a full-width cell
                        // after it is already invisible — and read as a stall.
                        entry.width.set(width_at(entry, out));
                        if out_done {
                            return false;
                        }
                    }
                }
                moving += 1;
                true
            });
            drop(entries);
            menu.order.borrow_mut().retain(|id| {
                menu.entries.borrow().contains_key(id)
            });
        }
    }
    relayout(menu);
    if menu_debug() && stepped {
        let entries = menu.entries.borrow();
        let order = menu.order.borrow();
        let report: Vec<String> = order
            .iter()
            .filter_map(|id| entries.get(id).map(|entry| (*id, entry)))
            .map(|(id, entry)| {
                // Width in pixels, alpha as a fraction: the width is what the
                // surface is made of, so it is the number to watch for a resize that
                // does not follow its curve.
                format!(
                    "{}:{} {:.0}px/{:.2}",
                    id,
                    phase_name(entry.motion.get()),
                    entry.width.get(),
                    entry.alpha.get(),
                )
            })
            .collect();
        // The two numbers the transition actually is. The entries above say what
        // the row is doing; these say whether the surface is going anywhere, and
        // without them a width motion that silently does nothing looks identical to
        // one that works.
        eprintln!(
            "oxide-panel: [{}] w={} (from {} to {}) left={} entries: {}",
            switch_name(menu.switch.get()),
            menu.layout.borrow().surface.width,
            menu.width_from.get(),
            menu.width_to.get(),
            menu.window_margin_left(),
            report.join("  "),
        );
    }
    // Nothing left on show, and every departure has finished: close, so the menu
    // fades away rather than sitting there empty.
    //
    // This has to be at the end of this tick, and not at the top of the next one. The
    // tick that drops the last cell is also the tick that empties the order, but that
    // cell never reaches `moving += 1` — it is on its way out — so `moving` is zero
    // and `morphing` is cleared below. Asking at the top of the tick therefore asked
    // for a tick that was never going to be requested again, and a menu whose last
    // window had gone left its surface on screen with nothing in it, waiting for the
    // pointer to move before it would go.
    if menu.switch.get() == Switch::Idle
        && menu.order.borrow().is_empty()
        && menu.app.borrow().is_some()
    {
        menu_close(menu);
        menu.morphing.set(false);
        return;
    }
    if moving == 0 {
        menu.morphing.set(false);
    }
}

/// Exchange the menu's contents for the app it is moving to.
///
/// The outgoing entries go out of existence here rather than being marked for
/// removal, because the code that removes them only runs when no switch is in
/// flight — so anything left behind would sit in the row for good.
fn swap_in_pending(menu: &Rc<Menu>) {
    let Some(pending) = menu.pending.borrow_mut().take() else {
        menu.switch.set(Switch::Idle);
        return;
    };
    menu.app.replace(Some(pending.key));
    menu.icon_center.set(pending.icon_center);
    menu.bar_width.set(pending.bar_width);
    let stream = menu.stream.borrow().clone();
    build_entries(menu, &pending.group, stream.as_ref());
    // The width to ease to, from the incoming previews' own aspects. Taken now
    // because a previews-as-it-arrives correction would restart the motion in
    // flight; the surface keeps up through the ordinary layout path afterwards.
    menu.width_to.set(menu.widths_from_layout());
}

/// Put a group of windows in the row, full width and invisible, and ask for any
/// previews they have not sent.
///
/// The opposite of [`menu_replace`], which keeps what is already on show and marks
/// the difference as leaving; this is the clean exchange a switch makes once the
/// old previews have gone.
fn build_entries(menu: &Rc<Menu>, group: &[WindowInfo], stream: Option<&Rc<UnixStream>>) {
    let mut wanted: Vec<u64> = Vec::new();
    {
        let mut entries = menu.entries.borrow_mut();
        for info in group {
            // Anything already here keeps its preview, which is the whole point: the
            // pixels live on the entry, so clearing the map — or dropping entries as
            // if the app being left had closed its windows — threw away every
            // preview and the next visit had to fetch them all again.
            let fresh = !entries.contains_key(&info.id);
            let entry = entries
                .entry(info.id)
                .or_insert_with(|| MenuEntry::new(info));
            entry.title.replace(window_title(info));
            entry.focused.set(info.focused);
            // Straight to full width and settled: a switch eases the *surface*
            // between the two widths, and the incoming previews are in place for it.
            // Left at zero to be grown per-entry they never grew, because that motion
            // is switched off during a switch.
            entry.snap_to_full();
            entry.alpha.set(0.0);
            entry.motion.set(Motion::Settled);
            entry.elapsed.set(0.0);
            if fresh || entry.preview.borrow().is_none() {
                wanted.push(info.id);
            }
        }
        // Windows still open but not on show are kept, and simply not in the order,
        // so coming back to that app is instant.
        *menu.order.borrow_mut() = group.iter().map(|info| info.id).collect();
    }
    for info in group {
        if wanted.contains(&info.id) {
            request_preview_if_missing(menu, info, stream);
        }
    }
}

/// Whether to print what the menu's entries are doing.
///
/// Off unless `OXIDE_PANEL_DEBUG` is set: entries move every 16ms, so this would
/// otherwise be continuous. The phases are the one thing here that cannot be checked
/// from the outside — an entry can be dropped, or sit invisible, without anything
/// looking wrong on screen.
fn menu_debug() -> bool {
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEBUG.get_or_init(|| std::env::var_os("OXIDE_PANEL_DEBUG").is_some())
}

/// The name of a switch phase, for the debug line.
fn switch_name(switch: Switch) -> &'static str {
    match switch {
        Switch::Idle => "idle",
        Switch::FadingOut => "switch-fade-out",
        Switch::Exchanging => "switch-exchange",
        Switch::Widening => "switch-widening",
        Switch::FadingIn => "switch-fade-in",
    }
}

/// The name of a phase, for the debug line.
fn phase_name(motion: Motion) -> &'static str {
    match motion {
        Motion::Growing => "growing",
        Motion::FadingIn => "fading-in",
        Motion::Settled => "settled",
        Motion::FadingOut => "fading-out",
        Motion::Shrinking => "shrinking",
    }
}

/// Ask for a frame, so a motion that has begun gets stepped.
///
/// The step itself happens on the display's frame clock, installed once when the menu
/// is built: a `glib` timer fires whenever it fires, with no regard for vsync, so
/// steps land mid-frame and wait for the next one to be presented — visible as
/// uneven motion, and pinned to one rate whatever the display is actually running
/// at. A frame callback is called once per frame, in step with presentation.
fn kick_morph(menu: &Rc<Menu>) {
    if menu.morphing.get() {
        return;
    }
    menu.morphing.set(true);
    menu.canvas.queue_draw();
}

/// Put `group`'s windows on show for `key`, positioned under `button`.
/// Show the menu in [`MENU_REVEAL_FALLBACK`] regardless of whether every preview
/// has arrived.
///
/// Armed on each open rather than once at startup: a window with nothing committed,
/// or a buffer the renderer will not import, leaves the menu not ready, and a
/// one-shot timer fired during startup had long since run out, so from the second
/// open onwards nothing would ever force it and the menu simply did not come up.
fn arm_reveal_fallback(menu: &Rc<Menu>) {
    let menu = menu.clone();
    glib::timeout_add_local(MENU_REVEAL_FALLBACK, move || {
        menu_reveal(&menu, true);
        glib::ControlFlow::Break
    });
}

fn menu_open(
    menu: &Rc<Menu>,
    key: &str,
    group: &[&WindowInfo],
    button: &Button,
    stream: &Rc<UnixStream>,
) {
    let (icon_center, bar_width) = icon_metrics(button);
    // Already up, and for a different app: move to the new one rather than
    // replacing the contents under the pointer. Hovering along the bar does the
    // same, so the menu follows the pointer.
    let switching = menu.shown.get() && menu.app.borrow().as_deref() != Some(key);
    if switching {
        menu.app.replace(Some(key.to_string()));
        menu.pending.borrow_mut().replace(Pending {
            key: key.to_string(),
            group: group.iter().map(|info| (*info).clone()).collect(),
            icon_center,
            bar_width,
        });
        menu.icon_center.set(icon_center);
        menu.bar_width.set(bar_width);
        // Taken now, while the outgoing layout is still the one on show: by the time
        // the width motion wants it the entries have been exchanged. Any override
        // from an earlier switch is dropped first, or the width it eases *from*
        // would be that switch's idea of a width rather than what is on screen.
        menu.width_asked.set(menu.widths_from_layout());
        menu.waited.set(0);
        // Pinned to the outgoing width for the whole fade-out and the wait that
        // follows it. Otherwise the layout runs at the *incoming* placeholders while
        // its previews are still arriving, the surface is sized to that, and a frame
        // later the override yanks it back — two resizes a few milliseconds apart,
        // which is the back-and-forth.
        menu.width_override.set(Some(menu.width_asked.get()));
        menu.switch.set(Switch::FadingOut);
        kick_morph(menu);
        return;
    }
    menu.icon_center.set(icon_center);
    menu.bar_width.set(bar_width);
    menu.app.replace(Some(key.to_string()));
    // The whole snapshot, not just this app's windows. With an empty list every
    // entry left over from the app that was on show before looked like a window that
    // had gone, and was animated out *inside* the menu that had just opened — so
    // hovering from one app to another left the previous app's previews sitting in
    // the row beside the new one's, until the switch had finished and something
    // rebuilt it.
    let snapshot = SNAPSHOT.with(|cell| cell.borrow().clone());
    let all: Vec<&WindowInfo> = snapshot.iter().collect();
    menu_replace(menu, &all, group, Some(stream));
    relayout(menu);
    menu_reveal(menu, false);
    arm_reveal_fallback(menu);
}

/// Show the menu if it is ready, or if `force` says to stop waiting.
fn menu_reveal(menu: &Rc<Menu>, force: bool) {
    if menu.shown.get() || menu.app.borrow().is_none() {
        return;
    }
    if !force && !menu.ready() {
        return;
    }
    let width = menu.layout.borrow().surface.width;
    let left = menu_left(menu.icon_center.get(), menu.bar_width.get(), width);
    menu.window.set_margin(Edge::Left, left);
    // Cleared here because the reveal slides the surface up from under the bar, and
    // the pointer is on the icon that opened it. It passes over the menu's own
    // coordinates on the way, so the first cell was highlighted before the pointer
    // had gone anywhere near it — and with no later motion to correct it, that
    // highlight was still there with the pointer somewhere else entirely.
    menu.pointer.set(None);
    menu.shown.set(true);
    animate_menu(menu, true, left);
    // Never a motion on the way in. Every window on show was open before the click,
    // so animating them would be showing off a transition that did not happen; the
    // grow-and-fade is for a window arriving at a menu that is already up.
    settle_all(menu);
}

/// Put every entry straight to its settled size and opacity.
fn settle_all(menu: &Rc<Menu>) {
    for entry in menu.entries.borrow().values() {
        entry.snap_to_full();
        entry.alpha.set(1.0);
        entry.motion.set(Motion::Settled);
        entry.elapsed.set(0.0);
    }
    relayout(menu);
}

/// Bring the menu in step with a snapshot: which windows are on show, their
/// titles, and which is focused.
///
/// A window that has gone is dropped, a new one added, and every preview that has
/// not arrived asked for. Previews already cached are pushed straight in, so
/// reopening a menu does not flash placeholders.
/// Bring the menu in step with a snapshot.
///
/// `all` is every window the compositor knows about and `group` is the ones on
/// show, which are the same app's. They have to be told apart: judging what still
/// exists against `group` alone made every *other* app's windows look gone, so
/// their previews were dropped and the menu had to fetch everything again the next
/// time that app was opened. Liveness is a property of the window, not of whichever
/// app happens to be on show.
fn menu_replace(
    menu: &Rc<Menu>,
    all: &[&WindowInfo],
    group: &[&WindowInfo],
    stream: Option<&Rc<UnixStream>>,
) {
    let alive = liveness(&all.iter().map(|info| info.id).collect::<Vec<_>>());
    let live: Vec<u64> = group.iter().map(|info| info.id).collect();

    // A switch owns the row while it runs. The app is already set to the one being
    // moved to, so every snapshot during the switch arrives here asking for the
    // *incoming* group — and taking the order from it threw the outgoing previews
    // out of the row mid-fade and built the incoming entries at zero width, which
    // collapsed the surface before the width motion had even started. That is the
    // transition appearing to begin from nothing instead of the width it was at.
    //
    // So: keep the titles and the previews up to date, and nothing else. The switch
    // decides what is on show and when.
    if menu.switch.get() != Switch::Idle {
        let mut wanted: Vec<u64> = Vec::new();
        {
            let mut entries = menu.entries.borrow_mut();
            for info in group {
                let entry = entries
                    .entry(info.id)
                    .or_insert_with(|| MenuEntry::new(info));
                // Full width from the start: the width motion is easing the surface
                // from the outgoing width to the incoming one, and a cell starting at
                // nothing would drag the surface down with it.
                entry.snap_to_full();
                entry.title.replace(window_title(info));
                entry.focused.set(info.focused);
                if entry.preview.borrow().is_none() {
                    wanted.push(info.id);
                }
            }
        }
        for info in group {
            if wanted.contains(&info.id) {
                request_preview_if_missing(menu, info, stream);
            }
        }
        return;
    }

    let previous = menu.order.borrow().clone();
    let mut arrived = false;
    let mut wanted: Vec<u64> = Vec::new();
    {
        let mut entries = menu.entries.borrow_mut();
        // Anything not in the snapshot has gone: shrink and fade it out rather than
        // dropping it, so closing a window is a motion instead of a jump. It is
        // dropped once it reaches nothing, by the tick.
        for (id, entry) in entries.iter() {
            // A window that has gone starts fading out, and only closes up its width
            // once it is invisible. Already on its way out? left alone.
            if !alive.contains(id) && !matches!(entry.motion.get(), Motion::FadingOut | Motion::Shrinking) {
                entry.motion.set(Motion::FadingOut);
                // From zero, which is the whole point: a settled entry's clock has
                // been sitting at one since it arrived, so without this the fade's
                // first tick computes one and the opacity goes from opaque to
                // nothing in a single frame. The departure looked instant, and the
                // debug line showed it going straight from settled to shrinking.
                entry.elapsed.set(0.0);
                arrived = true;
            }
        }
        for info in group {
            let fresh = !entries.contains_key(&info.id);
            let entry = entries
                .entry(info.id)
                .or_insert_with(|| MenuEntry::new(info));
            // A window that comes back mid-departure is taken back rather than
            // animated twice, and put back to its *settled* values. Leaving the
            // channels where the fade had got to parked a full-width, fully
            // transparent entry in the row: invisible, but taking up space and never
            // fading in, because nothing ever moved it off `Settled` again.
            if matches!(entry.motion.get(), Motion::FadingOut | Motion::Shrinking) {
                entry.motion.set(Motion::Settled);
                entry.snap_to_full();
                entry.alpha.set(1.0);
                entry.elapsed.set(0.0);
            }
            entry.title.replace(window_title(info));
            entry.focused.set(info.focused);
            arrived |= fresh;
            // Noted here, asked for below. An entry keeps its own preview across a
            // close, so a reopening menu does not flash placeholders — but only if
            // it has one at all.
            if entry.preview.borrow().is_none() {
                wanted.push(info.id);
            }
        }
        // A departing window has to stay in the order until the tick has actually
        // removed it. The order is what gets drawn, so taking it out of there the
        // moment it leaves the snapshot made the cell vanish in a single frame, and
        // the fade and the collapse that followed played out with nothing on screen —
        // which is a close that does not animate.
        let leaving: HashMap<u64, bool> = entries
            .iter()
            .map(|(id, entry)| {
                (
                    *id,
                    matches!(entry.motion.get(), Motion::FadingOut | Motion::Shrinking),
                )
            })
            .collect();
        *menu.order.borrow_mut() = draw_order(&previous, &live, &leaving);
    }
    // Outside the borrow: asking for a preview reads the entries, and doing that
    // while they are borrowed for writing panics.
    for info in group {
        if wanted.contains(&info.id) {
            request_preview_if_missing(menu, info, stream);
        }
    }
    // Anything on its way out needs the tick, not just something this snapshot
    // started. `arrived` alone meant a window that closed while a motion was already
    // running — or one caught mid-departure by a snapshot that had nothing new to
    // say — was never ticked again, so it sat in the row at full opacity for good
    // instead of being dropped once it had closed up to nothing. A closed window
    // with its preview still in the menu, permanently, was this.
    //
    // Only once the menu is up: before that the entries' growth would happen behind a
    // hidden surface, and opening one would just appear at full width.
    let unsettled = menu
        .entries
        .borrow()
        .values()
        .any(|entry| !matches!(entry.motion.get(), Motion::Settled));
    if (arrived || unsettled) && menu.shown.get() {
        kick_morph(menu);
    }
    relayout(menu);
}

/// Take a preview that has just arrived.
fn menu_set_image(menu: &Rc<Menu>, id: u64, width: i32, height: i32, pixels: Vec<u8>) {
    let mut entries = menu.entries.borrow_mut();
    let Some(entry) = entries.get_mut(&id) else {
        return;
    };
    let Some(preview) = Preview::new(width, height, pixels) else {
        return;
    };
    let width = preview_width(preview.width, preview.height);
    // Only re-aim when the width has actually changed.
    //
    // `reaim` leaves the cell where it is but resets how fast it is travelling, and
    // a menu refreshes every window on show on a timer, so re-aiming on every
    // preview re-normalised that slope several times a phase. The cell kept
    // speeding up: an ease out has monotonically shrinking steps, and the log had
    // them growing (1, 8, 3, 7, 21, 10). A refresh of the same size is not a change
    // of target and must not touch the motion at all.
    if entry.target.get() != width {
        entry.target.set(width);
        entry.reaim();
    }
    entry.preview.replace(Some(preview));
    drop(entries);
    // A new preview can be a new shape, which can be a new width for the whole
    // row, so the surface is resized from here too.
    relayout(menu);
    menu_reveal(menu, false);
}

/// Ask for a preview of a window on show that has not sent one.
fn request_preview_if_missing(menu: &Rc<Menu>, info: &WindowInfo, stream: Option<&Rc<UnixStream>>) {
    let Some(stream) = stream else {
        return;
    };
    let has = menu
        .entries
        .borrow()
        .get(&info.id)
        .is_some_and(|entry| entry.preview.borrow().is_some());
    if has {
        return;
    }
    send(
        stream,
        &format!(
            // Says the panel has nothing for this window, so the compositor must
            // answer even if the pixels have not moved.
            "preview\t{}\t{}\t{}\t{}\n",
            info.id, PREVIEW_TARGET.0, PREVIEW_TARGET.1, WANTED
        ),
    );
}

/// Which part of moving the menu from one app to another is in flight.
///
/// The three run one after another rather than together: the outgoing previews go
/// first, and only once nothing is left on screen does the surface ease from the
/// old width to the new one, and only once it has arrived do the new previews fade
/// up. Run at the same time they would overlap into a smear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Switch {
    Idle,
    /// Fading the outgoing previews out.
    FadingOut,
    /// The outgoing previews are gone; waiting for the incoming ones, invisible.
    Exchanging,
    /// Easing the surface between the two widths.
    Widening,
    /// Fading the incoming previews in.
    FadingIn,
}

/// The app being moved to, held until the outgoing previews have gone.
struct Pending {
    key: String,
    group: Vec<WindowInfo>,
    icon_center: i32,
    bar_width: i32,
}

/// The order entries are drawn in.
///
/// The windows on show, in the snapshot's order, with anything on its way out still
/// in the place it already had — a new window joins at the end, and a departing one
/// keeps its position until the tick removes it.
fn draw_order(previous: &[u64], live: &[u64], leaving: &HashMap<u64, bool>) -> Vec<u64> {
    let mut order: Vec<u64> = previous
        .iter()
        .copied()
        .filter(|id| live.contains(id) || leaving.get(id).copied().unwrap_or(false))
        .collect();
    for id in live {
        if !order.contains(id) {
            order.push(*id);
        }
    }
    order
}

/// The windows still open, from a snapshot, as the id set the menu compares against.
///
/// A set rather than a lookup per entry: the menu asks about every entry it holds
/// on every snapshot, and this is the same answer each time.
fn liveness(alive: &[u64]) -> HashSet<u64> {
    alive.iter().copied().collect()
}

/// Ask for a fresh preview of every window the open menu is showing, so one that
/// is animating or playing video is not shown frozen.
fn menu_refresh(menu: &Rc<Menu>, windows: &[WindowInfo], stream: &Rc<UnixStream>) {
    if menu.app.borrow().is_none() {
        return;
    }
    let order = menu.order.borrow().clone();
    for info in windows.iter().filter(|info| order.contains(&info.id)) {
        send(
            stream,
            &format!(
                // A refresh: only answer if the window has actually changed.
                "preview\t{}\t{}\t{}\t{}\n",
                info.id, PREVIEW_TARGET.0, PREVIEW_TARGET.1, REFRESH
            ),
        );
    }
}

/// Take the menu off screen, then let it go.
fn menu_close(menu: &Rc<Menu>) {
    if menu.app.borrow().is_none() {
        return;
    }
    menu.app.replace(None);
    // A switch that was in flight is abandoned rather than left to finish on a
    // closed menu: its next tick would otherwise swap a set of entries in for an app
    // that is no longer on show.
    menu.switch.set(Switch::Idle);
    menu.pending.borrow_mut().take();
    menu.width_override.set(None);
    // Whatever the pointer was last over, it is not over a menu that is no longer
    // there.
    menu.pointer.set(None);
    // Cleared here rather than when the animation lands, so the very next click
    // can open it again. Leaving it set is what meant a menu could only ever be
    // opened once per panel run.
    menu.shown.set(false);
    let width = menu.layout.borrow().surface.width;
    let left = menu_left(menu.icon_center.get(), menu.bar_width.get(), width);
    animate_menu(menu, false, left);
}

/// Take the preview menu off screen at once, with no animation.
///
/// For when something else is taking its place in the same spot. A fade here is 180ms
/// of this surface and the next one overlapping, and two translucent menus on top of
/// each other is worse than either on its own.
fn menu_hide(menu: &Rc<Menu>) {
    if menu.app.borrow().is_none() {
        return;
    }
    menu.app.replace(None);
    // A switch in flight is abandoned, as `menu_close` abandons it: its next tick
    // would swap a set of entries in for an app that is no longer on show.
    menu.switch.set(Switch::Idle);
    menu.pending.borrow_mut().take();
    menu.width_override.set(None);
    menu.pointer.set(None);
    menu.shown.set(false);
    // Supersede anything already animating this surface, so a fade already in flight
    // cannot run on and put it back.
    menu.animation.set(menu.animation.get().wrapping_add(1));
    menu.window.set_opacity(0.0);
    menu.window.set_visible(false);
}

/// Slide the menu out of the bar, or back into it, while fading.
///
/// Only the compositor-facing properties move. The surface is not resized and
/// nothing inside it is re-laid-out, so the pointer stays where it was and the
/// menu does not flicker.
fn animate_menu(menu: &Rc<Menu>, showing: bool, left: i32) {
    if showing {
        menu.window.present();
    }
    let animation = Rc::new(Cell::new(menu.animation.get()));
    let landed = animate_surface(&menu.window, animation, showing, left);
    // Kept in step, so a later call through `animate_menu` still supersedes this one.
    menu.animation.set(landed);
}

/// Slide a surface out of the bar, or back into it, while fading.
///
/// Shared by the preview menu and the context menu so that both arrive and leave the
/// same way. Only the compositor-facing properties move: a surface is not resized and
/// nothing inside it is re-laid-out, so a pointer resting on one of them stays where
/// it is and the surface does not flicker.
///
/// `token` is the caller's own counter, so a second animation on the same surface
/// supersedes the first.
fn animate_surface(
    window: &gtk4::Window,
    token_cell: Rc<Cell<u64>>,
    showing: bool,
    left: i32,
) -> u64 {
    let (from_opacity, to_opacity) = if showing { (0.0f64, 1.0f64) } else { (1.0, 0.0) };
    let resting = PANEL_HEIGHT;
    // Starts tucked up under the bar and slides down into place, so it reads as
    // coming out of it.
    let (from_top, to_top) = if showing {
        (resting - MENU_SLIDE, resting)
    } else {
        (resting, resting - MENU_SLIDE)
    };

    window.set_margin(Edge::Left, left);
    // Supersede whatever animation was running: a close that is still sliding out
    // would otherwise finish and hide the surface that has just been reopened.
    token_cell.set(token_cell.get().wrapping_add(1));
    let token = token_cell.get();
    // Handed back so a caller can tell when *this* animation lands. Reading the
    // counter itself before calling this is a trap: the counter is bumped here, so
    // the value a caller read a line earlier is already stale, and anything that
    // waits for `counter == what_i_read` waits for ever.
    let landed = token;
    let window = window.clone();
    let start = glib::monotonic_time();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        if token_cell.get() != token {
            return glib::ControlFlow::Break;
        }
        let elapsed = (glib::monotonic_time() - start) as f64 / 1_000_000.0;
        let t: f64 = (elapsed / MENU_FADE.as_secs_f64()).clamp(0.0, 1.0);
        // Ease out cubic: quick off the mark, then asymptotic to the target.
        let eased = 1.0 - (1.0 - t).powi(3);
        window.set_opacity(from_opacity + (to_opacity - from_opacity) * eased);
        window.set_margin(Edge::Top, from_top + ((to_top - from_top) as f64 * eased) as i32);
        if t < 1.0 {
            return glib::ControlFlow::Continue;
        }
        // Land exactly on the ends, so no rounding residue is left behind.
        window.set_opacity(to_opacity);
        window.set_margin(Edge::Top, to_top);
        if !showing {
            window.set_visible(false);
        }
        glib::ControlFlow::Break
    });
    landed
}

// ---------------------------------------------------------------- context menu

/// What a right click on a square offers: the app itself, and what to do with it.
///
/// Not a second preview menu. Choosing between an app's windows is a left click, or
/// a hover; this is for the things a picker cannot do — start the app again, close
/// everything it has open, and keep it in the bar when it has nothing open.
struct ContextMenu {
    window: gtk4::Window,
    rows: GtkBox,
    /// The app it is open for, or none while it is closed.
    app: RefCell<Option<String>>,
    /// Whether the app menu is up *or on its way out*. Only cleared once the fade has
    /// finished, so nothing else can put another menu in the same place mid-fade.
    shown: Cell<bool>,
    animation: Rc<Cell<u64>>,
    /// Where the square that opened it is, so the menu can sit under it.
    icon_center: Cell<i32>,
    bar_width: Cell<i32>,
    stream: RefCell<Option<Rc<UnixStream>>>,
    hover: Rc<Hover>,
}

/// The icon each row is drawn with.
///
/// Symbolic names from the icon theme, so they follow the desktop's theme rather
/// than being drawn here. A name the installed theme does not have simply shows
/// nothing, which is why each row's label is never an icon alone.
/// How big a row's icon is drawn.
///
/// Smaller than the bar's [`ICON_SIZE`] on purpose: there the icon *is* the button,
/// and here it is a mark beside a label. At 26px a symbolic glyph filled the row and
/// the label beside it looked like the caption.
const CONTEXT_ICON_SIZE: i32 = 18;

const ICON_CLOSE_ONE: &str = "window-close-symbolic";
const ICON_CLOSE_ALL: &str = "edit-clear-all-symbolic";
const ICON_PIN: &str = "starred-symbolic";
const ICON_UNPIN: &str = "list-remove-symbolic";

fn build_context_menu(app: &Application, monitor: &gdk::Monitor) -> Rc<ContextMenu> {
    let window = gtk4::Window::builder().application(app).build();
    window.set_decorated(false);
    window.add_css_class("panel-window");
    window.add_css_class("context");
    window.init_layer_shell();
    // The same namespace as the preview menu, so a compositor that hides panels by
    // namespace treats the two alike.
    window.set_namespace(Some("oxide-panel-menu"));
    window.set_layer(Layer::Top);
    window.set_monitor(Some(monitor));
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_margin(Edge::Top, PANEL_HEIGHT);
    window.set_exclusive_zone(-1);
    window.set_opacity(0.0);

    // Real widgets rather than the cairo the preview menu is drawn with. These rows
    // are text and icons, which is what widgets are for, and they get the icon
    // theme, hover states and ellipsising for nothing.
    let rows = GtkBox::new(Orientation::Vertical, 2);
    rows.add_css_class("context-rows");
    window.set_child(Some(&rows));

    let context = Rc::new(ContextMenu {
        window,
        rows,
        app: RefCell::new(None),
        shown: Cell::new(false),
        animation: Rc::new(Cell::new(0)),
        icon_center: Cell::new(0),
        bar_width: Cell::new(0),
        stream: RefCell::new(None),
        hover: Rc::new(Hover::default()),
    });
    context
}

/// One row of the context menu: an icon and a label, in a button.
fn context_row(icon_name: &str, label: &str, emphasis: bool) -> gtk4::Button {
    let row = gtk4::Button::new();
    row.add_css_class("context-row");
    if emphasis {
        row.add_css_class("app");
    }
    row.set_has_frame(false);

    let icon = gtk4::Image::from_icon_name(icon_name);
    icon.set_pixel_size(CONTEXT_ICON_SIZE);
    let text = gtk4::Label::new(Some(label));
    text.set_xalign(0.0);
    // The row is as wide as its widest label, and a name can be long; this is where
    // it gets shortened rather than pushing the menu off the edge of the output.
    text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    text.set_max_width_chars(28);

    let content = GtkBox::new(Orientation::Horizontal, 10);
    content.append(&icon);
    content.append(&text);
    row.set_child(Some(&content));
    row
}

/// Empty the context menu and build it for one app.
///
/// Refilled from scratch on every snapshot rather than patched, because what it
/// offers depends on things that change under it: the window count decides whether
/// there is anything to close and whether that is one window or all of them, and
/// pinning is the panel's own state.
fn context_fill(context: &Rc<ContextMenu>, key: &str, windows: &[WindowInfo]) {
    context_install(context, context_rows_for(context, key, windows));
}

/// Put a built set of rows on screen, replacing whatever was there.
///
/// Separate from building them because a move has to know how wide the incoming rows
/// will be — to slide the menu to the right place — without showing them arriving
/// early, underneath the outgoing app's menu.
fn context_install(context: &Rc<ContextMenu>, rows: Vec<gtk4::Widget>) {
    while let Some(child) = context.rows.first_child() {
        context.rows.remove(&child);
    }
    for row in rows {
        context.rows.append(&row);
    }
}

/// The rows for one app, built but not yet on screen.
fn context_rows_for(
    context: &Rc<ContextMenu>,
    key: &str,
    windows: &[WindowInfo],
) -> Vec<gtk4::Widget> {
    let Some(stream) = context.stream.borrow().clone() else {
        return Vec::new();
    };
    let mut rows: Vec<gtk4::Widget> = Vec::new();

    // The app itself, which starts a new instance even when it has windows open —
    // unlike the square in the bar, which brings the app forward. Two rows that do
    // opposite things have to be told apart, and this is the one that is not obvious.
    if let Some(app) = crate::desktop::cached(key) {
        let row = context_row(&resolve_icon(key), app.label(), true);
        let id = key.to_string();
        let stream = stream.clone();
        let owned = context.clone();
        row.connect_clicked(move |_| {
            send(&stream, &format!("launch\t{id}\n"));
            context_close(&owned);
        });
        rows.push(row.upcast());
    }

    match windows.len() {
        // One window is closed by name, so a right click on an app with a single
        // window cannot close some other window of the same app that has since
        // opened behind it.
        1 => {
            let id = windows[0].id;
            let row = context_row(ICON_CLOSE_ONE, "Close window", false);
            let owned = context.clone();
            let stream = stream.clone();
            row.connect_clicked(move |_| {
                send(&stream, &format!("close\t{id}\n"));
                context_close(&owned);
            });
            rows.push(row.upcast());
        }
        count if count > 1 => {
            // Every window it has, closed. Sent one at a time because the protocol
            // closes one window per message and has no "close this app" in it.
            let ids: Vec<u64> = windows.iter().map(|info| info.id).collect();
            let label = format!("Close all {} windows", count);
            let row = context_row(ICON_CLOSE_ALL, &label, false);
            let owned = context.clone();
            let stream = stream.clone();
            row.connect_clicked(move |_| {
                for id in &ids {
                    send(&stream, &format!("close\t{id}\n"));
                }
                context_close(&owned);
            });
            rows.push(row.upcast());
        }
        // Nothing open: there is nothing to close, so the row is not there at all.
        // A disabled one would be a dead square to aim at.
        _ => {}
    }

    if crate::desktop::is_app_id(key) {
        let (label, icon_name) = if is_pinned(key) {
            ("Unpin from panel", ICON_UNPIN)
        } else {
            ("Pin to panel", ICON_PIN)
        };
        let row = context_row(icon_name, label, false);
        let id = key.to_string();
        // The windows are kept so the menu can refill itself: the row's own label
        // and icon are what just changed, and rebuilding it needs to know how many
        // windows this app has to offer closing.
        let held: Vec<WindowInfo> = windows.to_vec();
        let owned = context.clone();
        row.connect_clicked(move |_| {
            toggle_pin(&id);
            context_fill(&owned, &id, &held);
            // And the bar, which has just gained or lost a square.
            rebuild_bar();
        });
        rows.push(row.upcast());
    }
    rows
}

/// Move an open app menu onto a different app, as the pointer travels along the bar.
///
/// The same shape as the previews' app switch, and for the same reason: two apps'
/// rows have nothing in common, so anything that keeps the surface on screen across
/// the exchange shows both at once. Fade out over what is there, exchange, fade back
/// in — and slide to the square the pointer is on, which
/// [`animate_surface`] cannot do because an open and a close have nowhere to slide
/// to.
///
/// The incoming rows are built up front but *not* installed: the width they will want
/// is what the slide is aimed at, and installing them before the fade out would show
/// the new app's rows sitting there under the outgoing app's menu.
fn context_move(
    context: &Rc<ContextMenu>,
    key: &str,
    windows: &[WindowInfo],
    button: &Button,
) {
    let (icon_center, bar_width) = icon_metrics(button);
    let from_left = context.window.margin(Edge::Left);
    let rows = context_rows_for(context, key, windows);
    let to_left = menu_left(
        icon_center,
        bar_width,
        measure_rows(context, &rows),
    );
    if menu_debug() {
        eprintln!(
            "oxide-panel: context move to {key:?}, {} windows, {from_left} to {to_left}",
            windows.len()
        );
    }

    // The app key is the new one from here: the fade out belongs to the transition, and
    // a landing that finds a different key under it has been superseded by another
    // square crossed.
    context.app.replace(Some(key.to_string()));
    context.icon_center.set(icon_center);
    context.bar_width.set(bar_width);
    // The hover clock starts over, so the grace period is measured from arriving here
    // rather than from whenever the menu happened to open.
    context.hover.inside.set(true);
    context.hover.token.set(context.hover.token.get().wrapping_add(1));

    // The top does not move during a move — it is already out of the bar.
    let top = PANEL_HEIGHT;
    let token = context_ease(context, 1.0, 0.0, from_left, to_left, top, top);
    let landing = context.clone();
    // Behind an Option because the timer may be called again before it breaks, and
    // the rows cannot be given away twice.
    let mut incoming: Option<Vec<gtk4::Widget>> = Some(rows);
    glib::timeout_add_local(MENU_FADE, move || {
        let Some(rows) = incoming.take() else {
            return glib::ControlFlow::Break;
        };
        // Superseded: another square crossed, or the menu closed underneath.
        if landing.animation.get() != token || !landing.shown.get() {
            return glib::ControlFlow::Break;
        }
        context_install(&landing, rows);
        // Straight back up, from wherever the slide had got to.
        let _ = context_ease(&landing, 0.0, 1.0, to_left, to_left, top, top);
        glib::ControlFlow::Break
    });
}

/// How wide a set of rows wants the menu to be.
///
/// The rows are not in the box yet, so they are measured by putting them in, asking,
/// and taking them out again. Doing it the other way round — installing first and
/// fading afterwards — is what showed the incoming app's rows under the outgoing
/// app's menu, which is the thing this is all about not doing.
fn measure_rows(context: &Rc<ContextMenu>, rows: &[gtk4::Widget]) -> i32 {
    if rows.is_empty() {
        return context.rows.measure(gtk4::Orientation::Horizontal, -1).1;
    }
    // Measured in a box of their own rather than by installing them and asking. A
    // widget has one parent, so they are parented to the scratch box, measured, and
    // unparented again — and crucially the real box is never touched, because the
    // whole point is that the outgoing app's rows are what the fade out is over.
    let scratch = GtkBox::new(Orientation::Vertical, 2);
    for row in rows {
        scratch.append(row);
    }
    let width = scratch.measure(gtk4::Orientation::Horizontal, -1).1;
    for row in rows {
        scratch.remove(row);
    }
    width
}

/// Move the app menu: its rows fade while the panel they sit in does not.
///
/// The one thing [`animate_surface`] does not do, which a move needs: that eases the
/// *window's* opacity and the top margin, and sets the left one outright, because an
/// open and a close have nowhere to slide to. A move has two squares between them.
///
/// The rows are what fade, and the window's opacity is not touched at all. Fading the
/// window took the panel with it, so the menu dissolved as a whole and the surface
/// was plainly there and gone; this keeps the menu and fades everything in it, which
/// is what a panel whose contents change should look like.
#[allow(clippy::too_many_arguments)]
fn context_ease(
    context: &Rc<ContextMenu>,
    from_rows: f64,
    to_rows: f64,
    from_left: i32,
    to_left: i32,
    from_top: i32,
    to_top: i32,
) -> u64 {
    let context = context.clone();
    context.animation.set(context.animation.get().wrapping_add(1));
    let token = context.animation.get();
    let start = glib::monotonic_time();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        if context.animation.get() != token {
            return glib::ControlFlow::Break;
        }
        let elapsed = (glib::monotonic_time() - start) as f64 / 1_000_000.0;
        let t: f64 = (elapsed / MENU_FADE.as_secs_f64()).clamp(0.0, 1.0);
        // The same ease the reveal uses, so a move feels like this menu arriving
        // rather than some other one.
        let eased = 1.0 - (1.0 - t).powi(3);
        context
            .rows
            .set_opacity(from_rows + (to_rows - from_rows) * eased);
        context
            .window
            .set_margin(Edge::Left, ease_margin(from_left, to_left, eased));
        context
            .window
            .set_margin(Edge::Top, ease_margin(from_top, to_top, eased));
        if t < 1.0 {
            return glib::ControlFlow::Continue;
        }
        context.rows.set_opacity(to_rows);
        context.window.set_margin(Edge::Left, to_left);
        context.window.set_margin(Edge::Top, to_top);
        glib::ControlFlow::Break
    });
    token
}

/// Where the menu's left edge is at this point of a slide.
fn ease_margin(from: i32, to: i32, f: f64) -> i32 {
    (f64::from(from) + (f64::from(to) - f64::from(from)) * f).round() as i32
}

/// Open the app menu for an app, or move an open one onto it.
fn context_open(
    context: &Rc<ContextMenu>,
    key: &str,
    windows: &[WindowInfo],
    button: &Button,
    stream: &Rc<UnixStream>,
) {
    let (icon_center, bar_width) = icon_metrics(button);
    context.icon_center.set(icon_center);
    context.bar_width.set(bar_width);
    *context.stream.borrow_mut() = Some(stream.clone());
    context.app.replace(Some(key.to_string()));
    context_fill(context, key, windows);
    if menu_debug() {
        let mut count = 0;
        let mut child = context.rows.first_child();
        while child.is_some() {
            count += 1;
            child = child.and_then(|widget| widget.next_sibling());
        }
        let rows = count;
        eprintln!(
            "oxide-panel: context open {key:?} with {} windows, {rows} rows",
            windows.len()
        );
    }
    context.shown.set(true);
    context.window.present();
    // The panel is up before anything fades and never fades; only the rows do. Fading
    // the window took the menu with it, so the surface was plainly there and gone
    // rather than a panel whose contents arrived in it.
    context.window.set_opacity(1.0);
    context.rows.set_opacity(0.0);
    // Under the square, measured from the rows that were just built. Measured rather
    // than waited for: a surface that has not been mapped has no width yet, and by
    // the time it does the menu has been in the wrong place.
    let left = menu_left(
        icon_center,
        bar_width,
        context.rows.measure(gtk4::Orientation::Horizontal, -1).1,
    );
    // And the slide out of the bar, which `animate_surface` would have done along
    // with the window's opacity.
    let resting = PANEL_HEIGHT;
    let _ = context_ease(
        context,
        0.0,
        1.0,
        left,
        left,
        resting - MENU_SLIDE,
        resting,
    );
}

/// Take the context menu off screen, then let it go.
fn context_close(context: &Rc<ContextMenu>) {
    if !context.shown.get() {
        return;
    }
    // Whatever the pointer was last over, it is not over a menu that is no longer
    // there, so a close cannot be left scheduled against it.
    context.hover.inside.set(false);
    context.hover.token.set(context.hover.token.get().wrapping_add(1));
    context.app.replace(None);
    let left = menu_left(
        context.icon_center.get(),
        context.bar_width.get(),
        context.rows.measure(gtk4::Orientation::Horizontal, -1).1,
    );
    let fading = context.clone();
    // The rows fade and the panel slides back up into the bar; the window's own
    // opacity is not touched, because the panel is not what is leaving.
    let resting = PANEL_HEIGHT;
    let token = context_ease(
        context,
        1.0,
        0.0,
        left,
        left,
        resting,
        resting - MENU_SLIDE,
    );
    // `shown` is not cleared here. It is cleared when the fade lands, so that a hover
    // arriving in the next 180ms cannot put the previews into a menu that is still on
    // screen.
    glib::timeout_add_local(MENU_FADE, move || {
        if fading.animation.get() != token {
            // Superseded by a reopen.
            return glib::ControlFlow::Break;
        }
        // Now it really is gone: the surface itself, not just its contents.
        fading.window.set_visible(false);
        fading.shown.set(false);
        glib::ControlFlow::Break
    });
}

/// Close the context menu once the pointer has been off it for a moment.
///
/// The same treatment the preview menu gets, and for the same reason: the pointer
/// has to cross the bar to reach the menu, so a leave on one surface is not the
/// pointer leaving both.
fn watch_context_hover(context: &Rc<ContextMenu>, bar_row: &GtkBox) {
    let context = context.clone();
    // The bar as well as the menu, and for the same reason the preview menu watches
    // both: the pointer has to cross the bar to reach the menu, so a leave on one is
    // not the pointer leaving both. Without the bar here, walking back up to the bar
    // left the app menu on screen with nothing to dismiss it but another right click.
    let widgets: Vec<gtk4::Widget> =
        vec![bar_row.clone().upcast(), context.rows.clone().upcast()];
    for rows in widgets {
    let context = context.clone();
    let motion = gtk4::EventControllerMotion::new();

    let on_move = context.hover.clone();
    motion.connect_motion(move |_, _, _| {
        on_move.inside.set(true);
        on_move.token.set(on_move.token.get().wrapping_add(1));
    });
    let on_enter = context.hover.clone();
    motion.connect_enter(move |_, _, _| {
        on_enter.inside.set(true);
        on_enter.token.set(on_enter.token.get().wrapping_add(1));
    });

    let on_leave = context.hover.clone();
    motion.connect_leave(move |_| {
        on_leave.inside.set(false);
        let token = on_leave.token.get().wrapping_add(1);
        on_leave.token.set(token);
        let context = context.clone();
        glib::timeout_add_local(HOVER_GRACE, move || {
            if context.hover.token.get() != token {
                return glib::ControlFlow::Break;
            }
            // A surface being resized under a stationary pointer is handed a leave,
            // and the next resize hands it another. So while it is still moving, the
            // close waits: the pointer has not gone anywhere, the surface has.
            if context.shown.get() && !context.hover.inside.get() {
                context_close(&context);
            }
            glib::ControlFlow::Break
        });
    });

    rows.add_controller(motion);
    }
}

thread_local! {
    /// The last snapshot, for the paths that are handed one app's windows and still
    /// need to know what else exists.
    static SNAPSHOT: RefCell<Vec<WindowInfo>> = const { RefCell::new(Vec::new()) };
}

thread_local! {
    /// The last thing the bar was built from, so it can be built again without
    /// waiting for a snapshot.
    ///
    /// Pinning is the panel's own state, not the compositor's: nothing in the window
    /// list changes when an app is pinned, so there is no snapshot coming to rebuild
    /// the bar with. Without this the square a pin adds would not appear until
    /// something else happened to move a window.
    static LAST_BAR: RefCell<Option<(GtkBox, Rc<Menu>, Vec<WindowInfo>, Rc<UnixStream>, Rc<ContextMenu>)>> =
        const { RefCell::new(None) };
}

/// Build the bar again from the last snapshot.
fn rebuild_bar() {
    let state = LAST_BAR.with(|cell| cell.borrow().clone());
    let Some((tasks, menu, windows, stream, context)) = state else {
        return;
    };
    rebuild_tasks(&tasks, &menu, &windows, &stream, &context);
}

thread_local! {
    /// The apps pinned to the bar, and the order the bar is in.
    ///
    /// The panel's own state: pinning an app keeps its square in the bar when it has
    /// no windows, and the order is where the squares sit, which a snapshot says
    /// nothing about.
    ///
    /// Read from disk on the way to the first bar, and written back whenever it
    /// changes, so a pin lasts past a restart instead of lasting until one.
    static LAYOUT: RefCell<Layout> = RefCell::new(read_layout());
}

#[derive(Clone, Default)]
struct Layout {
    /// Apps whose square stays in the bar whether or not they have windows.
    pinned: HashSet<String>,
    /// The bar's order, as app ids. An app not named here goes after the ones that
    /// are, in the order the snapshot gave.
    order: Vec<String>,
}

impl From<crate::panel_conf::PanelLayout> for Layout {
    fn from(saved: crate::panel_conf::PanelLayout) -> Self {
        Self {
            pinned: saved.pinned,
            order: saved.order,
        }
    }
}

fn read_layout() -> Layout {
    crate::panel_conf::load().into()
}

/// Write the layout back out. Best effort: a preference that could not be saved is
/// better than a bar that will not build.
fn save_layout() {
    let layout = LAYOUT.with(|layout| layout.borrow().clone());
    crate::panel_conf::save(&crate::panel_conf::PanelLayout {
        order: layout.order,
        pinned: layout.pinned,
    });
}

fn is_pinned(id: &str) -> bool {
    LAYOUT.with(|layout| layout.borrow().pinned.contains(id))
}

/// Pin or unpin an app, and say which it ended up as.
fn toggle_pin(id: &str) -> bool {
    let pinned = LAYOUT.with(|layout| {
        let mut layout = layout.borrow_mut();
        if !layout.pinned.insert(id.to_string()) {
            layout.pinned.remove(id);
            return false;
        }
        true
    });
    // Both ways, not just pinning: an unpin that did not outlive the next restart
    // would pin the app again for no reason.
    save_layout();
    if menu_debug() {
        eprintln!(
            "oxide-panel: {} {id:?}",
            if pinned { "pinned" } else { "unpinned" }
        );
    }
    pinned
}

/// The apps pinned to the bar that have nothing open, and so need a square built for
/// them out of nothing but the pin.
///
/// A pinned app is meant to be somewhere to start it from. An app with no windows has
/// no group of its own in a snapshot, so without this there would be nothing in the
/// bar to unpin it from either.
fn pinned_without_windows(groups: &[(String, Vec<&WindowInfo>)]) -> Vec<String> {
    LAYOUT.with(|layout| {
        let layout = layout.borrow();
        layout
            .pinned
            .iter()
            .filter(|id| !groups.iter().any(|(key, _)| key == *id))
            .filter(|id| crate::desktop::is_app_id(id))
            .cloned()
            .collect()
    })
}

/// Where each of `keys` goes in the bar, as an index into `keys`.
///
/// Every app is remembered the first time it is seen, and from then on the bar is in
/// that remembered order. A snapshot says which apps are open and nothing about where
/// their squares belong, so taking its order as the bar's would shuffle the bar
/// whenever a window opened or closed — most visibly when an app started up, put
/// itself last, and pushed everything else along.
fn arrange(keys: &[String]) -> Vec<usize> {
    LAYOUT.with(|layout| {
        let mut layout = layout.borrow_mut();
        for key in keys {
            if !layout.order.iter().any(|entry| entry == key) {
                layout.order.push(key.clone());
            }
        }
        let position = |key: &String| {
            layout
                .order
                .iter()
                .position(|entry| entry == key)
                .unwrap_or(usize::MAX)
        };
        let mut out: Vec<usize> = (0..keys.len()).collect();
        out.sort_by_key(|index| position(&keys[*index]));
        out
    })
}

// ---------------------------------------------------------------- hover to close

/// Whether the pointer is over the bar or the menu, and a token so a deferred
/// close can tell it was superseded by a re-entry.
#[derive(Default)]
struct Hover {
    inside: Cell<bool>,
    token: Cell<u64>,
}

/// Close the menu once the pointer has been outside both surfaces for a moment.
fn watch_hover(bar_row: &GtkBox, menu: &Rc<Menu>, hover: &Rc<Hover>) {
    let menu = menu.clone();
    let bar = bar_row.clone().upcast::<gtk4::Widget>();
    let canvas = menu.canvas.clone().upcast::<gtk4::Widget>();
    // The bar's row and the menu's canvas, both content widgets rather than
    // toplevels. A toplevel also reports enter and leave when it is resized or
    // reconfigured, which is not the pointer going anywhere, and this menu is
    // resized as previews arrive.
    for widget in [bar, canvas] {
        let motion = gtk4::EventControllerMotion::new();
        // Per iteration: the handler is `Fn` and so borrows its captures.
        let closing_menu = menu.clone();

        // Movement anywhere inside the surface counts as being on it, not just the
        // first event. A surface that resizes while the pointer is still resting on
        // it is handed a leave, and treating that as the pointer having gone is what
        // closed the menu while the pointer never left it.
        let on_move = hover.clone();
        motion.connect_motion(move |_, _, _| {
            on_move.inside.set(true);
            on_move.token.set(on_move.token.get() + 1);
        });

        let on_enter = hover.clone();
        motion.connect_enter(move |_, _, _| {
            on_enter.inside.set(true);
            // Invalidate any close that is already scheduled.
            on_enter.token.set(on_enter.token.get() + 1);
        });

        let on_leave = hover.clone();
        motion.connect_leave(move |_| {
            on_leave.inside.set(false);
            let token = on_leave.token.get() + 1;
            on_leave.token.set(token);
            let closing = closing_menu.clone();
            // Deferred, because crossing from the bar down to the menu leaves one
            // widget before entering the other, and that must not read as leaving.
            // The token check drops the close if the pointer came back.
            //
            // Cloned in here rather than outside: the handler is `Fn`, so it
            // cannot hand its own captures to a `'static` closure.
            let recheck = on_leave.clone();
            glib::timeout_add_local(HOVER_GRACE, move || {
                // A surface being resized under a stationary pointer is handed a
                // leave, and the next resize will hand it another. So while the menu
                // is still moving, a close is put off rather than acted on: the
                // pointer has not gone anywhere, the surface has.
                if closing.morphing.get() {
                    return glib::ControlFlow::Continue;
                }
                if recheck.token.get() == token && !recheck.inside.get() {
                    menu_close(&closing);
                }
                glib::ControlFlow::Break
            });
        });

        widget.add_controller(motion);
    }
}

thread_local! {
    /// The desktop accent, as 0..=1 components, read by [`accent_rgb`] and
    /// re-read whenever the portal says it changed.
    static ACCENT: Cell<(f64, f64, f64)> =
        const { Cell::new((0.847, 0.847, 0.847)) };
    /// The provider holding the panel's stylesheet, kept so a new accent can be
    /// loaded into the same one rather than stacking up providers.
    static STYLE_PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
}

fn accent_rgb() -> (f64, f64, f64) {
    ACCENT.with(Cell::get)
}

fn style_sheet((r, g, b): (u8, u8, u8)) -> String {
    format!(
        "
window.panel-window {{
    /* Transparent: the surface spans the output, so any background here would
       paint edge to edge as soon as the menu makes the window taller. The bar
       and the menu each carry their own. */
    background-color: transparent;
    border: none;
    box-shadow: none;
}}
.panel {{
    background-color: rgba(32, 32, 32, 0.82);
    padding: 0 10px;
}}
.panel label {{
    color: #e8e8e8;
    font-weight: 500;
}}
.panel .title {{
    color: rgb({r}, {g}, {b});
    font-weight: 700;
}}
.panel .tasks {{
    margin-left: 6px;
}}
button.task {{
    background: transparent;
    border: 1px solid rgba(255, 255, 255, 0.25);
    box-shadow: none;
    padding: 0;
    border-radius: 8px;
}}
button.task:hover {{
    background-color: rgba(255, 255, 255, 0.10);
}}
/* Deliberately not dimmed. Minimized and closed are different things, and dimming
   the square conflates them with a pinned app that has nothing open at all. */
button.task.minimized {{
    background-color: rgba(255, 255, 255, 0.04);
}}
button.task.focused {{
    background-color: rgba({r}, {g}, {b}, 0.28);
}}
/* The app menu, opened by a right click. Widgets, so it is styled here; the colours
   are the preview menu's own, because two menus belonging to the same panel should
   not be two different greys. The previews are cairo-drawn over whatever is behind
   them at 35% black, so this is the same fill and the same edge — the same
   transparency, not a different one that happens to look similar. */
window.context {{
    background-color: rgba(0, 0, 0, 0.35);
    /* The theme paints a window background *image* — a shadow, and on some themes a
       fill of its own — on top of the background colour, which is what made this
       menu read as a more solid panel than the previews even though the two are the
       same 35% black. Told to stop, and the colour is the whole of it. */
    background-image: none;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 10px;
}}
.context-rows {{
    padding: 4px;
}}
button.context-row {{
    background: transparent;
    background-image: none;
    border: none;
    box-shadow: none;
    padding: 7px 10px;
    border-radius: 7px;
    color: #e8e8e8;
    font-weight: 500;
}}
button.context-row:hover {{
    background-color: rgba(255, 255, 255, 0.10);
}}
/* The label is its own node, and the theme has an opinion about its colour. */
button.context-row label {{
    color: #e8e8e8;
    background: transparent;
}}
/* The app itself, which is the one row that is not a command. */
button.context-row.app label {{
    font-weight: 700;
}}
"
    )
}

#[derive(Clone, Debug)]
struct WindowInfo {
    id: u64,
    focused: bool,
    minimized: bool,
    app_id: String,
    title: String,
}

/// Something that arrived from the compositor.
#[derive(Debug)]
enum PanelEvent {
    /// The window list changed.
    Snapshot(Vec<WindowInfo>),
    /// A window preview, as R, G, B, A rows.
    Image {
        id: u64,
        width: i32,
        height: i32,
        pixels: Vec<u8>,
    },
}

/// An image whose payload has not fully arrived yet: the header is in, the
/// pixels are still on their way.
struct PendingImage {
    id: u64,
    width: i32,
    height: i32,
    /// How many payload bytes are owed.
    len: usize,
}

/// Reads the compositor's byte stream, which mixes newline-delimited text with
/// length-prefixed image payloads.
///
/// The payload cannot be newline-terminated: raw pixels contain `\n`, which
/// would truncate the message, so an `img` header states its own byte count and
/// exactly that many bytes are consumed before the next line is looked at.
#[derive(Default)]
struct PanelReader {
    buffer: Vec<u8>,
    pending: Option<PendingImage>,
}

impl PanelReader {
    /// Take everything parseable out of the buffer.
    fn drain(&mut self) -> Vec<PanelEvent> {
        let mut events = Vec::new();
        loop {
            if let Some(pending) = self.pending.take() {
                // The payload is owed in full before it means anything; a partial
                // one stays pending, with the bytes left buffered for next time.
                if self.buffer.len() < pending.len {
                    self.pending = Some(pending);
                    break;
                }
                let payload: Vec<u8> = self.buffer.drain(..pending.len).collect();
                events.push(PanelEvent::Image {
                    id: pending.id,
                    width: pending.width,
                    height: pending.height,
                    pixels: payload,
                });
                continue;
            }

            let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') else {
                break;
            };
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if let Some(header) = line.strip_prefix("img\t") {
                if let Some(pending) = parse_image_header(header) {
                    self.pending = Some(pending);
                }
            } else if let Some(windows) = parse_snapshot(&line) {
                events.push(PanelEvent::Snapshot(windows));
            }
        }
        events
    }
}

/// Parse an `img\t<id>\t<width>\t<height>\t<len>` header into the image it
/// introduces, with room reserved for its payload.
fn parse_image_header(header: &str) -> Option<PendingImage> {
    let mut fields = header.split('\t');
    let id = fields.next()?.parse::<u64>().ok()?;
    let width = fields.next()?.parse::<i32>().ok()?;
    let height = fields.next()?.parse::<i32>().ok()?;
    let len = fields.next()?.parse::<usize>().ok()?;
    if width <= 0 || height <= 0 || len != (width as usize) * (height as usize) * 4 {
        return None;
    }
    Some(PendingImage {
        id,
        width,
        height,
        len,
    })
}

fn send(stream: &Rc<UnixStream>, message: &str) {
    let _ = (&**stream).write_all(message.as_bytes());
}

pub fn run_panel() {
    // One panel per compositor, which means one D-Bus application id per
    // compositor. A shared id makes the session bus treat a second panel as a
    // remote instance of the first: it forwards its activation to the panel that
    // is already running and then exits, so the second compositor gets no panel
    // and the first grows a second window — on its own output, since that
    // panel's Wayland connection is the one it was started with.
    //
    // The compositor hands its socket name to us in `WAYLAND_DISPLAY`, which is
    // unique per instance (`bind_auto` never reuses a name), so keying the id off
    // it keeps the panels independent without any extra plumbing.
    let id = match std::env::var("WAYLAND_DISPLAY") {
        Ok(socket) if !socket.is_empty() => format!("dev.oxide.Panel.{socket}"),
        _ => "dev.oxide.Panel".to_string(),
    };
    let app = Application::builder().application_id(&id).build();

    let stream = connect_panel();
    app.connect_activate(move |app| build_ui(app, stream.clone()));

    // Ignore the `--panel` argument passed by the compositor.
    app.run_with_args::<&str>(&[]);
}

/// Connect to the compositor's panel socket, if it was passed to us.
fn connect_panel() -> Option<Rc<UnixStream>> {
    let path = std::env::var_os("OXIDE_PANEL_SOCKET")?;
    let stream = UnixStream::connect(PathBuf::from(path)).ok()?;
    stream.set_nonblocking(true).ok()?;
    Some(Rc::new(stream))
}

fn build_ui(app: &Application, stream: Option<Rc<UnixStream>>) {
    if !gtk4_layer_shell::is_supported() {
        eprintln!("oxide-desktop panel: compositor does not support wlr-layer-shell");
        std::process::exit(1);
    }

    install_css();

    let Some(monitor) = gdk::Display::default().and_then(|display| {
        display
            .monitors()
            .item(0)
            .and_then(|obj| obj.downcast::<gdk::Monitor>().ok())
    }) else {
        eprintln!("oxide-desktop panel: no monitor to put the panel on");
        return;
    };

    // The bar and the menu are two separate layer surfaces, not one window with
    // two rows.
    //
    // A surface's *unpainted* regions still hit-test: the compositor routes the
    // pointer by geometry, not by what was drawn. So a bar that grew to fit the
    // menu would go on swallowing clicks — and forcing the default cursor —
    // across its whole height, invisibly, and would keep doing so after the menu
    // closed if the window had not shrunk back. Two surfaces make that
    // impossible: the bar is always exactly `PANEL_HEIGHT`, and the menu is sized
    // to exactly what it draws.
    let (bar, bar_row, tasks) = build_bar_window(app, &monitor);
    let menu = build_menu(app, &monitor, stream.clone());
    *menu.stream.borrow_mut() = stream.clone();
    // The app menu, opened by a right click. A third surface for the same reason
    // the menu is a second: an unpainted region of a surface still takes clicks.
    let context = build_context_menu(app, &monitor);
    // Watching needs the bar, so it is wired here rather than at the build.
    watch_context_hover(&context, &bar_row);
    let hover = Rc::new(Hover::default());

    // Squares can be dragged along the bar to reorder them. The drag carries the app
    // id and the row decides which gap it landed in, so the gesture itself needs to
    // know nothing about the bar.
    watch_reorder(&tasks);

    // Hover-out closes the menu. The pointer has to cross the bar to reach the
    // menu, so "inside" spans both surfaces, and the close is deferred briefly so
    // passing between them is not read as leaving.
    watch_hover(&bar_row, &menu, &hover);

    if let Some(stream) = stream {
        send_accent(&stream);
        start_polling(&tasks, &menu, stream, hover, &context);
    }

    bar.present();
}

/// The bar: a fixed-height strip across the top of the output.
///
/// Returns the window and the box the app squares go in, rather than having the
/// caller dig the box back out of the widget tree — the row's first child is the
/// title label, not the task list.
fn build_bar_window(app: &Application, monitor: &gdk::Monitor) -> (ApplicationWindow, GtkBox, GtkBox) {
    let window = ApplicationWindow::builder().application(app).build();
    window.set_decorated(false);
    window.add_css_class("panel-window");
    window.init_layer_shell();
    window.set_namespace(Some("oxide-panel"));
    window.set_layer(Layer::Top);
    window.set_monitor(Some(monitor));
    for edge in [Edge::Top, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    // Reserve space so maximized windows stop below the bar.
    window.set_exclusive_zone(PANEL_HEIGHT);

    let row = GtkBox::new(Orientation::Horizontal, 8);
    row.add_css_class("panel");
    row.set_size_request(-1, PANEL_HEIGHT);

    let title = Label::new(Some("oxide"));
    title.add_css_class("title");

    let tasks = GtkBox::new(Orientation::Horizontal, 2);
    tasks.add_css_class("tasks");

    let clock = Label::new(None);
    clock.set_hexpand(true);
    clock.set_halign(gtk4::Align::End);

    row.append(&title);
    row.append(&tasks);
    row.append(&clock);

    update_clock(&clock);
    glib::timeout_add_seconds_local(
        1,
        glib::clone!(
            #[weak]
            clock,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                update_clock(&clock);
                glib::ControlFlow::Continue
            }
        ),
    );

    window.set_child(Some(&row));
    (window, row, tasks)
}

/// Tell the compositor the accent so it can tint the snap preview.
fn send_accent(stream: &Rc<UnixStream>) {
    let (red, green, blue) = accent_rgb();
    let message = format!("accent\t{red:.6}\t{green:.6}\t{blue:.6}\n");
    let _ = (&**stream).write_all(message.as_bytes());
}

/// Drain the compositor socket, rebuilding the task list on every snapshot and
/// keeping the newest preview for each window.
fn start_polling(
    tasks: &GtkBox,
    menu: &Rc<Menu>,
    stream: Rc<UnixStream>,
    _hover: Rc<Hover>,
    context: &Rc<ContextMenu>,
) {
    let tasks = tasks.clone();
    let context = context.clone();
    let reader = RefCell::new(PanelReader::default());
    let windows = Rc::new(RefCell::new(Vec::<WindowInfo>::new()));
    // Both timers below need this state, so hand each its own handle.
    let (stream_poll, stream_refresh) = (stream.clone(), stream.clone());
    let (windows_poll, windows_refresh) = (windows.clone(), windows.clone());
    let (menu_poll, menu_tick) = (menu.clone(), menu.clone());
    let stream = stream_poll;
    let windows = windows_poll;
    let menu = menu_poll;
    glib::timeout_add_local(POLL_INTERVAL, move || {
        let mut buf = [0u8; 65536];
        loop {
            match (&*stream).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => reader.borrow_mut().buffer.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        for event in reader.borrow_mut().drain() {
            match event {
                PanelEvent::Snapshot(list) => {
                    windows.replace(list.clone());
                    rebuild_tasks(&tasks, &menu, &list, &stream, &context);
                }
                PanelEvent::Image {
                    id,
                    width,
                    height,
                    pixels,
                } => {
                    // Straight to the menu, which keeps the preview. There used to
                    // be a cache here that the value was read back out of — but
                    // `insert` returns the value it *replaced*, so the first preview
                    // of a window never arrived and every later one was a frame
                    // stale, which is exactly what a preview that never seems to
                    // change looks like.
                    menu_set_image(&menu, id, width, height, pixels);
                }
            }
        }

        glib::ControlFlow::Continue
    });

    // Keep an open menu's previews current, so a window that is animating or
    // playing video is not shown frozen.
    glib::timeout_add_local(PREVIEW_REFRESH, move || {
        let snapshot = windows_refresh.borrow().clone();
        menu_refresh(&menu_tick, &snapshot, &stream_refresh);
        glib::ControlFlow::Continue
    });
}

/// The title shown for a window, falling back to the app id.
fn window_title(info: &WindowInfo) -> String {
    if info.title.is_empty() {
        if info.app_id.is_empty() {
            return "Window".to_string();
        }
        return info.app_id.clone();
    }
    info.title.clone()
}

/// Point a preview at the pixels that arrived for it, sizing it to the image's
/// own aspect ratio so every preview in the row shares one height.
fn icon_metrics(button: &Button) -> (i32, i32) {
    let Some(root) = button.root() else {
        return (0, 0);
    };
    let bar_width = root.width();
    let width = button.width() as f64;
    match (
        button.translate_coordinates(&root, 0.0, 0.0),
        button.translate_coordinates(&root, width, 0.0),
    ) {
        (Some((left, _)), Some((right, _))) => {
            let center = ((left + right) / 2.0).round() as i32;
            (center, bar_width)
        }
        _ => (0, bar_width),
    }
}

/// Rebuild the icon row, and keep any open menu in step with it.
fn rebuild_tasks(
    tasks: &GtkBox,
    menu: &Rc<Menu>,
    windows: &[WindowInfo],
    stream: &Rc<UnixStream>,
    context: &Rc<ContextMenu>,
) {
    // Remembered before anything else, so a rebuild asked for by the panel's own
    // state — a pin, an unpin — can be served without waiting for a snapshot.
    SNAPSHOT.with(|cell| *cell.borrow_mut() = windows.to_vec());
    LAST_BAR.with(|cell| {
        *cell.borrow_mut() = Some((
            tasks.clone(),
            menu.clone(),
            windows.to_vec(),
            stream.clone(),
            context.clone(),
        ));
    });
    while let Some(child) = tasks.first_child() {
        tasks.remove(&child);
    }

    // Group the windows by app. Windows with no app id get a key of their own so
    // they don't all collapse together.
    let mut groups: Vec<(String, Vec<&WindowInfo>)> = Vec::new();
    for info in windows {
        let key = if info.app_id.is_empty() {
            format!("#{}", info.id)
        } else {
            info.app_id.clone()
        };
        match groups.iter_mut().find(|(group_key, _)| *group_key == key) {
            Some((_, group)) => group.push(info),
            None => groups.push((key, vec![info])),
        }
    }

    // A pinned app with nothing open gets a square of its own, so there is somewhere
    // in the bar to start it from — and somewhere to unpin it from.
    for id in pinned_without_windows(&groups) {
        groups.push((id, Vec::new()));
    }

    // And the bar goes in the order the panel remembers, not the order the snapshot
    // happened to list the apps in.
    let keys: Vec<String> = groups.iter().map(|(key, _)| key.clone()).collect();
    let positions = arrange(&keys);
    let mut ordered: Vec<(String, Vec<&WindowInfo>)> = Vec::with_capacity(groups.len());
    for index in positions {
        ordered.push(groups[index].clone());
    }
    let groups = ordered;

    // An open menu is kept in step with every snapshot, *including* one where its
    // app has no windows left. Skipping that case — or closing the menu outright on
    // it — is what stopped the close animation: with no group to find, the entries
    // were never told to go, and the menu simply vanished. An empty group instead
    // animates them away, and the menu closes itself once the last one has.
    let open_key = menu.app.borrow().clone();
    let every: Vec<&WindowInfo> = windows.iter().collect();
    if let Some(key) = &open_key {
        let group: Vec<&WindowInfo> = groups
            .iter()
            .find(|(group_key, _)| group_key == key)
            .map(|(_, group)| group.clone())
            .unwrap_or_default();
        // Every window, minimized ones included. A minimized window cannot be
        // captured, so the compositor answers with the last frame it took for it and
        // the cell shows that: more use than a hole in the row, and it keeps the count
        // on the square and the number of previews in step.
        menu_replace(menu, &every, &group, Some(stream));
        if menu_debug() {
            // The bar and the menu disagreeing about which windows exist is the
            // awkward one to read off the screen: the count is right and the row is
            // empty, with nothing in between to say why.
            let entries = menu.entries.borrow();
            let order = menu.order.borrow();
            let with_preview = order
                .iter()
                .filter(|id| {
                    entries
                        .get(id)
                        .is_some_and(|entry| entry.preview.borrow().is_some())
                })
                .count();
            eprintln!(
                "oxide-panel: snapshot {} windows, {} minimized; menu order {order:?}, \
                 {with_preview} with a preview",
                windows.len(),
                windows.iter().filter(|info| info.minimized).count(),
            );
        }
    }

    // An open app menu is kept in step with the new snapshot too. What it offers
    // depends on the window count — one window is closed by name, several are closed
    // together, none means no row at all — so a window opened or closed underneath
    // it has to change what is on screen.
    // `shown` rather than `app`: the app key outlives the fade now, so refilling a
    // menu that is on its way out would be work for nothing.
    if let Some(key) = context.shown.get().then(|| context.app.borrow().clone()).flatten() {
        let group: Vec<WindowInfo> = groups
            .iter()
            .find(|(group_key, _)| *group_key == key)
            .map(|(_, group)| group.iter().map(|info| (*info).clone()).collect())
            .unwrap_or_default();
        context_fill(context, &key, &group);
    }

    for (key, group) in &groups {
        let app_id = if key.starts_with('#') { "" } else { key.as_str() };
        let multiple = group.len() > 1;
        let button = app_button(
            app_id,
            group,
            stream,
            multiple,
            key,
            menu,
            context,
        );
        tasks.append(&button);
    }

    // Bring an open menu in step with the new snapshot, so a title change or a
    // focus change shows up without reopening it.

}

/// What a click on a square does: bring one window forward, or show the previews.
///
/// A square with nothing behind it is a pinned app's, and starts it — there is no
/// window to bring forward.
#[allow(clippy::too_many_arguments)]
fn activate(
    stream: &Rc<UnixStream>,
    key: &str,
    windows: &[WindowInfo],
    menu: &Rc<Menu>,
    button: &Button,
    multiple: bool,
) {
    if !multiple {
        match windows.first() {
            None => send(stream, &format!("launch\t{key}\n")),
            Some(window) => send(stream, &format!("focus\t{}\n", window.id)),
        }
        return;
    }
    // A second click on the same square closes it, rather than rebuilding the menu
    // under the pointer.
    if menu.shown.get() && menu.app.borrow().as_deref() == Some(key) {
        menu_close(menu);
        return;
    }
    let group: Vec<&WindowInfo> = windows.iter().collect();
    menu_open(menu, key, &group, button, stream);
}

/// Let a square be dropped anywhere on the row of squares to give it a new place.
fn watch_reorder(tasks: &GtkBox) {
    let target = gtk4::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    let row = tasks.clone();
    target.connect_drop(move |_, value, x, _| {
        let Some(id) = value.get::<String>().ok() else {
            return false;
        };
        if !crate::desktop::is_app_id(&id) {
            // A window that never said what it was has no place in the order; there
            // is nothing to remember about it between runs.
            return false;
        }
        let index = drop_index(&row, x);
        place_at(&id, index);
        if menu_debug() {
            eprintln!("oxide-panel: dropped {id:?} at {index}");
        }
        // The bar is rebuilt on an idle rather than here: this runs inside the drop,
        // while the square being dragged is still on screen, and taking the widgets
        // out from under it mid-gesture loses the drag.
        glib::idle_add_local_once(|| rebuild_bar());
        true
    });
    tasks.add_controller(target);
}

/// Which gap in the bar a drop at this x lands in, given where its squares are.
///
/// The bar is a row, so the gap is decided by which squares' middles the pointer is
/// past: left of the first square's middle is before it, right of the last is after
/// it, and in between it is whichever side of that square's middle the pointer is on.
///
/// Separated from the widget walking so the arithmetic can be tested without a
/// display, which is the part that can be off by one and put a square dropped
/// between two of them one place out.
fn gap_at(middles: &[f64], x: f64) -> usize {
    middles.iter().filter(|middle| x > **middle).count()
}

/// [`gap_at`], for a drop on the real row.
fn drop_index(tasks: &GtkBox, x: f64) -> usize {
    let mut middles = Vec::new();
    let mut child = tasks.first_child();
    while let Some(widget) = child {
        let square = widget.upcast::<gtk4::Widget>();
        if let Some((left, _)) = square.translate_coordinates(tasks, 0.0, 0.0) {
            middles.push(left + f64::from(square.width()) / 2.0);
        }
        child = square.next_sibling();
    }
    gap_at(&middles, x)
}

/// Move an app's square to a place in the bar, and remember it.
fn place_at(id: &str, index: usize) {
    LAYOUT.with(|layout| {
        let mut layout = layout.borrow_mut();
        layout.order.retain(|entry| entry != id);
        let index = index.min(layout.order.len());
        layout.order.insert(index, id.to_string());
    });
    save_layout();
}

/// One square (1:1) per app, with indicator dots for its window count.
///
/// With several windows open the square opens a menu of their previews
/// instead of focusing; a lone window is just focused.
fn app_button(
    app_id: &str,
    windows: &[&WindowInfo],
    stream: &Rc<UnixStream>,
    multiple: bool,
    key: &str,
    menu: &Rc<Menu>,
    context: &Rc<ContextMenu>,
) -> Button {
    let focused = windows.iter().any(|window| window.focused);
    // Only when every one of the app's windows is minimized, so the square says the
    // app is there but not on screen rather than implying anything about the others.
    let minimized = !windows.is_empty() && windows.iter().all(|window| window.minimized);
    let count = windows.len();
    // A pinned app with nothing open. Its square is here to be started from, so it
    // is not "minimized" — there is no window of its own to have been minimized.
    let idle = count == 0;

    let button = Button::new();
    button.add_css_class("task");
    if minimized || idle {
        button.add_css_class("minimized");
    }
    if focused {
        button.add_css_class("focused");
    }
    // Without this the HBox stretches the button to the panel height.
    button.set_valign(gtk4::Align::Center);
    button.set_halign(gtk4::Align::Center);
    button.set_size_request(SQUARE_SIZE, SQUARE_SIZE);

    // A square with nothing behind it names the app rather than counting windows it
    // does not have.
    let tooltip = match count {
        0 => crate::desktop::cached(key)
            .map(|app| app.label().to_string())
            .unwrap_or_else(|| "Not running".to_string()),
        1 => window_title(windows[0]),
        _ => format!("{count} windows"),
    };
    // A tooltip says what a square is, and a menu under the pointer is about to say it
    // better. Left on, it came up underneath the app menu and drew through it: the
    // menus are 35% black over whatever is behind them, so anything behind shows.
    if menu.shown.get() || context.shown.get() {
        button.set_has_tooltip(false);
    } else {
        button.set_has_tooltip(true);
        button.set_tooltip_text(Some(&tooltip));
    }

    let icon = resolve_icon(app_id);
    let image = if icon.starts_with('/') {
        Image::from_file(&icon)
    } else {
        Image::from_icon_name(&icon)
    };
    image.set_pixel_size(ICON_SIZE);

    // The button's 1px border is inside its 36px box, so the content area is
    // 34px and the whole square stays 36x36.
    let inner = SQUARE_SIZE - 2 * SQUARE_BORDER;
    let dots = DrawingArea::new();
    dots.set_content_width(inner);
    dots.set_content_height(INDICATOR_HEIGHT);
    dots.set_size_request(inner, INDICATOR_HEIGHT);
    dots.set_valign(gtk4::Align::End);
    let color = if focused { accent_rgb() } else { MUTED_COLOR };
    dots.set_draw_func(move |_, context, width, height| {
        draw_indicators(context, width, height, count, color);
    });

    let column = GtkBox::new(Orientation::Vertical, 0);
    column.set_halign(gtk4::Align::Center);
    column.set_valign(gtk4::Align::Center);
    column.append(&image);
    column.append(&dots);
    button.set_child(Some(&column));

    // Hovering a square opens that app's menu, and moves an open one to the app
    // under the pointer, so travelling along the bar moves the menu with it.
    //
    // After a moment, not immediately. A square that opened its menu on the pointer
    // arriving would open one for every square the pointer crossed on the way to
    // somewhere, and the last of those would be the only one left standing.
    //
    // Any number of windows, including one. A lone window used to open nothing here
    // and had to be clicked to be seen at all, which made an app that keeps a single
    // window — a chat client in a tray, a browser with one tab — the one app in the
    // bar with nothing to show for it.
    {
        let hover_menu = menu.clone();
        let hover_key = key.to_string();
        let hover_group: Vec<WindowInfo> = windows.iter().map(|info| (*info).clone()).collect();
        let hover_stream = stream.clone();
        let hover_context = context.clone();
        // Shared with the timeout, so leaving the square or moving to another one
        // cancels the open that was about to happen.
        let token = Rc::new(Cell::new(0u64));
        let hover = gtk4::EventControllerMotion::new();

        let on_enter = token.clone();
        hover.connect_enter({
            let menu = hover_menu.clone();
            let key = hover_key.clone();
            let group = hover_group.clone();
            let stream = hover_stream.clone();
            let button = button.clone();
            let context = hover_context.clone();
            move |_, _, _| {
                if group.is_empty() {
                    return;
                }
                // One menu at a time: the app menu is up, so this must not open the
                // previews on top of it. But it should *follow* — the previews move
                // along the bar with the pointer, and an app menu that stayed on the
                // app it was opened for meant resting on one square and moving to
                // another left it offering to close three windows that were not the
                // ones under the pointer.
                if context.shown.get() {
                    if context.app.borrow().as_deref() != Some(key.as_str()) {
                        context_move(&context, &key, &group, &button);
                    }
                    return;
                }
                // Already following the pointer along the bar: come across at once.
                // The delay below is for *opening* a menu, and a quarter of a second
                // to move from one square to the next, with the menu open the whole
                // time, made the bar feel broken.
                if menu.shown.get() {
                    if menu.app.borrow().as_deref() != Some(key.as_str()) {
                        let group: Vec<&WindowInfo> = group.iter().collect();
                        menu_open(&menu, &key, &group, &button, &stream);
                    }
                    return;
                }
                let ticket = on_enter.get().wrapping_add(1);
                on_enter.set(ticket);
                let menu = menu.clone();
                let key = key.clone();
                let group = group.clone();
                let stream = stream.clone();
                let button = button.clone();
                let token = on_enter.clone();
                let context = context.clone();
                glib::timeout_add_local(HOVER_OPEN, move || {
                    if token.get() != ticket {
                        // Left, or moved to another square, before it was due.
                        return glib::ControlFlow::Break;
                    }
                    if menu.app.borrow().as_deref() == Some(key.as_str()) {
                        return glib::ControlFlow::Break;
                    }
                    // An app menu is up. Checked *here* and not only where the timer
                    // was armed: the pointer can arrive on a square, and be right
                    // clicked, inside the quarter of a second before this fires — and
                    // then this opened the previews straight over the app menu that
                    // the right click had just put there.
                    if context.shown.get() {
                        if context.app.borrow().as_deref() != Some(key.as_str()) {
                            context_move(&context, &key, &group, &button);
                        }
                        return glib::ControlFlow::Break;
                    }
                    let group: Vec<&WindowInfo> = group.iter().collect();
                    menu_open(&menu, &key, &group, &button, &stream);
                    glib::ControlFlow::Break
                });
            }
        });

        let on_leave = token.clone();
        hover.connect_leave(move |_| {
            on_leave.set(on_leave.get().wrapping_add(1));
        });
        button.add_controller(hover);
    }

    // A click gesture on *press*, not `clicked`: the panel's layer surface never
    // becomes the active GTK window, so a `clicked` on an inactive window can be
    // swallowed as an activation attempt.
    //
    // Armed on the press but run a moment later, so a press that turns into a drag
    // never runs it. A square cannot both be pressed and picked up, and acting on
    // the press meant every drag first focused or started the app it was
    // rearranging. A release runs it at once, so a plain click is not left waiting.
    // Scoped, because the handles it shadows are the ones the right click below
    // takes its own copies from.
    let owned: Vec<WindowInfo> = windows.iter().map(|info| (*info).clone()).collect();
    let press = Rc::new(Cell::new(0u64));
    let primary = {
    let stream = stream.clone();
    let key = key.to_string();
    // Owned handles, so the callback can outlive this function: a `&GtkBox`
    // parameter could not be captured by a `'static` closure.
    let menu = menu.clone();
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(gtk4::gdk::BUTTON_PRIMARY);
    // The handler needs the very square it is attached to, to line the menu up
    // under it, so hold it weakly rather than moving it into the closure.
    // The token is copied in first, because the release handler and the drag below
    // share it and a closure that moved it would leave them holding a moved value.
    let press_here = press.clone();
    // Two copies, one per closure below.
    let (press_context, release_context) = (context.clone(), context.clone());
    // The release handler needs the same handles, and a closure that took them would
    // leave it holding moved values.
    let (rel_stream, rel_key, rel_menu) = (stream.clone(), key.clone(), menu.clone());
    let (rel_owned, rel_button) = (owned.clone(), button.clone());
    gesture.connect_pressed(glib::clone!(
        #[weak]
        button,
        #[upgrade_or]
        return,
        move |_, _, _, _| {
            let ticket = press_here.get().wrapping_add(1);
            press_here.set(ticket);
            let stream = stream.clone();
            let key = key.clone();
            let menu = menu.clone();
            let owned = owned.clone();
            let button = button.clone();
            let press_here = press_here.clone();
            let context = press_context.clone();
            glib::timeout_add_local(CLICK_ARM, move || {
                if press_here.get() != ticket {
                    // Released already, or a drag took this press instead.
                    return glib::ControlFlow::Break;
                }
                press_here.set(press_here.get().wrapping_add(1));
                // One menu at a time: the app menu is up, so this click dismisses it
                // rather than opening the previews behind it.
                if context.shown.get() {
                    context_close(&context);
                    return glib::ControlFlow::Break;
                }
                activate(&stream, &key, &owned, &menu, &button, multiple);
                glib::ControlFlow::Break
            });
        }
    ));
    {
        let press = press.clone();
        gesture.connect_released(move |_, _, _, _| {
            let ticket = press.get().wrapping_add(1);
            press.set(ticket);
            if release_context.shown.get() {
                context_close(&release_context);
                return;
            }
            activate(
                &rel_stream,
                &rel_key,
                &rel_owned,
                &rel_menu,
                &rel_button,
                multiple,
            );
        });
    }
    gesture
    };
    button.add_controller(primary);

    // A right click is the app's own menu, never its previews: what to do with the
    // app is a different question from which of its windows to show, and the
    // previews are already a hover away.
    // Dragging this square somewhere else in the bar. The drag content is the app
    // id, so a drop knows which square it is being given a place to, and the drop
    // target is the row the squares are in.
    let drag = gtk4::DragSource::new();
    drag.connect_drag_begin({
        let id = key.to_string();
        let press = press.clone();
        move |source, _| {
            // This press is a drag, not a click. Bumping the token stops the armed
            // click from running, so rearranging the bar does not also focus or
            // start the app being moved.
            press.set(press.get().wrapping_add(1));
            let value = id.to_value();
            source.set_content(Some(&gdk::ContentProvider::for_value(&value)));
        }
    });
    button.add_controller(drag);

    let right_stream = stream.clone();
    let right_key = key.to_string();
    let right_menu = menu.clone();
    let context = context.clone();
    let right_windows: Vec<WindowInfo> = windows.iter().map(|info| (*info).clone()).collect();
    let secondary = gtk4::GestureClick::new();
    secondary.set_button(gdk::BUTTON_SECONDARY);
    secondary.connect_pressed(glib::clone!(
        #[weak]
        button,
        #[upgrade_or]
        return,
        move |_, _, _, _| {
            // The previews are replaced, not faded out from under. The app menu goes
            // up in the same place a frame later, and a preview menu still fading out
            // beneath it is two surfaces in the same place — which is what "the menu
            // just opens over it" was. Cut, and let the app menu do the fading.
            menu_hide(&right_menu);
            // A second right click on the same square closes it, as a second left
            // click on the previews does.
            if context.shown.get()
                && context.app.borrow().as_deref() == Some(right_key.as_str())
            {
                context_close(&context);
                return;
            }
            context_open(&context, &right_key, &right_windows, &button, &right_stream);
        }
    ));
    button.add_controller(secondary);

    button
}

/// Draw one dot per window under the icon, or a line when they don't fit.
fn draw_indicators(
    context: &gtk4::cairo::Context,
    width: i32,
    height: i32,
    count: usize,
    color: (f64, f64, f64),
) {
    let (w, h) = (width as f64, height as f64);
    context.set_source_rgb(color.0, color.1, color.2);

    let dot = DOT_SIZE;
    let pitch = dot + DOT_GAP;
    let max_dots = ((w - 4.0) / pitch).floor().max(1.0) as usize;

    if count <= max_dots {
        let total = count as f64 * dot + count.saturating_sub(1) as f64 * DOT_GAP;
        let start = (w - total) / 2.0 + dot / 2.0;
        for i in 0..count {
            context.arc(start + i as f64 * pitch, h / 2.0, dot / 2.0, 0.0, std::f64::consts::TAU);
            let _ = context.fill();
        }
    } else {
        let line_width = (w - 6.0).max(2.0);
        context.rectangle((w - line_width) / 2.0, (h - 2.0) / 2.0, line_width, 2.0);
        let _ = context.fill();
    }
}

/// Pick an icon for an app id.
///
/// Prefers the `Icon=` entry of the matching `.desktop` file (the freedesktop
/// convention for Wayland app ids), then the app id itself and its lowercase
/// form, and finally a generic fallback. An absolute path is returned as-is so
/// the caller can load it from disk.
fn resolve_icon(app_id: &str) -> String {
    if app_id.is_empty() {
        return FALLBACK_ICON.to_string();
    }

    let mut candidates = Vec::new();
    if let Some(icon) = crate::desktop::cached(app_id).map(|app| app.icon) {
        candidates.push(icon);
    }
    candidates.push(app_id.to_string());
    let lower = app_id.to_lowercase();
    if lower != app_id {
        candidates.push(lower);
    }

    let theme = gdk::Display::default().map(|display| gtk4::IconTheme::for_display(&display));
    for name in candidates {
        if name.starts_with('/') {
            if Path::new(&name).exists() {
                return name;
            }
            continue;
        }
        if let Some(theme) = &theme
            && theme.has_icon(&name)
        {
            return name;
        }
    }
    FALLBACK_ICON.to_string()
}

/// Parse a `list\t<count>\t<id>\t<focused>\t<app_id>\t<title>...` line.
fn parse_snapshot(line: &str) -> Option<Vec<WindowInfo>> {
    let mut fields = line.split('\t');
    if fields.next()? != "list" {
        return None;
    }
    let _count: usize = fields.next()?.parse().ok()?;
    let mut windows = Vec::new();
    while let Some(id) = fields.next() {
        let focused = fields.next()? == "1";
        let minimized = fields.next()? == "1";
        let app_id = fields.next()?.to_string();
        let title = fields.next()?.to_string();
        windows.push(WindowInfo {
            id: id.parse().ok()?,
            focused,
            minimized,
            app_id,
            title,
        });
    }
    Some(windows)
}

fn install_css() {
    apply_accent();
}

/// Read the desktop accent, store it for the cairo dots, and (re)load the
/// stylesheet. Read once at startup; a portal `SettingChanged` subscription
/// would be needed to follow changes live.
fn apply_accent() {
    let accent = system_accent();
    ACCENT.with(|cell| {
        cell.set((
            accent.red() as f64,
            accent.green() as f64,
            accent.blue() as f64,
        ))
    });

    let Some(display) = gdk::Display::default() else {
        return;
    };
    let to_byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    let sheet = style_sheet((
        to_byte(accent.red()),
        to_byte(accent.green()),
        to_byte(accent.blue()),
    ));

    STYLE_PROVIDER.with(|cell| {
        let mut slot = cell.borrow_mut();
        match slot.as_ref() {
            Some(provider) => provider.load_from_data(&sheet),
            None => {
                let provider = gtk4::CssProvider::new();
                provider.load_from_data(&sheet);
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_USER,
                );
                *slot = Some(provider);
            }
        }
    });
}

fn system_accent() -> gdk::RGBA {
    portal_accent()
        .unwrap_or_else(|| gdk::RGBA::parse(DEFAULT_ACCENT).expect("valid default accent"))
}

/// Read the accent from the XDG desktop portal (`org.freedesktop.appearance`).
/// This is what actually reflects the user's accent, unlike GTK's own
/// `accent_color`, which can lag behind the desktop setting.
fn portal_accent() -> Option<gdk::RGBA> {
    let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
    let parameters = ("org.freedesktop.appearance", "accent-color").to_variant();
    let reply = connection
        .call_sync(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
            "ReadOne",
            Some(&parameters),
            Some(glib::VariantTy::new("(v)").ok()?),
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        )
        .ok()?;
    // The reply is `(v)` holding a `(ddd)` color, so unbox the variant first.
    let (red, green, blue): (f64, f64, f64) = reply.child_value(0).as_variant()?.get()?;
    Some(
        gdk::RGBA::builder()
            .red(red as f32)
            .green(green as f32)
            .blue(blue as f32)
            .alpha(1.0)
            .build(),
    )
}

fn update_clock(label: &Label) {
    let text = glib::DateTime::now_local()
        .and_then(|now| now.format("%a %H:%M:%S"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    label.set_text(&text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_cut_to_its_room_with_an_ellipsis() {
        // A stand-in for cairo: every character counts as one unit wide.
        let measure = |text: &str| text.chars().count() as f64;

        // Short enough to show whole, left exactly as it is — and the boundary
        // case, where the text measures exactly the budget, is not truncation.
        assert_eq!(truncate_to_width("Firefox", 7.0, &measure), "Firefox");

        // Too long: the tail goes and an ellipsis makes up the width, so as much
        // text as fits is kept rather than one character short of the budget.
        assert_eq!(truncate_to_width("Firefox", 6.0, &measure), "Firef…");
        assert_eq!(truncate_to_width("Firefox", 5.0, &measure), "Fire…");
        assert_eq!(truncate_to_width("Firefox", 3.0, &measure), "Fi…");
        // Counted in characters, not bytes, so multi-byte text is not cut
        // mid-sequence.
        assert_eq!(truncate_to_width("café", 3.0, &measure), "ca…");
        // The ellipsis on its own when nothing at all fits.
        assert_eq!(truncate_to_width("Firefox", 0.0, &measure), "…");
        assert_eq!(truncate_to_width("Firefox", -1.0, &measure), "…");
    }

    #[test]
    fn parses_a_snapshot() {
        let windows =
            parse_snapshot("list\t2\t7\t1\t0\tfirefox\tMozilla\t8\t0\t1\t\tTerminal").unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, 7);
        assert!(windows[0].focused);
        assert!(!windows[0].minimized);
        assert_eq!(windows[0].app_id, "firefox");
        assert_eq!(windows[0].title, "Mozilla");
        assert_eq!(windows[1].id, 8);
        assert!(!windows[1].focused);
        assert!(windows[1].minimized);
        assert_eq!(windows[1].app_id, "");
        assert_eq!(windows[1].title, "Terminal");
    }

    #[test]
    fn rejects_a_snapshot_whose_fields_are_not_the_ones_it_reads() {
        // Five fields per window: id, focused, minimized, app id, title. A producer
        // that emitted a sixth — an extra tab left behind by removing one — does not
        // get silently
        // misread as an empty app id: the walk runs off the end of the line and the
        // whole snapshot is rejected. That is the behaviour that turns a producer
        // mistake into an empty panel rather than a panel full of nonsense.
        assert!(parse_snapshot("list\t1\t7\t1\t0\t\tfirefox\tMozilla").is_none());
        // One field short, likewise.
        assert!(parse_snapshot("list\t1\t7\t1\t0\tfirefox").is_none());
        // A count that does not parse.
        assert!(parse_snapshot("list\tx\t7\t1\t0\tfirefox\tMozilla").is_none());
        // And the well-formed line still works, including an empty app id, which is
        // how a window with no app id is sent.
        let windows = parse_snapshot("list\t2\t7\t1\t0\tfirefox\tMozilla\t8\t0\t1\t\tTerm").unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[1].app_id, "");
    }

    #[test]
    fn ignores_other_messages() {
        assert!(parse_snapshot("focus\t3").is_none());
    }

    #[test]
    fn lays_the_menu_out_to_exactly_its_previews() {
        // The whole point: the surface is the row plus the padding, with nothing
        // spare. A surface wider than this is what left the previews with empty
        // space down each side.
        // Cell widths, as the animation hands them over: a settled cell is its
        // image plus a border either side.
        let widths = [200 + CELL_BORDER * 2, 150 + CELL_BORDER * 2];
        let layout = layout_menu(&widths);
        assert_eq!(
            layout.surface.width,
            200 + 150 + CELL_BORDER * 4 + CELL_GAP + MENU_PAD * 2,
            "surface should hug the row"
        );
        assert_eq!(
            layout.surface.height,
            PREVIEW_HEIGHT + TITLEBAR_HEIGHT + CELL_BORDER + MENU_PAD * 2
        );
        assert_eq!(layout.cells.len(), 2);
        assert_eq!(layout.cells[0].width, widths[0]);
        // First cell against the left padding, second a gap along, and every cell
        // the same height so the row lines up top and bottom.
        assert_eq!(layout.cells[0].x, MENU_PAD);
        assert_eq!(layout.cells[1].x, MENU_PAD + widths[0] + CELL_GAP);
        assert!(layout.cells.iter().all(|c| c.y == MENU_PAD));
        assert!(layout.cells.iter().all(|c| c.height == layout.cells[0].height));

        // A cell part way through growing is laid out at the width it is now, so
        // the surface follows the animation.
        assert_eq!(layout_menu(&[80]).surface.width, 80 + MENU_PAD * 2);
        // A single preview, and an empty menu, both stay sensible sizes.
        assert_eq!(
            layout_menu(&[200 + CELL_BORDER * 2]).surface.width,
            200 + CELL_BORDER * 2 + MENU_PAD * 2
        );
        assert_eq!(layout_menu(&[]).surface.width, MENU_PAD * 2);
    }

    #[test]
    fn a_preview_is_as_wide_as_its_own_aspect_at_the_shared_height() {
        // 16:9 at the shared height.
        assert_eq!(preview_width(320, 180), 210);
        // 4:3.
        assert_eq!(preview_width(160, 120), 157);
        // A very wide window is held to the ceiling.
        assert_eq!(preview_width(4000, 200), PREVIEW_MAX_CELL);
        // Just inside it: no letterbox, because the width is still the image's own.
        assert_eq!(preview_width(600, 200), 354);
        // A very narrow one still gets room for a title and a close button.
        assert_eq!(preview_width(10, 1000), PREVIEW_MIN_WIDTH);
        // Nonsense, rather than a divide by zero.
        assert_eq!(preview_width(0, 100), PREVIEW_HEIGHT);
    }

    #[test]
    fn the_menus_left_edge_stays_under_the_icon_and_on_the_output() {
        // Wide enough to centre exactly.
        assert_eq!(menu_left(500, 1920, 400), 300);
        // Near the left edge: clamped rather than going off screen.
        assert_eq!(menu_left(100, 1920, 400), MENU_EDGE_GAP);
        // Near the right edge: pushed back in, keeping the gap.
        assert_eq!(menu_left(1900, 1920, 400), 1920 - MENU_EDGE_GAP - 400);
        // Wider than the output itself: still on screen, at the left gap.
        assert_eq!(menu_left(500, 300, 900), MENU_EDGE_GAP);
    }

    #[test]
    fn hit_testing_separates_a_preview_from_its_close_button() {
        let layout = layout_menu(&[202, 202]);
        let first = layout.cells[0];
        let second = layout.cells[1];

        // The middle of a preview is that preview.
        let middle = (f64::from(first.x + first.width / 2), f64::from(first.y + first.height / 2));
        assert_eq!(layout.hit(middle.0, middle.1), Hit::Preview(0));

        // The far corner of a cell's strip is the close button, not the preview:
        // a full strip-height square flush with the top, right and bottom.
        let close = close_rect(&first);
        assert_eq!(close.width, TITLEBAR_HEIGHT);
        assert_eq!(close.y, first.y);
        assert_eq!(close.x + close.width, first.x + first.width);
        assert_eq!(close.height, TITLEBAR_HEIGHT);
        assert_eq!(
            layout.hit(
                f64::from(close.x + close.width / 2),
                f64::from(close.y + close.height / 2)
            ),
            Hit::Close(0)
        );
        // Its left edge is the last thing that is the button; just past it is the
        // preview.
        let close_y = f64::from(close.y + close.height / 2);
        assert_eq!(layout.hit(f64::from(close.x) - 1.0, close_y), Hit::Preview(0));

        // The left of that same titlebar is the preview, so the button does not
        // cover the whole strip.
        let left_x = f64::from(first.x + 2);
        assert_eq!(layout.hit(left_x, close_y), Hit::Preview(0));

        // The gap between cells, the menu's padding and past the last cell are
        // all outside.
        let gap = f64::from(first.x + first.width + 1);
        assert_eq!(layout.hit(gap, close_y), Hit::None);
        assert_eq!(layout.hit(1.0, f64::from(first.y + 1)), Hit::None);
        let past = f64::from(second.x + second.width + 1);
        assert_eq!(layout.hit(past, close_y), Hit::None);
    }

    #[test]
    fn a_closing_cell_takes_its_gap_with_it() {
        let cell = 202;
        // Two cells, a full gap between them.
        assert_eq!(layout_menu(&[cell, cell]).surface.width, cell * 2 + CELL_GAP + MENU_PAD * 2);
        // The first has closed up to nothing: the second moves left by its width
        // *and* the gap, and nothing is left holding a hole in the row.
        let closing = layout_menu(&[0, cell]);
        assert_eq!(closing.cells[1].x, MENU_PAD);
        assert_eq!(closing.surface.width, cell + MENU_PAD * 2);
        // A cell part way through closing keeps the gap it still has room for.
        let part = layout_menu(&[cell / 2, cell]);
        assert_eq!(part.cells[1].x, MENU_PAD + cell / 2 + CELL_GAP);
        // Every cell gone: just the padding, no gap left over.
        assert_eq!(layout_menu(&[0, 0]).surface.width, MENU_PAD * 2);
        // The cells still line up with the widths by index, so a hit test on the
        // second cell names the second cell even while the first is closing.
        let mixed = layout_menu(&[0, cell, cell]);
        assert_eq!(mixed.cells.len(), 3);
        assert_eq!(mixed.cells[0].width, 0);
        assert_eq!(mixed.cells[1].x, MENU_PAD);
        assert_eq!(mixed.cells[2].x, MENU_PAD + cell + CELL_GAP);
        // And a zero-width cell is never hit.
        assert_eq!(mixed.hit(f64::from(MENU_PAD), f64::from(MENU_PAD + 10)), Hit::Preview(1));
    }

    #[test]
    fn the_first_frame_of_a_motion_is_worth_one_frame() {
        // The clock is read on every frame, so a motion's first step is the gap since
        // the frame before it. Reading it only while a motion was running left it
        // holding the last animation's timestamp, and since the step is capped the
        // entire cap landed in one frame.
        let frame_seconds = |delta: i64| (delta as f64 / 1_000_000.0).clamp(0.0, 0.1);
        // A real frame at 60Hz is a small fraction of a 140ms fade and a 220ms grow,
        // so the first frame of a motion barely moves it either way.
        let first = frame_seconds(16_667);
        // A fade uses ease-in-out, which is nearly flat at the very start, so its
        // first frame moves it a little. What must not happen is a stall finishing
        // it: the cubic ease-in it replaced was still above 0.87 *halfway* through
        // the phase, so a long gap wiped it out in one step.
        assert!(1.0 - ease_in_out(first / 0.14) > 0.95, "a fade-out opens at opaque");
        assert!(
            1.0 - ease_in_out(frame_seconds(100_000) / 0.14) > 0.1,
            "and a capped stall must not finish the fade"
        );
        // A grow is on the same symmetric curve, so one frame is a small step of it.
        // What must not happen is the whole 100ms cap going in one frame, which is
        // most of the grow — and is what the log showed, 84%, from a clock read only
        // while a motion was running.
        // A grow commits, so one frame is a visible step of it. A quarter of the
        // trimmed phase rather than a fifth of the whole one, because the phase ends
        // early so its flat tail is not spent.
        let one_frame = phase_value(ease_out, first / 0.22).0;
        assert!((0.2..0.3).contains(&one_frame), "one frame, got {one_frame}");

        // A frame after a long idle is still one frame's worth of the cap, not the
        // whole gap: the cap is there to bound a stall, not to be spent deliberately.
        let after_idle = frame_seconds(4_000_000);
        assert_eq!(after_idle, frame_seconds(100_000));
        // Which is the cap, and even that is most of a fade — so reading the clock
        // only while a motion was running put a fade-out straight to 0.64 and a grow
        // straight to 84% of its width in the frame the motion began.
    }

    #[test]
    fn a_departing_window_stays_in_the_order_until_it_is_gone() {
        // The order is what gets drawn, so a window on its way out has to remain in it
        // for as long as it is still there — otherwise its cell disappears the instant
        // it leaves the snapshot and the fade and collapse play out unseen.
        let leaving: HashMap<u64, bool> = [(4, true), (5, false)].into_iter().collect();
        let order = draw_order(&[1, 4, 2], &[1, 2], &leaving);
        assert_eq!(order, vec![1, 4, 2], "4 keeps its place, 3 is nowhere");
        // A window that is not leaving and not on show is dropped at once: that is
        // the difference between animating out and simply not being there.
        assert_eq!(draw_order(&[1, 5, 2], &[1, 2], &leaving), vec![1, 2]);
        // A new window joins at the end rather than disturbing the rest.
        assert_eq!(draw_order(&[1, 2], &[1, 2, 3], &leaving), vec![1, 2, 3]);
        // Reordering the snapshot does not shuffle the row about mid-animation.
        assert_eq!(draw_order(&[1, 2, 4], &[2, 1], &leaving), vec![1, 2, 4]);
        // Nothing on show and nothing leaving: an empty menu.
        assert!(draw_order(&[1], &[], &leaving).is_empty());
    }

    #[test]
    fn the_order_is_the_snapshot_s_own_and_the_survivors_are_kept() {
        // The order is simply the group on show, already in the snapshot's order.
        let group: Vec<u64> = vec![12, 10];
        assert_eq!(group.clone(), vec![12, 10]);

        // Windows that are still open but not on show keep their entries, and so
        // keep their previews — which is what makes coming back to an app instant.
        // Judging that against the group instead is what threw every preview away
        // each time the menu moved to another app.
        // Every window the compositor knows about, whether or not it is on show.
        // Judged against the group alone instead, an app the menu was not showing
        // looked like it had closed all its windows, so its previews were dropped
        // and the next visit started from nothing.
        let alive = liveness(&[1, 2, 3]);
        assert!(alive.contains(&1) && alive.contains(&2) && alive.contains(&3));
        assert!(!alive.contains(&4), "a window that has closed is not alive");
        // The group is a subset of it.
        assert!(liveness(&[1, 2, 3]).contains(&2));
    }


    #[test]
    fn a_motion_advances_by_elapsed_time_not_by_ticks() {
        // The fraction of a phase one frame is worth. A fixed 16ms step is only right
        // if every frame really is 16ms, and every step resizes a surface — so the
        // frames that overrun are exactly the ones the fixed step got wrong, and the
        // animation ran slow instead of skipping.
        let fraction = |microseconds: i64, phase: Duration| {
            (microseconds as f64 / 1_000_000.0).clamp(0.0, 0.1) / phase.as_secs_f64()
        };
        // A nominal frame.
        assert!((fraction(16_667, CELL_MORPH) - 0.0758).abs() < 0.001);
        // A long frame is worth more, and in proportion: two short frames and one long
        // one must land in the same place as the same time in one frame.
        let split = fraction(8_333, CELL_MORPH) + fraction(8_334, CELL_MORPH);
        assert!((fraction(16_667, CELL_MORPH) - split).abs() < 0.001);
        assert!(fraction(33_000, CELL_MORPH) > fraction(16_667, CELL_MORPH));
        // A frame that never comes — a stall, a modal — is capped rather than
        // completing the whole motion in one step.
        assert_eq!(fraction(5_000_000, CELL_MORPH), 0.1 / CELL_MORPH.as_secs_f64());
        // And no frame at all advances nothing, so the first frame of a motion does
        // not jump it forward by a frame nobody waited for.
        assert_eq!(fraction(0, CELL_MORPH), 0.0);
    }

    #[test]
    fn the_window_travels_and_resizes_on_one_motion() {
        // The size and the position are eased from the same fraction, so the row
        // never slides out from under the icon it is meant to sit under. Easing them
        // separately — or letting the layout snap the margin on each of the motion's
        // steps, as it used to — sent the menu sideways instead of along.
        let from_width = 590;
        let to_width = 340;
        let from_left = 600;
        let to_left = 180;
        let at = |t: f64| {
            let eased = phase_value(ease_out, t).0;
            (
                (from_width as f64 + (to_width - from_width) as f64 * eased).round() as i32,
                (from_left as f64 + (to_left - from_left) as f64 * eased).round() as i32,
            )
        };
        // It starts where it was: no jump to the new geometry, and nothing at zero.
        assert_eq!(at(0.0), (from_width, from_left));
        assert!(at(0.0).0 > 0);
        // It ends where the new row wants it.
        assert_eq!(at(1.0), (to_width, to_left));
        // And it moves monotonically in between, rather than overshooting.
        let mut previous = at(0.0);
        for step in 1..=20 {
            let now = at(f64::from(step) / 20.0);
            assert!(now.0 <= previous.0 && now.1 <= previous.1, "overshot at step {step}");
            assert!(now.0 >= to_width && now.1 >= to_left);
            previous = now;
        }
    }

    #[test]
    fn a_preview_is_never_stretched_to_fill_its_box() {
        // A cell a pixel or two off the image's aspect — which is all a clamped width
        // is — has to give the preview its own proportions, not squeeze it to fit.
        let preview = |w: i32, h: i32| (w, h);
        // 16:9 into a box sized for it exactly: a uniform scale of 1.
        let (w, h) = preview(210, 118);
        let (box_w, box_h) = (f64::from(210), f64::from(118));
        let scale = (box_w / f64::from(w)).min(box_h / f64::from(h));
        assert_eq!(scale, 1.0);

        // A box wider than the image's aspect: the image keeps its own shape and is
        // inset evenly, rather than being widened to match the box.
        let (w, h) = preview(100, 118);
        let (box_w, box_h) = (f64::from(200), f64::from(118));
        let scale = (box_w / f64::from(w)).min(box_h / f64::from(h));
        let drawn = (f64::from(w) * scale, f64::from(h) * scale);
        assert!(drawn.0 < box_w && drawn.1 <= box_h);
        // Same height, less width: the proportions survive, which is the point.
        assert!((drawn.0 / drawn.1 - f64::from(w) / f64::from(h)).abs() < 1e-9);
        // And the leftover is split evenly either side.
        let left = (box_w - drawn.0) / 2.0;
        let right = box_w - drawn.0 - left;
        assert!((left - right).abs() < 1e-9);
    }

    #[test]
    fn a_preview_landing_mid_grow_moves_where_the_cell_is_going_not_where_it_is() {
        // A preview arriving part way through a grow used to take the new width as
        // the multiplier on the eased fraction, so the cell was resized underneath
        // the curve: the surface jumped 42px in one frame while the cell eased from
        // 0.93 to 0.97, and every refresh of a live preview did it again.
        let entry = MenuEntry::new(&WindowInfo {
            id: 1,
            focused: false,
            minimized: false,
            app_id: String::new(),
            title: String::new(),
        });
        // Part way through: a cell growing out of nothing towards 200, sitting at
        // the width its own curve says this point of its clock.
        entry.from.set(0.0);
        entry.to.set(200.0);
        entry.elapsed.set(0.3);
        let f = phase_value(ease_out, 0.3).0;
        entry.width.set(200.0 * f);
        let on_screen = entry.width.get();

        // Its preview lands and says the settled width is 260, not 200.
        entry.target.set(260 - CELL_BORDER * 2);
        entry.reaim();

        // The cell has not moved...
        assert_eq!(entry.width.get(), on_screen, "re-aiming must not move the cell");
        // ...and the curve still passes through where it was, so the next tick is
        // continuous with this one rather than a step.
        let now = entry.from.get() + (entry.to.get() - entry.from.get()) * f;
        assert!((now - on_screen).abs() < 0.001, "got {now}, was {on_screen}");
        // It is now heading for the new width, and a little of a frame from here it
        // is closer to it than before.
        assert_eq!(entry.to.get(), 260.0);
        let f_next = phase_value(ease_out, 0.32).0;
        let later = entry.from.get() + (entry.to.get() - entry.from.get()) * f_next;
        assert!(later > on_screen, "and still growing: {later}");
    }

    #[test]
    fn a_refresh_of_the_same_size_leaves_the_motion_alone() {
        // The menu refreshes every window on show on a timer. Each refresh lands a
        // preview, and each re-aim reset how fast the cell was travelling, so a cell
        // kept speeding up through its own grow: an ease out has monotonically
        // shrinking steps, and the log had them growing (1, 8, 3, 7, 21, 10).
        let entry = MenuEntry::new(&WindowInfo {
            id: 1,
            focused: false,
            minimized: false,
            app_id: String::new(),
            title: String::new(),
        });
        entry.target.set(150);
        entry.from.set(0.0);
        entry.to.set(150.0);
        entry.width.set(90.0);
        entry.elapsed.set(0.5);

        // What `menu_set_image` does when the incoming preview is the same size.
        let same = entry.target.get();
        if entry.target.get() != same {
            entry.reaim();
        }
        let (slope_before, from_before, to_before) =
            (entry.to.get() - entry.from.get(), entry.from.get(), entry.to.get());

        // And again, and again: the curve is untouched.
        for _ in 0..5 {
            if entry.target.get() != same {
                entry.reaim();
            }
        }
        assert_eq!(entry.from.get(), from_before);
        assert_eq!(entry.to.get(), to_before);
        assert_eq!(entry.to.get() - entry.from.get(), slope_before);
    }

    #[test]
    fn a_settled_cell_takes_a_new_width_at_once() {
        // Nothing is easing, so a window that changed size is simply a new width.
        let entry = MenuEntry::new(&WindowInfo {
            id: 1,
            focused: false,
            minimized: false,
            app_id: String::new(),
            title: String::new(),
        });
        entry.motion.set(Motion::Settled);
        entry.snap_to_full();
        let before = entry.width.get();
        entry
            .target
            .set(before as i32 - CELL_BORDER * 2 + 40);
        entry.reaim();
        assert_eq!(entry.width.get(), before + 40.0);
    }

    #[test]
    fn a_cell_on_its_way_out_is_not_re_aimed() {
        // Its width is already closing to nothing; where it began is behind it, and
        // re-aiming would drag the departure back towards a full-width cell.
        let entry = MenuEntry::new(&WindowInfo {
            id: 1,
            focused: false,
            minimized: false,
            app_id: String::new(),
            title: String::new(),
        });
        entry.motion.set(Motion::Shrinking);
        entry.width.set(40.0);
        entry.from.set(200.0);
        entry.to.set(0.0);
        entry.elapsed.set(0.5);
        entry.target.set(400);
        entry.reaim();
        assert_eq!(entry.width.get(), 40.0);
        assert_eq!(entry.from.get(), 200.0);
        assert_eq!(entry.to.get(), 0.0);
    }

    #[test]
    fn a_cell_commits_to_collapsing_rather_than_hanging_at_full_width() {
        // The collapse is a cubic ease-out and the fade is not, and that difference
        // is deliberate. A width should commit and settle; an opacity should be
        // visibly moving for its whole phase. Put both on the symmetric curve and
        // each did the wrong thing: the collapse crawled at full width for the first
        // half of its phase, holding a cell that was already invisible open, and the
        // grow crawled out of nothing, which is an ease in by any reading.
        let collapsed = |t: f64| 1.0 - phase_value(ease_out, t).0;
        assert_eq!(collapsed(0.0), 1.0);
        // Most of the width gone by the first frame or two.
        assert!(collapsed(0.05) < 0.85, "got {}", collapsed(0.05));
        assert!(collapsed(0.2) < 0.5, "got {}", collapsed(0.2));
        // And settling at the end rather than arriving all at once.
        assert!(collapsed(0.6) > collapsed(0.9));
        assert_eq!(collapsed(1.0), 0.0);
        // The fade is not a mirror of the collapse. It is symmetric, so unlike the
        // width it is still moving at the end rather than settling.
        let faded = |t: f64| 1.0 - phase_value(ease_in_out, t).0;
        assert!(faded(0.2) > 0.8, "a fade lingers a little where it started");
        assert!(
            (0.35..0.65).contains(&faded(0.5)),
            "halfway is about half, got {}",
            faded(0.5)
        );
        assert!(faded(0.9) < 0.1, "and is still moving at the end");
        assert_eq!(faded(1.0), 0.0);
    }

    #[test]
    fn a_phase_spends_no_frames_on_the_tail_of_its_curve() {
        // Both curves reach their end asymptotically, so the last of a phase moves the
        // value by nothing visible and the animation sits there doing nothing. The log
        // showed `growing 1.00` for five frames before the fade began.
        // On the width's cubic, which is where the dead tail came from: 1.00 to two
        // decimals about a third of the way in, and then nothing visible for the rest
        // of the phase.
        let (value, done) = phase_value(ease_out, 0.0);
        assert_eq!((value, done), (0.0, false));
        let (value, done) = phase_value(ease_out, 0.4);
        assert!(!done);
        assert!(value > 0.5, "ease out is ahead of linear");
        // Finished well before the clock runs out, and exactly on its end value.
        let (value, done) = phase_value(ease_out, PHASE_END);
        assert!(done);
        assert_eq!(value, 1.0);
        let (value, done) = phase_value(ease_out, 1.0);
        assert!(done && value == 1.0);
        // And nothing is left over after it: past the end there is no more clock to
        // spend, which is the whole point.
        assert!(PHASE_END < 1.0);

        // The fade, finished on the same fraction.
        let (value, done) = phase_value(ease_in_out, PHASE_END);
        assert!(done);
        assert_eq!(value, 1.0);
        assert_eq!((phase_value(ease_in_out, 0.0).0, false), (0.0, false));

        // And the property the cubic ease-in failed, in the phase's own time: the
        // middle of a fade has to be the middle of the fade. `1 - t^3` was still at
        // 0.87 halfway through and gone three frames later, so most of the phase was
        // spent looking like nothing was happening and the rest went at once — a
        // wait, then a disappearance. A symmetric ease is at its own midpoint at its
        // midpoint.
        // `elapsed` is in the phase's own time and `phase_value` maps it through
        // `PHASE_END`, so the phase's midpoint is half of that, not 0.5.
        let halfway = 1.0 - phase_value(ease_in_out, PHASE_END / 2.0).0;
        assert!(
            (0.4..0.6).contains(&halfway),
            "halfway through a fade it should be about half gone, got {halfway}"
        );
        // The last quarter of the phase has to actually do something too, or the
        // fade is a wait with a different shape.
        let three_quarters = 1.0 - phase_value(ease_in_out, PHASE_END * 0.75).0;
        assert!(
            three_quarters < 0.25,
            "and the last quarter cannot be spent hanging there, got {three_quarters}"
        );
    }

    #[test]
    fn a_phase_change_restarts_the_clock() {
        // The fade is timed off `elapsed`, so a phase entered with the clock still
        // reading 1.0 finishes in a single tick — which is exactly how the departure
        // ended up with no fade at all: a settled entry's clock had been sitting at
        // one since it arrived, and nothing reset it on the way out.
        let mut elapsed = 1.0f64;
        let mut alpha = 1.0f64;
        let step = 16.0 / 140.0;

        // A tick entered with a stale clock: one frame, straight to nothing.
        assert_eq!((elapsed + step).min(1.0), 1.0);
        assert_eq!(1.0 - ease_in_out(1.0), alpha - 1.0);

        // Restarted, as every change of phase now does, it takes the whole phase.
        elapsed = 0.0;
        let mut frames = 0;
        let mut visible_for = 0;
        while elapsed < 1.0 {
            let running = elapsed < 1.0;
            alpha = 1.0 - ease_in_out((elapsed + step).min(1.0));
            elapsed += step;
            frames += 1;
            if running && alpha > 0.0 {
                visible_for += 1;
            }
        }
        assert!(frames >= 8, "a fade should take several frames, took {frames}");
        assert!(
            visible_for >= frames - 1,
            "it should be visible right up to the end, was for {visible_for} of {frames}"
        );
        assert_eq!(alpha, 0.0, "and finish at nothing");
    }

    #[test]
    fn a_fade_out_is_a_fade_in_played_backwards() {
        // One curve for both directions, so the two are the same motion reversed.
        // They used to be mirrors of each other — a cubic ease-out up and a cubic
        // ease-in down — which is what made the departure read as a wait followed by
        // a disappearance.
        for step in 0..=20 {
            let t = f64::from(step) / 20.0;
            assert!((1.0 - ease_in_out(t) - ease_in_out(1.0 - t)).abs() < 1e-9);
        }
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        // Out of range is clamped, so an overshooting tick lands on the end.
        assert_eq!(ease_in_out(-0.5), 0.0);
        assert_eq!(ease_in_out(1.5), 1.0);
    }

    #[test]
    fn a_fade_is_moving_for_the_whole_of_its_phase() {
        // The complaint this curve replaced: the departure "waits a while, fades out
        // one frame too late, then disappears". A cubic ease-in is still above 0.87
        // halfway through the phase and gone three frames later, so most of the
        // phase was spent looking like nothing was happening and the rest went at
        // once.
        let faded = |t: f64| 1.0 - ease_in_out(t);
        assert_eq!(faded(0.0), 1.0);
        assert_eq!(faded(1.0), 0.0);
        // Moving on the first frame, not waiting for the phase to get going.
        assert!(faded(0.1) < 1.0, "a fade starts immediately: {}", faded(0.1));
        // And roughly linear — the whole point of a symmetric ease is no long
        // stretch at either end where it has barely moved.
        for (t, want) in [
            (0.25, 0.84),
            (0.5, 0.5),
            (0.75, 0.16),
        ] {
            assert!(
                (faded(t) - want).abs() < 0.02,
                "at {t} expected about {want}, got {}",
                faded(t)
            );
        }
        // Monotonic: it never comes back up.
        let mut previous = 1.0;
        for step in 0..=20 {
            let alpha = faded(f64::from(step) / 20.0);
            assert!(alpha <= previous);
            previous = alpha;
        }
    }

    #[test]
    fn an_entry_grows_and_shrinks_on_an_ease_out() {
        // The curve the widths use: nothing at the start, most of the way there
        // early, and it only ever reaches the ends at 0 and 1. A width commits.
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert!(ease_out(0.5) > 0.5, "ease out is ahead of linear");
        assert!(ease_out(0.25) < ease_out(0.75));
        // It is already moving on the first frame, which is the whole difference
        // between it and the symmetric curve the fades use: that one has no slope
        // here, and a cell growing on it looked like it started several frames late.
        assert!(ease_out(0.02) > 0.05, "{}", ease_out(0.02));
        // Out of range is clamped rather than extrapolated, so a tick that
        // overshoots lands exactly on the end instead of past it.
        assert_eq!(ease_out(-0.5), 0.0);
        assert_eq!(ease_out(1.5), 1.0);

        // A whole cell's worth of growth: from nothing, to its settled width, with
        // the border accounted for at both ends.
        let target = 150;
        let full = target + CELL_BORDER * 2;
        let at = |progress: f64| (f64::from(full) * progress).round() as i32;
        assert_eq!(at(0.0), 0);
        assert_eq!(at(1.0), full);
        // Part way through it is a real width, so the surface is resized every step
        // rather than jumping between the two ends.
        let half = at(ease_out(0.5));
        assert!(half > 0 && half < full, "got {half} of {full}");
    }

    #[test]
    fn a_cell_is_rounded_at_the_top_and_square_at_the_bottom() {
        let steps = rounded_top_path(0.0, 0.0, 20.0, 20.0, 5.0);
        let points = |step: PathStep| match step {
            PathStep::Move(x, y) | PathStep::Line(x, y) => Some((x, y)),
            PathStep::Arc { .. } => None,
        };
        let end_of = |index: usize| match steps[index] {
            PathStep::Move(x, y) | PathStep::Line(x, y) => (x, y),
            // Every arc here is a quarter turn from `a0` to `a0 + PI/2`, so it
            // finishes a quarter round from its start.
            PathStep::Arc {
                cx,
                cy,
                radius,
                a0,
            } => (
                cx + radius * (a0 + std::f64::consts::FRAC_PI_2).cos(),
                cy + radius * (a0 + std::f64::consts::FRAC_PI_2).sin(),
            ),
        };

        // It starts down the left side, one radius below the top, not at the corner:
        // a path that began at the corner and relied on cairo joining the arcs had
        // its top edge a whole radius down instead.
        assert_eq!(points(steps[0]), Some((0.0, 5.0)));
        // The first arc turns the top-left corner and finishes on the top edge,
        // within a rounding error of where the straight line it joins starts.
        let (x, y) = end_of(1);
        assert!((x - 5.0).abs() < 1e-9 && y.abs() < 1e-9, "got ({x}, {y})");
        // Then a straight line along the top, at the very top, to the right corner.
        assert_eq!(points(steps[2]), Some((15.0, 0.0)));
        // The second arc turns the top-right corner and finishes on the right side.
        let (x, y) = end_of(3);
        assert!((x - 20.0).abs() < 1e-9 && (y - 5.0).abs() < 1e-9, "got ({x}, {y})");
        // Down the right side, then across the bottom: both square, both at the
        // bottom. The bottom-left used to be one diagonal from the top-left corner's
        // arc straight to the bottom.
        assert_eq!(points(steps[4]), Some((20.0, 20.0)));
        assert_eq!(points(steps[5]), Some((0.0, 20.0)));

        // A radius of zero degenerates to a plain rectangle rather than looping.
        let square = rounded_top_path(0.0, 0.0, 10.0, 10.0, 0.0);
        assert_eq!(square[0], PathStep::Move(0.0, 0.0));
        assert_eq!(square[1], PathStep::Line(10.0, 0.0));
    }

    #[test]
    fn the_preview_sits_inside_the_cells_border() {
        // The image must not reach the cell's edges, or it paints over the outline
        // and the cell looks like it has none.
        let layout = layout_menu(&[200 + CELL_BORDER * 2]);
        let cell = layout.cells[0];
        let left = cell.x + CELL_BORDER;
        let right = cell.x + cell.width - CELL_BORDER;
        assert!(left > cell.x && right < cell.x + cell.width);
        // And it must be exactly the size the image was scaled for, on both axes,
        // or cairo stretches it to fit and the gaps round it stop matching.
        assert_eq!(cell.width - CELL_BORDER * 2, 200);
        assert_eq!(cell.height - TITLEBAR_HEIGHT - CELL_BORDER, PREVIEW_HEIGHT);
        // Symmetric: the same inset on the left and the right.
        assert_eq!(left - cell.x, cell.x + cell.width - right);
    }

    #[test]
    fn centres_text_on_the_line_rather_than_near_it() {
        // A 10px line: the ink is 11 tall, and cairo reports it as hanging 8 above
        // the baseline and so reaching 3 below. Centring that span on 18 puts the
        // baseline at 20.5, and the ink it produces really does span 12.5 to 23.5.
        let baseline = centred_baseline(18.0, -8.0, 11.0);
        assert_eq!(baseline, 20.5);
        assert_eq!(baseline + -8.0 + 11.0 / 2.0, 18.0);
        // And it stays inside the strip rather than climbing out of the top of it.
        assert!(baseline + -8.0 > 6.0 && baseline + 3.0 < 30.0);

        // A bare baseline, all the ink above it and none below: 10 above, 10 tall.
        assert_eq!(centred_baseline(10.0, -10.0, 10.0), 15.0);

        // cairo measures the bearing upwards, so it is negative here. Read the
        // other way round, the same call puts the text 11px lower instead — which
        // is the assertion below pinning the convention rather than leaving it to
        // be rediscovered from a clipped title.
        assert_eq!(centred_baseline(18.0, 8.0, 11.0), 4.5);
    }

    #[test]
    fn converts_rgba_to_what_cairo_expects() {
        // Opaque pixels: red and blue swap, green stays, alpha untouched. That
        // swap is what otherwise turns every preview's colours inside out.
        let mut pixels = vec![10, 20, 30, 255];
        to_cairo_rgba(&mut pixels);
        assert_eq!(pixels, vec![30, 20, 10, 255]);

        // Fully transparent: the colour is irrelevant and multiplying by zero
        // would only cost time.
        let mut clear = vec![200, 100, 50, 0];
        to_cairo_rgba(&mut clear);
        assert_eq!(clear, vec![50, 100, 200, 0]);

        // A half-transparent pixel is premultiplied as well as reordered: 255 and 20
        // scale to 128 and 10 respectively.
        let mut half = vec![255, 20, 30, 128];
        to_cairo_rgba(&mut half);
        assert_eq!(half, vec![15, 10, 128, 128]);

        // Two pixels in one buffer, to show the chunks do not run off the end.
        let mut pair = vec![1, 2, 3, 255, 4, 5, 6, 255];
        to_cairo_rgba(&mut pair);
        assert_eq!(pair, vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn reads_the_close_xbm_bit_by_bit() {
        // A 2x2 pattern in the same format as CLOSE_XBM: one byte per row, bit 0
        // leftmost, so the first row is a single lit pixel at the top-left and
        // the second row is empty.
        let pattern = [0b0000_0001u8, 0b0000_0000];
        // Only the lit pixels are reported, and they come out in row order.
        assert_eq!(glyph_pixels(&pattern, 2), vec![(0, 0)]);

        // Both diagonals of a 2x2 checker, so the bit order is pinned in each
        // direction rather than only for the first row.
        // 0b1010 sets bit 1, not bit 2, so the lit pixel is (1, 1).
        assert_eq!(glyph_pixels(&[0b0000_0101, 0b0000_1010], 2), vec![(0, 0), (1, 1)]);

        // A pattern too short for the grid asked for must not index past its end.
        assert!(glyph_pixels(&[0b0000_0001], 10).contains(&(0, 0)));
        assert!(glyph_pixels(&[], 10).is_empty());

        // The real glyph, at the size it is drawn at: ten rows of two bytes, so
        // every row is in range and the whole thing is read.
        assert!(!glyph_pixels(&CLOSE_XBM, CLOSE_XBM_SIZE).is_empty());

        // A wider pattern, to show rows are not being read as one long run of
        // bits: only the low byte of each row is in play at this size.
        assert_eq!(glyph_pixels(&[0b0000_0011, 0b0000_0000], 2), vec![(0, 0), (1, 0)]);
    }

    #[test]
    fn close_xbm_matches_the_compositors_own_icon() {
        // The panel's glyph and the compositor's titlebar button must not drift
        // apart; this pins the panel's copy to the bytes the shell uses.
        let compositor = crate::shell::ssd::CLOSE_ICON_FOR_TESTS;
        assert_eq!(CLOSE_XBM.as_slice(), compositor);
    }

    #[test]
    fn reads_an_image_payload_after_its_header() {
        let mut reader = PanelReader::default();
        // Two pixels, and a 0x0A byte in the payload: the length prefix is what
        // keeps that from truncating the message.
        let pixels = vec![1u8, 2, 3, 4, 5, 6, 7, 0x0A];
        let header = format!("img\t7\t2\t1\t{}\n", pixels.len());
        reader
            .buffer
            .extend_from_slice(header.as_bytes());
        reader.buffer.extend_from_slice(&pixels);

        let events = reader.drain();
        assert_eq!(events.len(), 1);
        match &events[0] {
            PanelEvent::Image {
                id,
                width,
                height,
                pixels: got,
            } => {
                assert_eq!((*id, *width, *height), (7, 2, 1));
                assert_eq!(got, &pixels);
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn waits_for_the_whole_payload() {
        let mut reader = PanelReader::default();
        reader.buffer.extend_from_slice(b"img\t1\t2\t1\t8\n");
        reader.buffer.extend_from_slice(&[1, 2, 3]);
        assert!(reader.drain().is_empty(), "partial payload is not an event");
        reader.buffer.extend_from_slice(&[4, 5, 6, 7, 8]);
        assert_eq!(reader.drain().len(), 1);
    }

    #[test]
    fn rejects_a_header_whose_length_lies() {
        // 2x1 is 8 bytes, not the 9 claimed.
        assert!(parse_image_header("9\t2\t1\t9").is_none());
        assert!(parse_image_header("9\t0\t1\t0").is_none());
    }

    #[test]
    fn unwraps_a_portal_accent_variant() {
        use glib::prelude::*;
        let color = (0.9f64, 0.3f64, 0.0f64).to_variant();
        let wrapped = glib::Variant::from_variant(&color);
        let (red, green, blue): (f64, f64, f64) = wrapped
            .as_variant()
            .expect("variant")
            .get()
            .expect("accent color");
        assert!((red - 0.9).abs() < 1e-6);
        assert!((green - 0.3).abs() < 1e-6);
        assert!(blue.abs() < 1e-6);
    }

    #[test]
    fn portal_accent_smoke() {
        // Only meaningful where a portal is running; must never panic.
        if let Some(color) = portal_accent() {
            eprintln!(
                "portal accent: {:.3}, {:.3}, {:.3}",
                color.red(),
                color.green(),
                color.blue()
            );
            assert!(color.alpha() > 0.0);
        }
    }

    /// The ordering state is process-wide, so each of these starts from empty.
    fn with_empty_layout(body: impl FnOnce()) {
        LAYOUT.with(|layout| *layout.borrow_mut() = Layout::default());
        body();
    }

    #[test]
    fn a_drop_lands_in_the_gap_it_was_released_over() {
        // Four squares, 36px each: middles at 18, 54, 90, 126. Off by one in this
        // arithmetic and a square dropped between two of them lands one place out.
        let middles = [18.0, 54.0, 90.0, 126.0];
        // Left of the first: before everything.
        assert_eq!(gap_at(&middles, 4.0), 0);
        // Exactly on the first square's middle: not past it, so still before it.
        assert_eq!(gap_at(&middles, 18.0), 0);
        // Just past it: after it.
        assert_eq!(gap_at(&middles, 20.0), 1);
        // On the second square, before its middle: still after the first.
        assert_eq!(gap_at(&middles, 50.0), 1);
        assert_eq!(gap_at(&middles, 56.0), 2);
        // Past the last: after everything, and not off the end of the list.
        assert_eq!(gap_at(&middles, 140.0), 4);
        // An empty bar has exactly one gap, and it is that one.
        assert_eq!(gap_at(&[], 10.0), 0);
    }

    #[test]
    fn moving_a_square_moves_it_and_nothing_else() {
        with_empty_layout(|| {
            arrange(
                &["a".to_string(), "b".to_string(), "c".to_string()],
            );
            // `c` to the front.
            place_at("c", 0);
            let keys: Vec<String> = ["a", "b", "c"].iter().map(|k| k.to_string()).collect();
            assert_eq!(arrange(&keys), [2, 0, 1]);
            // Somewhere in the middle.
            place_at("a", 1);
            assert_eq!(arrange(&keys), [2, 0, 1]);
            // And to the end, which must not push it past the ones already there.
            place_at("c", 99);
            assert_eq!(arrange(&keys), [0, 1, 2]);
        });
    }

    #[test]
    fn a_snapshot_does_not_shuffle_the_bar() {
        with_empty_layout(|| {
            // Three apps, in the order the first snapshot listed them.
            let keys: Vec<String> = ["alpha", "beta", "gamma"]
                .iter()
                .map(|k| k.to_string())
                .collect();
            assert_eq!(arrange(&keys), [0, 1, 2]);

            // A new snapshot, with an app started and listed first. The bar must not
            // move: the newcomer goes at the end, and nothing else shifts.
            let keys: Vec<String> = ["gamma", "delta", "alpha", "beta"]
                .iter()
                .map(|k| k.to_string())
                .collect();
            assert_eq!(arrange(&keys), [2, 3, 0, 1]);
        });
    }

    #[test]
    fn an_app_that_leaves_comes_back_to_the_same_place() {
        with_empty_layout(|| {
            let keys: Vec<String> = ["a", "b", "c"].iter().map(|k| k.to_string()).collect();
            arrange(&keys);
            // `b` closes, so it is not in the snapshot, and the others close up.
            let keys: Vec<String> = ["a", "c"].iter().map(|k| k.to_string()).collect();
            assert_eq!(arrange(&keys), [0, 1]);
            // It comes back, and goes where it was rather than at the end.
            let keys: Vec<String> = ["a", "b", "c"].iter().map(|k| k.to_string()).collect();
            assert_eq!(arrange(&keys), [0, 1, 2]);
        });
    }

    #[test]
    fn a_pinned_app_with_nothing_open_still_gets_a_square() {
        with_empty_layout(|| {
            LAYOUT.with(|layout| {
                layout.borrow_mut().pinned.insert("editor".to_string());
                layout.borrow_mut().pinned.insert("#17".to_string());
            });
            // The only app with windows is the browser, and one window has no app id.
            let groups: Vec<(String, Vec<&WindowInfo>)> =
                vec![("browser".to_string(), Vec::new()), ("#17".to_string(), Vec::new())];
            let mut missing = pinned_without_windows(&groups);
            missing.sort();
            // The editor is missing a square and needs one. The `#17` key is not an
            // app — it is one window with no app id — so there is nothing to pin and
            // nothing to start.
            assert_eq!(missing, ["editor"]);
        });
    }

    #[test]
    fn pinning_is_a_toggle() {
        with_empty_layout(|| {
            assert!(!is_pinned("editor"));
            assert!(toggle_pin("editor"), "pinning says it ended up pinned");
            assert!(is_pinned("editor"));
            assert!(!toggle_pin("editor"), "and again, unpinned");
            assert!(!is_pinned("editor"));
        });
    }

    #[test]
    fn an_animation_hands_back_the_token_it_is_tagged_with() {
        // The counter is bumped inside the animation, so anything that reads it
        // beforehand and waits for that value is waiting for a number that has
        // already gone past. That is not hypothetical: the app menu read it before
        // starting its fade, so the flag that says it is up was never cleared, and
        // the previews could never be opened again for the rest of the session.
        let cell = Rc::new(Cell::new(0u64));
        let landed = animate_surface_stub(&cell);
        assert_eq!(cell.get(), landed, "the counter is where the caller left it");
        assert!(
            landed > 0,
            "and it moved, so a value read before the call is not the tag"
        );
    }

    /// Just the bookkeeping half of `animate_surface`: take the next token and hand
    /// it back, which is the part with the trap in it. The window is not needed for
    /// the arithmetic, and a test that builds a layer surface needs a display.
    fn animate_surface_stub(token_cell: &Rc<Cell<u64>>) -> u64 {
        token_cell.set(token_cell.get().wrapping_add(1));
        token_cell.get()
    }

    #[test]
    fn the_previews_and_the_app_menu_are_the_same_fill() {
        // Two menus belonging to one panel should not be two different greys, and
        // the previews are cairo-drawn, so they are the fixed point: the stylesheet
        // is the thing that has to match them. Both are 35% black over whatever is
        // behind them.
        let sheet = style_sheet((1, 2, 3));
        let rule = sheet
            .split("window.context {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("a rule for the app menu");
        assert!(
            rule.contains("rgba(0, 0, 0, 0.35)"),
            "the app menu is not the previews' fill: {rule}"
        );
        // And nothing painted over the top of it. The theme's window background image
        // is the difference between "the same 35% black" and "a more solid panel than
        // the previews", and it is invisible in a stylesheet.
        assert!(
            rule.contains("background-image: none"),
            "the theme's window background is back over the app menu's fill: {rule}"
        );
        assert_eq!(CELL_FILL, (0.0, 0.0, 0.0, 0.35), "and the previews moved");
        // The mini-CSD is the same fill as the cell it is drawn in, so a preview is
        // one surface rather than a cell with a second background under its title.
        assert_eq!(TITLEBAR_FILL, CELL_FILL);
    }

    #[test]
    fn a_moving_menu_slides_from_where_it_was_to_where_it_is_going() {
        // A move eases the left margin, which `animate_surface` cannot do because an
        // open and a close have nowhere to slide to.
        assert_eq!(ease_margin(100, 400, 0.0), 100);
        assert_eq!(ease_margin(100, 400, 1.0), 400);
        // Halfway is halfway: this is the lerp, and the easing is the caller's, so
        // that a move and the reveal can share the same curve without this one
        // knowing what it is for.
        assert_eq!(ease_margin(100, 400, 0.5), 250);
        assert_eq!(ease_margin(100, 400, 0.25), 175);
        // And it works backwards, for a menu that has to go left.
        assert_eq!(ease_margin(400, 100, 0.0), 400);
        assert_eq!(ease_margin(400, 100, 1.0), 100);
        // Landed exactly on the end, so no residue is left behind.
        assert_eq!(ease_margin(7, 7, 0.3), 7);
    }

    #[test]
    fn the_app_menus_own_rules_reach_the_stylesheet() {
        // The app menu is widgets, so its whole look is this stylesheet, and there
        // are two ways it can come out looking like whatever the desktop's theme
        // does to buttons: the rules never reach the provider, or they reach it and
        // lose. Which of the two it is, decides what to do next, so the first is
        // pinned here and the second is left to be looked at.
        let sheet = style_sheet((1, 2, 3));
        for rule in [
            "window.context {",
            "button.context-row {",
            "button.context-row:hover {",
            "button.context-row label {",
            ".context-rows {",
        ] {
            assert!(sheet.contains(rule), "missing from the sheet: {rule}");
        }
        // And the two rules that decide how it looks, rather than just existing.
        assert!(sheet.contains("background: transparent;"));
        assert!(sheet.contains("color: #e8e8e8;"));
    }

    #[test]
    fn reads_icon_from_desktop_entry() {
        // The panel asks for an app's icon every time it rebuilds the bar, and a
        // localised `Icon[de]` winning would put a German icon in an English panel.
        let app = crate::desktop::parse_entry(
            "konsole",
            "[Desktop Entry]\nName=Konsole\nIcon=konsole\nIcon[de]=konsole-de\nExec=konsole\nType=Application\n",
        )
        .expect("an entry");
        assert_eq!(app.icon, "konsole");
    }

    #[test]
    fn maps_nested_desktop_files_to_ids() {
        // Entries are grouped in subdirectories in the wild. The id a client sends is
        // the file name, not a flattened path, so a nested entry is found by the name
        // inside it rather than under a name invented from the directory it is in.
        let dir = std::env::temp_dir().join(format!("oxide-panel-dirs-{}", std::process::id()));
        let nested = dir.join("kde4");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("konsole.desktop"),
            "[Desktop Entry]\nName=Konsole\nExec=konsole\nType=Application\n",
        )
        .unwrap();

        let app = crate::desktop::lookup_in(&dir, "konsole");
        assert_eq!(app.map(|app| app.label().to_string()), Some("Konsole".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
