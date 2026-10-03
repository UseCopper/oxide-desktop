//! The desktop panel: a GTK4 client that uses the `wlr-layer-shell` protocol to
//! pin itself to the top of an output.
//!
//! The compositor starts this as a child of itself (`oxide-desktop --panel`),
//! pointed at its own Wayland socket. The list of open windows and focus
//! requests travel over a Unix socket, framed by [`crate::panel_proto`] — the
//! module both halves of that connection share, so the two can never disagree about how
//! a message is delimited.

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

use crate::panel_proto;

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
/// How long a square must be held before the press becomes a drag.
///
/// Or any movement past [`DRAG_SLOP`], whichever comes first. Without the hold, a drag
/// needed movement, and a press that was meant to pick an app up and put it down
/// somewhere else — which is most drags — started by activating the app instead.
const DRAG_HOLD: Duration = Duration::from_millis(250);

/// How long a press may last before it is acted on without waiting for the release.
///
/// Short enough to feel immediate, and long enough that a hand which is still on its way
/// to the pointer does not activate an app it passed over. The reason it exists at all is
/// in `app_button`: the release is delivered to the square that was pressed, and the bar
/// is rebuilt from scratch on every snapshot, so a square rebuilt mid-press never delivers
/// it and the click is lost.
const CLICK_ARM: Duration = Duration::from_millis(120);

/// How often to look for a compositor that is not there yet.
///
/// Short enough that a compositor restart is invisible, long enough not to spin: a
/// `connect` against a socket that is not there fails immediately, so this would
/// otherwise be a busy loop for as long as the compositor is down.
const RECONNECT_INTERVAL: Duration = Duration::from_millis(500);

/// How many times a hover-out close may be put off while the menu is still moving.
///
/// After this the close happens anyway. See `watch_hover`: the deferral exists for a
/// menu being resized under a stationary pointer, and it is bounded because a `morphing`
/// that is never cleared would otherwise defer the close forever and leave the menu
/// impossible to dismiss with the pointer.
const HOVER_DEFER_LIMIT: u8 = 3;
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
/// The mini-CSD's strip. Nothing: the cell's own fill is already under it.
///
/// It was a 7% white wash, which made one preview two greys, and then it was set to
/// [`CELL_FILL`] on the reasoning that the same value would look the same — which is
/// wrong, because the strip is drawn *on top of* the cell fill rather than instead of
/// it. Two 35% blacks are 58% black, so the strip came out a third value, darker than
/// the cell it is part of, and that is what changed.
///
/// Transparent is what "the same background transparency" actually means when the
/// thing under it is already the colour you want. The title and the close square are
/// told apart by the border and the text rather than by a second background.
const TITLEBAR_FILL: (f64, f64, f64, f64) = (0.0, 0.0, 0.0, 0.0);
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
/// How long the pointer must be somewhere the previews do not follow before the menu
/// closes.
///
/// A close scheduled by leaving a square is cancelled by the token if the pointer turns
/// out to be heading for the menu, so this is a floor on how fast a close may happen
/// rather than a penalty added to one — which is why it can be this short.
const HOVER_GRACE: Duration = Duration::from_millis(60);

/// How long the pointer must rest on a square before its menu opens. Long enough
/// that travelling along the bar does not open a menu for every square crossed.
const HOVER_OPEN: Duration = Duration::from_millis(250);
/// How long a press on a square is armed before it acts.
///
/// Long enough that a press which becomes a drag is not also a click — a square
/// cannot be brought forward and picked up at once — and short enough that a click is
/// not left feeling slow. A release beats it, so a plain click acts immediately.
/// How long a press may last before the hold is read as a drag rather than a click.
///
/// The gesture a square answers to: press and let go quickly and it opens its windows,
/// press and hold — or move while pressed — and it is picked up. There is deliberately no
/// hold-to-activate underneath this any more. There used to be one, firing at 180ms, and
/// the two could not both be true: holding a square for a quarter of a second activated
/// the app *and then* picked the square up.

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
    // Clamped to the cell's own width, because a cell mid-morph can be narrower than the
    // titlebar. The square used to extend to `cell.x - N` in that case, which put its
    // left half over the previous cell: the highlight was drawn on the shrinking one
    // while a hit test in the overhang resolved to the previous cell's, so the pointer
    // and the paint disagreed about what was under it.
    let width = TITLEBAR_HEIGHT.min(cell.width);
    Rect {
        x: cell.x + cell.width - width,
        y: cell.y,
        width,
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
    /// What is under the pointer, if every cell is live. Geometry alone — for tests, and
    /// for the cases where nothing is animating.
    #[cfg(test)]
    fn hit(&self, x: f64, y: f64) -> Hit {
        self.hit_where(x, y, |_| true)
    }

    /// [`Self::hit`], skipping cells that `live` says are not there to be pressed.
    ///
    /// Geometry alone is not enough, and this was the difference between a cell that
    /// had faded out and a cell that was gone. A cell keeps its full width for
    /// `CELL_MORPH` after its fade finishes, and keeps its full footprint while it is
    /// *growing*, at zero opacity throughout. Both were still hit, so clicking a preview
    /// as it dissolved re-focused — or, with the close button, closed — a window that was
    /// already on its way out, and clicking the space a preview was about to occupy
    /// pressed it early.
    fn hit_where(&self, x: f64, y: f64, mut live: impl FnMut(usize) -> bool) -> Hit {
        for (index, cell) in self.cells.iter().enumerate() {
            if !cell.contains(x, y) {
                continue;
            }
            // A cell that is not there to be pressed still swallows the point: falling
            // through to the cell behind it would mean pressing one window while aiming
            // at another.
            if !live(index) {
                return Hit::None;
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

/// Squeeze a menu onto the output if it does not fit.
///
/// A row of previews has a natural width, and it is not bounded by the output's: six
/// ordinary 16:9 windows come to more than a 1280px screen between them. Left alone, the
/// menu simply ran off the right edge — the tail of it was off-screen but still part of
/// the surface, so its cells were drawn over nothing and still took clicks.
///
/// Squeezed, not truncated. Narrowing the surface alone would have been worse than doing
/// nothing: the cells past the new edge would be clipped away and become unclickable,
/// which is the same loss of reachability this is here to fix. So every cell is narrowed
/// in proportion and the row stays whole — every window remains visible and pressable,
/// just smaller.
///
/// Below one readable cell's width there is no sensible squeeze left, and the menu is
/// left at its natural size: a bar that narrow is not a real case, and collapsing the
/// menu would help nobody.
fn fit_menu_to_output(layout: &mut MenuLayout, bar_width: i32) {
    let room = bar_width.saturating_sub(MENU_EDGE_GAP * 2);
    if layout.surface.width <= room {
        return;
    }
    let scale = f64::from(room) / f64::from(layout.surface.width);
    // The guard is on what a cell would end up as, not on the scale: squeezing six
    // previews into 24px makes each of them four pixels wide, which is not a preview.
    let widest = layout.cells.iter().map(|cell| cell.width).max().unwrap_or(0);
    if f64::from(widest) * scale < f64::from(PREVIEW_MIN_WIDTH) {
        return;
    }
    for cell in &mut layout.cells {
        cell.x = (f64::from(cell.x) * scale).round() as i32;
        cell.width = ((f64::from(cell.width) * scale).round() as i32).max(1);
    }
    layout.surface.width = room;
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
    stream: Channel,
    /// Ticks the switch has spent waiting for the incoming previews.
    waited: Cell<u32>,
    /// A snapshot arrived while a switch was running, so the row was not brought into
    /// step with it. Set by the switch path of [`menu_replace`], cleared by the resync
    /// that runs when the switch ends.
    ///
    /// Without this, closing a window during a switch lost that update permanently. The
    /// switch owns the row while it runs and skips the prune, and the snapshot that
    /// reported the window has already been consumed — so if nothing else changed
    /// afterwards there was no further snapshot to prune it, and the menu kept showing a
    /// window that had been closed for good. That is why it took hovering along the bar
    /// first: travelling between squares is what starts a switch.
    resync_pending: Cell<bool>,
    /// Bumped whenever the pointer is somewhere the menu should stay open, so a close
    /// scheduled by leaving one square can tell it was superseded.
    ///
    /// Shared by the squares and by the menu's own canvas, because the two are the same
    /// question: is the pointer still on the app whose previews are up?
    hover_token: Cell<u64>,
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
    /// Whether the surface is being held at [`Self::hold_width`], and at what.
    ///
    /// For the last cell leaving. The surface is sized by the row, and the row shrinks
    /// with the cell — so the last cell collapsing took the whole menu to nothing
    /// *before* the close could fade it, and a one-window app closing simply vanished
    /// rather than playing the same close as any other. Held for the length of the
    /// departure, the cell still fades and collapses inside a menu that keeps its
    /// size, and the close has something to fade.
    hold: Cell<bool>,
    hold_width: Cell<i32>,
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
            Some((x, y)) => {
                let layout = self.layout.borrow();
                layout.hit_where(x, y, |index| self.cell_is_live(index))
            }
            None => Hit::None,
        }
    }

    /// Whether the cell at this index can be pressed right now.
    ///
    /// Anything not fully opaque is on its way in or out, and neither is a thing to
    /// click. Deliberately checked for both, rather than only for the shrink: a cell
    /// part way *up* is at full opacity but not yet at its final width, and a cell part
    /// way down is not yet gone.
    fn cell_is_live(&self, index: usize) -> bool {
        let Some(id) = self.order.borrow().get(index).copied() else {
            return false;
        };
        let entries = self.entries.borrow();
        let Some(entry) = entries.get(&id) else {
            return false;
        };
        entry.alpha.get() >= 1.0 && entry.motion.get() == Motion::Settled
    }
}

// ---------------------------------------------------------------- the surface

/// Build the menu: one layer surface with a canvas on it, sized and drawn by us.
fn build_menu(app: &Application, monitor: &gdk::Monitor, stream: Channel) -> Rc<Menu> {
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
        stream: Channel::default(),
        waited: Cell::new(0),
        resync_pending: Cell::new(false),
        hover_token: Cell::new(0),
        left_from: Cell::new(0),
        left_to: Cell::new(0),
        last_frame: Cell::new(0),
        width_asked: Cell::new(0),
        asked: Cell::new((0, 0)),
        width_from: Cell::new(0),
        width_to: Cell::new(0),
        width_elapsed: Cell::new(0.0),
        hold: Cell::new(false),
        hold_width: Cell::new(0),
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
            let target = match clicked
                .layout
                .borrow()
                .hit_where(x, y, |index| clicked.cell_is_live(index))
            {
                Hit::Close(index) => clicked.order.borrow().get(index).copied().map(|id| (id, true)),
                Hit::Preview(index) => {
                    clicked.order.borrow().get(index).copied().map(|id| (id, false))
                }
                Hit::None => None,
            };
            let Some((id, close)) = target else {
                return;
            };
            if close {
                // Only that window, and the menu stays: you are picking the next one.
                stream.send(&format!("close\t{id}"));
            } else {
                // Focusing a window is choosing it, so the menu goes away.
                stream.send(&format!("focus\t{id}"));
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
    let layout = menu.layout.borrow().clone();
    let cell_count = layout.cells.len();
    let surface = layout.surface;
    // What the panel believes it is showing, every frame, under OXIDE_PANEL_DEBUG.
    //
    // A menu that is on screen with nothing in it has a cause that is not visible in the
    // picture: an entry can be at zero opacity, or absent, or present with no preview.
    // Those look identical on screen and are fixed in completely different places, and
    // reading the code is not enough to tell them apart — this line is.
    if menu_debug() {
        let entries = menu.entries.borrow();
        let cells: Vec<String> = menu
            .order
            .borrow()
            .iter()
            .map(|id| {
                match entries.get(id) {
                    None => format!("{id}:gone"),
                    Some(entry) => format!(
                        "{id}:{:?}/{:.2}w{}p{}",
                        entry.motion.get(),
                        entry.alpha.get(),
                        entry.width.get(),
                        if entry.preview.borrow().is_some() { '+' } else { '-' }
                    ),
                }
            })
            .collect();
        eprintln!(
            "oxide-panel: draw {}x{} surface {}x{} cells {} entries {} order[{}]",
            width,
            height,
            surface.width,
            surface.height,
            cell_count,
            entries.len(),
            cells.join(" ")
        );
    }
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
    // Halve, measure, then bisect. The obvious version — shave one character off and
    // remeasure — is quadratic in the title's length and `measure` is a Pango layout, so
    // it is not a cheap operation. A title is whatever a client passed to `set_title`,
    // and this runs on every repaint of every preview cell, so a client that set a long
    // one could take the panel's main loop down with it.
    let chars: Vec<char> = text.chars().collect();
    let fits = |count: usize| {
        let mut candidate: String = chars[..count].iter().collect();
        candidate.push(TITLE_ELLIPSIS);
        measure(&candidate) <= budget
    };
    let (mut low, mut high) = (0usize, chars.len());
    while low < high {
        // The midpoint, rounded up, so the search always makes progress.
        let middle = low + (high - low).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    if low == 0 {
        return TITLE_ELLIPSIS.to_string();
    }
    let mut out: String = chars[..low].iter().collect();
    out.push(TITLE_ELLIPSIS);
    out
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
    // The departure, as opposed to a switch: the row is emptying and its width says
    // so, but the menu is on its way out by the same animation as any other close and
    // deserves the same surface to fade.
    if menu.hold.get() {
        layout.surface.width = menu.hold_width.get().max(MENU_PAD * 2);
    }
    // Nothing on screen may be wider than the output. The layout's own width is the sum
    // of the row and has no idea how wide the display is, so a menu showing enough
    // windows to overflow it used to run off the right edge: the surplus was off-screen,
    // drawn over nothing, and still part of the input surface.
    fit_menu_to_output(&mut layout, menu.bar_width.get());
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
                schedule_resync(menu);
            } else {
                moving += 1;
            }
        }
        Switch::Idle => {
            let mut entries = menu.entries.borrow_mut();
            // An entry is dropped once it has closed up to nothing.
            entries.retain(|id, entry| {
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
                            if menu_debug() {
                                eprintln!("oxide-panel: cell {id} dropped");
                            }
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
        schedule_resync(menu);
        return;
    };
    menu.app.replace(Some(pending.key));
    menu.icon_center.set(pending.icon_center);
    menu.bar_width.set(pending.bar_width);
    build_entries(menu, &pending.group, &menu.stream);
    // The width to ease to, from the incoming previews' own aspects. Taken now
    // because a previews-as-it-arrives correction would restart the motion in
    // flight; the surface keeps up through the ordinary layout path afterwards.
    menu.width_to.set(menu.widths_from_layout());
}

/// Apply the snapshots that arrived while a switch was running, now that it is over.
///
/// On an idle rather than inline: this is reached from the draw function, which is
/// holding borrows of the very entries and order a rebuild would want for writing.
fn schedule_resync(menu: &Rc<Menu>) {
    if !menu.resync_pending.replace(false) {
        return;
    }
    let menu = menu.clone();
    glib::idle_add_local_once(move || {
        // Only if it is still the same menu and still open. A rebuild asked for by a pin
        // has already been through `menu_replace` by then, and doing it twice would
        // restart the departure of anything it had just started.
        if menu.app.borrow().is_some() {
            menu_replace_current(&menu);
        }
    });
}

/// Bring the open menu into step with the most recent snapshot.
///
/// The same thing [`rebuild_tasks`] does for an open menu, reachable without a rebuild —
/// a rebuild is driven by a snapshot arriving, and a snapshot that arrived while the row
/// was switched out has nothing left to drive one.
fn menu_replace_current(menu: &Rc<Menu>) {
    let Some(key) = menu.app.borrow().clone() else {
        return;
    };
    let snapshot = SNAPSHOT.with(|cell| cell.borrow().clone());
    let every: Vec<&WindowInfo> = snapshot.iter().collect();
    let group = group_for(&key, &snapshot);
    menu_replace(menu, &every, &group, &menu.stream);
}

/// The windows belonging to one app, as the bar groups them.
///
/// Shared with [`rebuild_tasks`] so the two cannot disagree about what "this app's
/// windows" means — a window with no app id is a group of its own, named after its panel
/// id, and a resync that forgot that would prune a perfectly good cell.
fn group_for<'a>(key: &str, windows: &'a [WindowInfo]) -> Vec<&'a WindowInfo> {
    windows
        .iter()
        .filter(|info| group_key(info) == key)
        .collect()
}

/// The group an app's windows are collected under.
fn group_key(info: &WindowInfo) -> String {
    if info.app_id.is_empty() {
        // A window that never said what it was gets a key of its own so it does not
        // collapse into every other such window.
        format!("#{}", info.id)
    } else {
        info.app_id.clone()
    }
}

/// Put a group of windows in the row, full width and invisible, and ask for any
/// previews they have not sent.
///
/// The opposite of [`menu_replace`], which keeps what is already on show and marks
/// the difference as leaving; this is the clean exchange a switch makes once the
/// old previews have gone.
fn build_entries(menu: &Rc<Menu>, group: &[WindowInfo], stream: &Channel) {
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
    stream: &Channel,
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
        // Not held at the width of whatever was leaving: this is a new app's menu, and a
        // hold left over from a departure that is still in progress would pin the surface
        // to the departing app's width for good. It used to be cleared only on the
        // non-switching path below, so hovering off a square whose app had just closed,
        // within the departure's fade, left the *new* app's row drawn in the old app's
        // surface for the rest of the session.
        menu.hold.set(false);
        // Only the cells that are *at rest* need their clocks restarted, and only those.
        //
        // `Switch::FadingOut` derives each cell's opacity from its clock, so a cell whose
        // clock has already run out — which is every settled cell, since `settle_all`
        // leaves it there — is at zero opacity on the very first tick and the row
        // hard-cuts to blank, then sits blank for the whole exchange. That is why the
        // switch path reset nothing at all, and why the switch looked like a cut.
        //
        // But a cell that is *already* animating has a clock that is doing its job:
        // `FadingOut` derives opacity from the clock and ignores the motion, so
        // restarting one mid-fade would pop it back to opaque and fade it again, and
        // restarting a `Shrinking` one — at zero width and zero alpha — would bring it
        // back as a bright sliver. So: settled cells only.
        {
            let order = menu.order.borrow().clone();
            let mut entries = menu.entries.borrow_mut();
            for id in &order {
                if let Some(entry) = entries.get_mut(id)
                    && entry.motion.get() == Motion::Settled
                {
                    entry.elapsed.set(0.0);
                }
            }
        }
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
    // Not held at the width of whatever was leaving: this is a new app's menu.
    menu.hold.set(false);
    // The whole snapshot, not just this app's windows. With an empty list every
    // entry left over from the app that was on show before looked like a window that
    // had gone, and was animated out *inside* the menu that had just opened — so
    // hovering from one app to another left the previous app's previews sitting in
    // the row beside the new one's, until the switch had finished and something
    // rebuilt it.
    let snapshot = SNAPSHOT.with(|cell| cell.borrow().clone());
    let all: Vec<&WindowInfo> = snapshot.iter().collect();
    menu_replace(menu, &all, group, stream);
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
    sync_tooltips_from(&menu);
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
    stream: &Channel,
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
                if menu_debug() {
                    eprintln!(
                        "oxide-panel: cell {id} {:?} is going",
                        entry.title.borrow()
                    );
                }
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
    // The row, named. Read outside the borrow above, because `entries` is held for
    // writing across it: reading the same cell here is a second borrow while the first is
    // still alive, and a `RefCell` refuses that by panicking. The panic is not caught —
    // this runs inside a GLib trampoline that cannot unwind — so it aborts the panel, and
    // a panel that has aborted does not respond to anything. It only ever happened with
    // OXIDE_PANEL_DEBUG set, which is why it survived.
    if menu_debug() && menu.order.borrow().as_slice() != previous.as_slice() {
        let rows: Vec<String> = {
            let order = menu.order.borrow();
            let entries = menu.entries.borrow();
            order
                .iter()
                .map(|id| {
                    let title = entries
                        .get(id)
                        .map(|entry| entry.title.borrow().clone())
                        .unwrap_or_default();
                    format!("{id}:{title:?}")
                })
                .collect()
        };
        eprintln!("oxide-panel: row is now {}", rows.join(" "));
    }
    // Outside the borrow: asking for a preview reads the entries, and doing that
    // while they are borrowed for writing panics.
    for info in group {
        if wanted.contains(&info.id) {
            request_preview_if_missing(menu, info, stream);
        }
    }
    // The last of an app's windows has gone: hold the surface where it is for the
    // length of the departure, so the menu plays the same close as it would if the
    // pointer had closed it. Without this the row shrinks with the last cell and the
    // surface is down to its padding before the close has anything to fade, so a
    // one-window app simply disappeared.
    if group.is_empty() && menu.shown.get() && !menu.hold.get() {
        menu.hold_width.set(menu.layout.borrow().surface.width);
        menu.hold.set(true);
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
fn request_preview_if_missing(menu: &Rc<Menu>, info: &WindowInfo, stream: &Channel) {
    let has = menu
        .entries
        .borrow()
        .get(&info.id)
        .is_some_and(|entry| entry.preview.borrow().is_some());
    if has {
        return;
    }
    stream.send(&format!(
        // Says the panel has nothing for this window, so the compositor must answer even
        // if the pixels have not moved.
        "preview\t{}\t{}\t{}\t{}",
        info.id, PREVIEW_TARGET.0, PREVIEW_TARGET.1, WANTED
    ));
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
fn menu_refresh(menu: &Rc<Menu>, windows: &[WindowInfo], stream: &Channel) {
    if menu.app.borrow().is_none() {
        return;
    }
    let order = menu.order.borrow().clone();
    for info in windows.iter().filter(|info| order.contains(&info.id)) {
        stream.send(&format!(
            // A refresh: only answer if the window has actually changed.
            "preview\t{}\t{}\t{}\t{}",
            info.id, PREVIEW_TARGET.0, PREVIEW_TARGET.1, REFRESH
        ));
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
    // The hold was for the departure; there is nothing left to hold open for.
    menu.hold.set(false);
    // Whatever the pointer was last over, it is not over a menu that is no longer
    // there.
    menu.pointer.set(None);
    // Cleared here rather than when the animation lands, so the very next click
    // can open it again. Leaving it set is what meant a menu could only ever be
    // opened once per panel run.
    menu.shown.set(false);
    sync_tooltips_from(&menu);
    let width = menu.layout.borrow().surface.width;
    let left = menu_left(menu.icon_center.get(), menu.bar_width.get(), width);
    animate_menu(menu, false, left);
}

/// Whether the preview menu is up and showing.
fn menu_open_now(menu: &Rc<Menu>) -> bool {
    menu.shown.get() && menu.app.borrow().is_some()
}

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
    stream: Channel,
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

fn build_context_menu(app: &Application, monitor: &gdk::Monitor, stream: Channel) -> Rc<ContextMenu> {
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
        stream,
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
    // Not a reason to open an empty menu: with no compositor to talk to there is nothing
    // these rows could do, and a box with nothing in it is a box that looks broken.
    if !context.stream.is_connected() {
        return Vec::new();
    }
    let stream = context.stream.clone();
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
            stream.send(&format!("launch\t{id}"));
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
            row.connect_clicked(move |_| {
                stream.send(&format!("close\t{id}"));
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
                    stream.send(&format!("close\t{id}"));
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

/// Move the app menu: the whole thing fades, and it slides to the new square.
///
/// The whole surface fades — fill, outline and rows together. Fading only the rows
/// and holding the panel was a misreading of "fade everything except the menu": it
/// leaves an empty frame hanging over the desktop with the contents gone, which does
/// not read as a menu going anywhere.
///
/// What this does that [`animate_surface`] does not, is ease the *left* margin. That
/// one eases the window's opacity and the top margin, and sets the left outright,
/// because an open and a close have nowhere to slide to. A move has two squares
/// between them.
#[allow(clippy::too_many_arguments)]
fn context_ease(
    context: &Rc<ContextMenu>,
    from_opacity: f64,
    to_opacity: f64,
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
        // The whole surface, so the fill and the outline fade with the rows.
        //
        // Fading only the rows and holding the panel was a misreading of "fade
        // everything except the menu". It leaves an empty frame hanging over the
        // desktop with the contents gone, which does not read as a menu going
        // anywhere.
        context
            .window
            .set_opacity(from_opacity + (to_opacity - from_opacity) * eased);
        context
            .window
            .set_margin(Edge::Left, ease_margin(from_left, to_left, eased));
        context
            .window
            .set_margin(Edge::Top, ease_margin(from_top, to_top, eased));
        if t < 1.0 {
            return glib::ControlFlow::Continue;
        }
        context.window.set_opacity(to_opacity);
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
fn context_open(context: &Rc<ContextMenu>, key: &str, windows: &[WindowInfo], button: &Button) {
    let (icon_center, bar_width) = icon_metrics(button);
    context.icon_center.set(icon_center);
    context.bar_width.set(bar_width);
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
    sync_tooltips_from_context(&context);
    context.window.present();
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
    sync_tooltips_from_context(context);
    let left = menu_left(
        context.icon_center.get(),
        context.bar_width.get(),
        context.rows.measure(gtk4::Orientation::Horizontal, -1).1,
    );
    let fading = context.clone();
    // The whole menu fades as it slides back up into the bar.
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
    static LAST_BAR: RefCell<Option<(GtkBox, Rc<Menu>, Vec<WindowInfo>, Channel, Rc<ContextMenu>)>> =
        const { RefCell::new(None) };

    /// The bar's overlay, which the carried layer is added to for the length of a drag.
    static OVERLAY: RefCell<Option<gtk4::Overlay>> = const { RefCell::new(None) };
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
        let missing = |id: &String| {
            crate::desktop::is_app_id(id) && !groups.iter().any(|(key, _)| key == id)
        };
        // In the remembered order, so a pinned app that has not been started yet sits
        // where the bar says it should. This used to iterate the pin set directly, which
        // is a hash set: two or more pinned-but-idle apps came out in an order that
        // differs every run, and the next save wrote that shuffle into panel.conf. So a
        // bar quietly rearranged itself across restarts, and the file it wrote was noise.
        let mut out: Vec<String> = layout
            .order
            .iter()
            .filter(|id| layout.pinned.contains(*id) && missing(id))
            .cloned()
            .collect();
        // Pins with no place in the order — a hand-edited file, or an app pinned before it
        // was ever seen — go after those, sorted, so they too land in the same place
        // every run rather than a new one.
        let mut strays: Vec<String> = layout
            .pinned
            .iter()
            .filter(|id| missing(id) && !layout.order.contains(*id))
            .cloned()
            .collect();
        strays.sort();
        out.extend(strays);
        out
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
    let (out, discovered) = LAYOUT.with(|layout| {
        let mut layout = layout.borrow_mut();
        let mut discovered = false;
        for key in keys {
            // A window that never said what it was gets a synthetic key so its windows
            // do not all collapse into one square. It is a position for this session
            // only: the key names a panel id, and panel ids start again at one in every
            // compositor run, so remembering it would give an unrelated window in some
            // later session the place this one had. It must not reach the file.
            if key.starts_with('#') {
                continue;
            }
            if !layout.order.iter().any(|entry| entry == key) {
                layout.order.push(key.clone());
                discovered = true;
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
        (out, discovered)
    });
    // A newly discovered app changes where the squares sit, so the order is worth
    // keeping. It used to be left in memory only, which meant an order the user never
    // touched was forgotten on the next restart and rebuilt from whatever the first
    // snapshot happened to say.
    //
    // Saved outside the borrow above: `save_layout` reads the same cell.
    if discovered {
        save_layout();
    }
    out
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
    // The menu's own window as well as its canvas, so that moving onto the menu counts as
    // moving onto the menu whatever part of it the pointer lands on.
    let surface = menu.window.clone().upcast::<gtk4::Widget>();
    // The bar's row and the menu's canvas, both content widgets rather than
    // toplevels. A toplevel also reports enter and leave when it is resized or
    // reconfigured, which is not the pointer going anywhere, and this menu is
    // resized as previews arrive.
    // Whether this surface is the menu rather than the bar, which decides whether being
    // on it cancels a close that leaving a square scheduled.
    //
    // Only the menu does. The bar does not: leaving a square for the panel *background*
    // is the pointer going somewhere the previews do not follow, and that has to close
    // them. Cancelling it there left the previews on screen with the pointer resting on
    // empty bar.
    for (widget, is_menu) in [(bar, false), (canvas, true), (surface, true)] {
        let motion = gtk4::EventControllerMotion::new();
        // Per iteration: the handler is `Fn` and so borrows its captures.
        let closing_menu = menu.clone();

        // Movement anywhere inside the surface counts as being on it, not just the
        // first event. A surface that resizes while the pointer is still resting on
        // it is handed a leave, and treating that as the pointer having gone is what
        // closed the menu while the pointer never left it.
        let on_move = hover.clone();
        let on_move_menu = menu.clone();
        motion.connect_motion(move |_, _, _| {
            on_move.inside.set(true);
            on_move.token.set(on_move.token.get() + 1);
            if is_menu {
                // Being on the menu counts as still being on the app whose previews it is
                // showing, so it cancels a close that leaving the bar square scheduled.
                on_move_menu.hover_token.set(on_move_menu.hover_token.get() + 1);
            }
        });

        let on_enter = hover.clone();
        let on_enter_menu = menu.clone();
        motion.connect_enter(move |_, _, _| {
            on_enter.inside.set(true);
            // Invalidate any close that is already scheduled.
            on_enter.token.set(on_enter.token.get() + 1);
            if is_menu {
                on_enter_menu.hover_token.set(on_enter_menu.hover_token.get() + 1);
            }
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
            // How many times a close may be put off because the menu is still moving.
            //
            // Bounded, because the alternative is a close that never happens: `morphing`
            // is set when a motion is kicked and cleared when a tick finishes it, and a
            // kick with no tick behind it leaves it true for good. The timer then
            // rescheduled itself forever and the menu could not be dismissed by moving
            // the pointer away from it at all — it could only be dismissed by clicking.
            let mut deferred = 0u8;
            glib::timeout_add_local(HOVER_GRACE, move || {
                // A surface being resized under a stationary pointer is handed a leave,
                // and the next resize will hand it another. So while the menu is still
                // moving, a close is put off rather than acted on: the pointer has not
                // gone anywhere, the surface has. A few times only.
                if closing.morphing.get() && deferred < HOVER_DEFER_LIMIT {
                    deferred += 1;
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

/// The bar's panel colour, at [`BAR_PANEL_ALPHA`].
///
/// Named once because three rules in the stylesheet have to agree on it — the bar, the
/// app menu, and a test that would otherwise be a fourth copy of a colour literal.
const BAR_PANEL: (u8, u8, u8) = (32, 32, 32);
const BAR_PANEL_ALPHA: f64 = 0.82;

fn style_sheet((r, g, b): (u8, u8, u8)) -> String {
    // Fed in rather than written out, so the bar and the app menu cannot drift apart
    // into two different greys.
    let (panel_r, panel_g, panel_b) = BAR_PANEL;
    let panel_a = BAR_PANEL_ALPHA;
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
    background-color: rgba({panel_r}, {panel_g}, {panel_b}, {panel_a});
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
/* A square being carried. It has left the row and is in a layer over it, so it needs no
 * transition to stay under the pointer and nothing to lift it: the layer is above.
 *
 * No margin transitions and no z-index here. Both were how the row was rearranged
 * before, and both are wrong: a negative margin asks a widget with a 36px minimum for a
 * negative width, which GTK refuses, and GTK's CSS has no z-index at all — the property
 * it warned about on startup. */
button.task.carrying {{
    background-color: rgba(255, 255, 255, 0.18);
}}
/* Deliberately not dimmed. Minimized and closed are different things, and dimming
   the square conflates them with a pinned app that has nothing open at all. */
button.task.minimized {{
    background-color: rgba(255, 255, 255, 0.04);
}}
button.task.focused {{
    background-color: rgba({r}, {g}, {b}, 0.28);
}}
/* The app menu, opened by a right click. Widgets, so it is styled here.
   Its fill is the bar's own panel colour, not the previews' 0.35 black: a preview
   cell is 0.35 black *around an opaque window capture*, so only its padding shows,
   while every part of this is background. A square has no background of its own, so
   what a square is made of is the bar — and that is what this is. */
window.context {{
    background-color: rgba({panel_r}, {panel_g}, {panel_b}, {panel_a});
    /* The theme paints a window background image on top of a window's background
       colour, which would put a fill of its own over this and make it a third value
       again. */
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
    /// The connection is gone. The panel starts over and reconnects.
    Disconnected,
}

/// Reads the compositor's byte stream into events.
///
/// All the framing lives in [`crate::panel_proto`], which the compositor's half shares;
/// this is only the part that knows what the messages *mean*. The one thing worth saying
/// here is that a dropped connection is an event rather than an error: the compositor
/// restarts underneath a running panel, and a panel that treats that as fatal is a panel
/// that has to be killed and restarted by hand to get its bar back.
struct PanelReader {
    frames: panel_proto::FrameReader,
    /// Set when the socket has gone, cleared by [`Self::drain`].
    ///
    /// An event rather than a return value: a disconnection and a stream we cannot parse
    /// want the same response from the panel, and a panel that treats one as fatal is a
    /// panel that needs killing by hand whenever the compositor restarts.
    lost: bool,
}

impl Default for PanelReader {
    fn default() -> Self {
        Self {
            frames: panel_proto::FrameReader::new(),
            lost: false,
        }
    }
}

impl PanelReader {
    /// Take delivery of bytes read from the socket.
    fn feed(&mut self, bytes: &[u8]) {
        self.frames.feed(bytes);
    }

    /// Note that the compositor has gone, so [`Self::drain`] reports it once.
    ///
    /// Separate from the framer's own error path because the socket reading nothing —
    /// `read` returning zero — is the ordinary way a peer closes, and the framer cannot
    /// see it: it is a fact about the file descriptor, not about the bytes.
    fn report_lost(&mut self) {
        self.frames.reset();
        self.lost = true;
    }

    /// Every event the buffer now holds.
    ///
    /// Reading stops at the first frame that cannot be understood, because a stream we
    /// have lost our place in has no next message: the rest is reported as one
    /// disconnection rather than guessed at.
    fn drain(&mut self) -> Vec<PanelEvent> {
        let mut events = Vec::new();
        if self.lost {
            self.lost = false;
            events.push(PanelEvent::Disconnected);
        }
        loop {
            let Some(frame) = self.frames.next_frame() else {
                return events;
            };
            match frame {
                Err(err) => {
                    // Includes the peer hanging up, which is the ordinary case here: the
                    // compositor exited, or is being restarted.
                    tracing::debug!(%err, "panel: connection lost, will reconnect");
                    self.frames.reset();
                    events.push(PanelEvent::Disconnected);
                    return events;
                }
                Ok(frame) => match parse_frame(&frame) {
                    Some(event) => events.push(event),
                    // Framed correctly but not a message we know. The stream is still in
                    // step, because the frame length is what put it there.
                    None => {}
                },
            }
        }
    }
}

/// Turn one framed message into an event, or `None` if it is not one we know.
fn parse_frame(frame: &[u8]) -> Option<PanelEvent> {
    if frame.starts_with(b"img\t") {
        let image = panel_proto::decode_image(frame)?;
        return Some(PanelEvent::Image {
            id: image.id,
            width: image.width,
            height: image.height,
            pixels: image.pixels,
        });
    }
    let fields = panel_proto::parse_text(frame)?;
    if fields.first().map(String::as_str) != Some("list") {
        return None;
    }
    parse_snapshot(&fields).map(PanelEvent::Snapshot)
}

/// Write a message to the compositor.
///
/// A partial write is not treated as an error worth reporting: the socket is
/// non-blocking, and these are short requests the compositor drains every frame. Silently
/// dropping one costs a preview that is asked for again on the next tick.
fn send(stream: &Rc<UnixStream>, message: &str) {
    let _ = (&**stream).write_all(&panel_proto::encode_text(message));
}

/// The panel's handle on the compositor.
///
/// A shared, replaceable pointer rather than an `Rc<UnixStream>` threaded through every
/// call, because the compositor can go away and come back: the bar, the app menu and the
/// context menu all send through the same socket, and on a reconnect all of them have to
/// be talking to the new one. Holding the socket in each of them would mean reconnecting
/// each of them, and any one of them that was missed would keep writing into a dead
/// connection forever.
#[derive(Clone, Default)]
struct Channel(Rc<RefCell<Option<Rc<UnixStream>>>>);

impl Channel {
    fn new(stream: Option<Rc<UnixStream>>) -> Self {
        Self(Rc::new(RefCell::new(stream)))
    }

    fn get(&self) -> Option<Rc<UnixStream>> {
        self.0.borrow().clone()
    }

    /// Point every holder at a new socket, or at nothing.
    fn set(&self, stream: Option<Rc<UnixStream>>) {
        *self.0.borrow_mut() = stream;
    }

    fn is_connected(&self) -> bool {
        self.0.borrow().is_some()
    }

    /// Send one message, if there is anywhere to send it.
    fn send(&self, message: &str) {
        if let Some(stream) = self.get() {
            send(&stream, message);
        }
    }
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

fn build_ui(app: &Application, socket: Option<Rc<UnixStream>>) {
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
    // One handle, shared by the bar, both menus and the poll timer, so that a
    // reconnect is a single assignment rather than something each holder has to be told
    // about — and cannot be missed by.
    let channel = Channel::new(socket.clone());
    let (bar, bar_row, tasks) = build_bar_window(app, &monitor);
    // The same handle, not a copy of the socket: a reconnect is then a single assignment
    // and there is no second place where a menu can still be holding the connection that
    // just died. The context menu used to be given its own, and nothing ever set it, so
    // right-clicking a square opened a menu with nothing in it.
    let menu = build_menu(app, &monitor, channel.clone());
    // The app menu, opened by a right click. A third surface for the same reason
    // the menu is a second: an unpainted region of a surface still takes clicks.
    let context = build_context_menu(app, &monitor, channel.clone());
    // Watching needs the bar, so it is wired here rather than at the build.
    watch_context_hover(&context, &bar_row);
    let hover = Rc::new(Hover::default());


    // Hover-out closes the menu. The pointer has to cross the bar to reach the
    // menu, so "inside" spans both surfaces, and the close is deferred briefly so
    // passing between them is not read as leaving.
    watch_hover(&bar_row, &menu, &hover);

    if let Some(stream) = socket {
        channel.send(&format!("accent\t{:.6}\t{:.6}\t{:.6}", accent_rgb().0, accent_rgb().1, accent_rgb().2));
        drop(stream);
    }
    // Polled whether or not there was a socket to begin with: a panel started before its
    // compositor is listening yet is the ordinary case on a login, and the reconnect in
    // the poll timer is what picks it up.
    start_polling(&tasks, &menu, &channel, hover, &context);

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

    // The row, with a layer over it for the square a drag is carrying.
    //
    // A carried square cannot stay in the box. GTK lays a box out from its children's
    // size requests, and a square with a negative margin asks for a negative width, which
    // a widget with a 36px minimum cannot have — GTK rejects it and asserts. Moving one
    // square out of the flow and leaving a placeholder of the same size behind is the only
    // way to carry a square *and* have the row close up around the hole, without asking
    // the layout for something impossible.
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&row));
    // The layer a carried square goes into is built when a drag starts and destroyed when
    // it ends, so that at rest there is nothing at all between the pointer and the
    // squares.
    //
    // It used to live here for the whole session. It is a real widget, in an overlay, in
    // front of every square — and a widget in front of a button takes the pointer before
    // the button sees it. That is why nothing in the bar could be pressed: not a broken
    // drag, a layer that was always there, doing nothing, eating every click.
    OVERLAY.with(|cell| *cell.borrow_mut() = Some(overlay.clone()));

    window.set_child(Some(&overlay));
    (window, row, tasks)
}

/// Tell the compositor the accent so it can tint the snap preview.
///
/// Sent on every connect rather than once at startup, because the compositor forgets it
/// when it exits: a panel that outlives its compositor has to say it again or the snap
/// preview comes back in whatever colour the last session ended on.
fn send_accent(channel: &Channel) {
    let (red, green, blue) = accent_rgb();
    channel.send(&format!("accent\t{red:.6}\t{green:.6}\t{blue:.6}"));
}

/// Drain the compositor socket, rebuilding the task list on every snapshot and
/// keeping the newest preview for each window.
fn start_polling(
    tasks: &GtkBox,
    menu: &Rc<Menu>,
    channel: &Channel,
    _hover: Rc<Hover>,
    context: &Rc<ContextMenu>,
) {
    let menu = menu.clone();
    let tasks = tasks.clone();
    let context = context.clone();
    let reader = RefCell::new(PanelReader::default());
    let windows = Rc::new(RefCell::new(Vec::<WindowInfo>::new()));
    // Both timers below need this state, so hand each its own handle.
    let channel_poll = channel.clone();
    let channel_refresh = channel.clone();
    let windows_refresh = windows.clone();
    let menu_tick = menu.clone();

    glib::timeout_add_local(POLL_INTERVAL, move || {
        let mut buf = [0u8; 65536];
        let mut lost = false;
        // Through the channel rather than a captured socket, because the socket is
        // replaceable: a reconnect has to be visible here without this closure being
        // rebuilt.
        let stream = channel_poll.get();
        match stream.as_ref() {
            Some(stream) => loop {
                match (&**stream).read(&mut buf) {
                    // The compositor has gone. Not fatal — the panel outlives it, and
                    // this used to be a plain `break`, which left the panel polling a
                    // dead file descriptor every 8ms for the rest of the session with an
                    // empty task list and no way back.
                    Ok(0) => {
                        lost = true;
                        break;
                    }
                    Ok(n) => reader.borrow_mut().feed(&buf[..n]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        lost = true;
                        break;
                    }
                }
            },
            // Nothing connected: not an error, just nothing to read. This is also the
            // state between a disconnect and the next attempt.
            None => {}
        }
        // The reader is told about the loss rather than being thrown away, so everything
        // downstream sees one code path for "the window list is no longer trustworthy"
        // however it happened.
        if lost {
            reader.borrow_mut().report_lost();
        }

        for event in reader.borrow_mut().drain() {
            match event {
                PanelEvent::Snapshot(list) => {
                    windows.replace(list.clone());
                    rebuild_tasks(&tasks, &menu, &list, &channel_poll, &context);
                }
                PanelEvent::Image {
                    id,
                    width,
                    height,
                    pixels,
                } => {
                    // Straight to the menu, which keeps the preview. There used to be a
                    // cache here that the value was read back out of — but `insert`
                    // returns the value it *replaced*, so the first preview of a window
                    // never arrived and every later one was a frame stale, which is
                    // exactly what a preview that never seems to change looks like.
                    menu_set_image(&menu, id, width, height, pixels);
                }
                PanelEvent::Disconnected => {
                    // Forget the compositor's window list rather than leaving a stale one
                    // on screen: every square in it belongs to a compositor that is gone.
                    // The compositor sends a fresh list on connect, so nothing has to be
                    // asked for by hand.
                    windows.replace(Vec::new());
                    rebuild_tasks(&tasks, &menu, &[], &channel_poll, &context);
                    reconnect(&channel_poll);
                }
            }
        }

        glib::ControlFlow::Continue
    });

    // Keep an open menu's previews current, so a window that is animating or playing
    // video is not shown frozen.
    glib::timeout_add_local(PREVIEW_REFRESH, move || {
        let snapshot = windows_refresh.borrow().clone();
        menu_refresh(&menu_tick, &snapshot, &channel_refresh);
        glib::ControlFlow::Continue
    });
}

/// Point the panel at a new compositor connection, if there is one to have.
///
/// Runs from the poll timer after a disconnect, so a compositor that has been restarted
/// underneath a live panel is picked up without anybody having to kill the panel. The
/// compositor sends its whole window list the moment it accepts a connection, so there is
/// nothing to re-request here beyond the accent, which it does not remember between
/// sessions.
fn reconnect(channel: &Channel) {
    match connect_panel() {
        Some(stream) => {
            channel.set(Some(stream));
            // Both menus keep their own copy of the channel, and are pointed at the new
            // connection too — otherwise a menu opened after the reconnect would send
            // into the socket that just died.
            send_accent(channel);
            tracing::info!("Reconnected to the compositor");
        }
        None => {
            // Not an error: the compositor may not be listening yet. Try again shortly —
            // this has to be a timer rather than waiting for the next disconnection,
            // because nothing further will happen to wake us. A panel started before its
            // compositor came up, and a panel whose compositor is restarting, both land
            // here, and neither produces another `Disconnected` to try again from.
            channel.set(None);
            let channel = channel.clone();
            glib::timeout_add_local(RECONNECT_INTERVAL, move || {
                if channel.is_connected() {
                    return glib::ControlFlow::Break;
                }
                reconnect(&channel);
                if channel.is_connected() {
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
        }
    }
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
    stream: &Channel,
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
    // Before anything is taken out of the row: a carried square is not in it, and would
    // otherwise be left over the bar taking clicks that are not its own.
    drag_cancel();
    while let Some(child) = tasks.first_child() {
        tasks.remove(&child);
    }
    if menu_debug() {
        eprintln!(
            "oxide-panel: rebuilt bar with {} window(s) for {:?}",
            windows.len(),
            menu.app.borrow().as_deref().unwrap_or("<none>")
        );
    }

    // Group the windows by app. Windows with no app id get a key of their own so
    // they don't all collapse together.
    let mut groups: Vec<(String, Vec<&WindowInfo>)> = Vec::new();
    for info in windows {
        // The same rule [`group_for`] uses, so a rebuild and a resync cannot disagree
        // about which windows belong to an app.
        let key = group_key(info);
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
        // A rebuild is itself the reconciliation, so anything the row missed while a
        // switch was running has now been applied.
        menu.resync_pending.set(false);
        let group: Vec<&WindowInfo> = groups
            .iter()
            .find(|(group_key, _)| group_key == key)
            .map(|(_, group)| group.clone())
            .unwrap_or_default();
        // Every window, minimized ones included. A minimized window cannot be
        // captured, so the compositor answers with the last frame it took for it and
        // the cell shows that: more use than a hole in the row, and it keeps the count
        // on the square and the number of previews in step.
        menu_replace(menu, &every, &group, stream);
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
    stream: &Channel,
    key: &str,
    windows: &[WindowInfo],
    menu: &Rc<Menu>,
    button: &Button,
    multiple: bool,
) {
    if menu_debug() {
        eprintln!(
            "oxide-panel: activate {key:?} multiple={multiple} windows={} connected={}",
            windows.len(),
            stream.is_connected()
        );
    }
    if !multiple {
        match windows.first() {
            None => stream.send(&format!("launch\t{key}")),
            Some(window) => stream.send(&format!("focus\t{}", window.id)),
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

/// The class on the square being carried.
const DRAG_CARRYING: &str = "carrying";

/// The widget name a placeholder carries, so it is never mistaken for a square.
const GAP_NAME: &str = "gap:";

/// The prefix a square's widget name carries, so a drag can tell which app a button is.
///
/// Widget names are the only per-widget field a plain `Button` keeps that survives a
/// rebuild, and the row has to be able to map a position back to an app to move the
/// right one.
const APP_NAME_PREFIX: &str = "app:";

/// The app a square belongs to, from its widget name. A placeholder has none.
fn app_of(widget: &gtk4::Widget) -> Option<String> {
    let name = widget.widget_name();
    if name.starts_with(GAP_NAME) {
        return None;
    }
    Some(name.strip_prefix(APP_NAME_PREFIX)?.to_string())
}

/// Name a square, so a drag can find it again.
fn set_app_of(button: &Button, app: &str) {
    button.set_widget_name(&format!("{APP_NAME_PREFIX}{app}"));
}

/// Which gap in a row a pointer at this x is over.
///
/// The bar is a row, so the gap is decided by which squares' middles the pointer is past:
/// left of the first square's middle is before it, right of the last is after it, and in
/// between it is whichever side of that square's middle the pointer is on.
///
/// Separated from the widget walking so the arithmetic can be tested without a display,
/// which is the part that can be off by one and put a square down one place out.
fn gap_at(middles: &[f64], x: f64) -> usize {
    middles.iter().filter(|middle| x > **middle).count()
}

/// Where each square in the row is, in the row's own coordinates.
struct RowGeometry {
    /// The middles of the visible children, left to right. A placeholder is one of them,
    /// because the hole it leaves is exactly as wide as the square that left it.
    middles: Vec<f64>,
}

impl RowGeometry {
    fn gap_at(&self, x: f64) -> usize {
        gap_at(&self.middles, x)
    }
}

/// Measure the row of squares.
fn row_geometry(tasks: &GtkBox) -> RowGeometry {
    let mut middles = Vec::new();
    let mut child = tasks.first_child();
    while let Some(widget) = child {
        let square = widget.upcast::<gtk4::Widget>();
        // A square that has not been allocated yet has no position, and contributes
        // nothing to the row either.
        if let Some((left, _)) = square.translate_coordinates(tasks, 0.0, 0.0) {
            middles.push(left + f64::from(square.width()) / 2.0);
        }
        child = square.next_sibling();
    }
    RowGeometry { middles }
}

/// The square being carried, and the placeholder standing in for it.
struct DragState {
    /// The app being carried.
    app: String,
    /// The gap it would land in if let go now.
    gap: usize,
    /// Where it was picked up from, so putting it back where it started is not a change.
    original: usize,
    /// Where its top left corner is in the carried layer, which is what the pointer's
    /// travel is added to.
    carried_at: (f64, f64),
}

thread_local! {
    // One square at a time, and every square's handlers must be able to see it: the
    // gesture that starts the drag belongs to the pressed button, but the row has to be
    // rearranged by squares that know nothing about it.
    static DRAG: RefCell<Option<Rc<RefCell<DragState>>>> = const { RefCell::new(None) };
}

/// Whether a square is being carried.
fn drag_active() -> bool {
    DRAG.with(|drag| drag.borrow().is_some())
}

/// Pick a square up.
///
/// The square leaves the row and goes into the overlay, and a placeholder of the same
/// size takes its place, so the row keeps its width and closes up around the hole exactly
/// as it would if the square were still there and had moved. Nothing in the row is asked
/// for a negative size.
fn drag_begin(app: &str) {
    if drag_active() {
        // A second square cannot join the first. Ignoring it is better than ending the
        // drag in progress, which would leave the carried square stranded in the overlay.
        return;
    }
    if !crate::desktop::is_app_id(app) {
        // A window that never said what it was has no place in the order; there is
        // nothing to remember about it between runs.
        return;
    }
    let Some(tasks) = tasks_box() else {
        return;
    };
    let Some(square) = square_for(&tasks, app) else {
        return;
    };
    let Some(overlay) = carried_layer() else {
        return;
    };
    let index = child_index(&tasks, square.upcast_ref()).unwrap_or(0);

    // Where the square is *before* anything moves, so the layer can put it back exactly
    // there and the pointer can be measured against it. In the row's own coordinates,
    // which is also the layer's, because the layer sits exactly over the row.
    //
    // Taken first deliberately: inserting the placeholder at `index` pushes the square
    // one place along, so measuring afterwards lands a whole square out — every drag
    // began from the wrong position.
    let (left, top) = square
        .translate_coordinates(&tasks, 0.0, 0.0)
        .unwrap_or((0.0, 0.0));

    // The placeholder. Invisible, the same size as a square, and holding the gap open.
    let placeholder = GtkBox::new(Orientation::Horizontal, 0);
    placeholder.set_widget_name(GAP_NAME);
    placeholder.set_size_request(SQUARE_SIZE, SQUARE_SIZE);
    put_child(&tasks, placeholder.upcast_ref(), index);

    tasks.remove(&square);
    square.add_css_class(DRAG_CARRYING);
    overlay.put(&square, left, top);

    if menu_debug() {
        eprintln!(
            "oxide-panel: carrying {app:?} at {index}, left {left:.0}, {} squares",
            row_geometry(&tasks).middles.len()
        );
    }
    DRAG.with(|drag| {
        *drag.borrow_mut() = Some(Rc::new(RefCell::new(DragState {
            app: app.to_string(),
            gap: index,
            original: index,
            carried_at: (left, top),
        })));
    });
    if menu_debug() {
        eprintln!("oxide-panel: picked up {app:?}");
    }
}

/// Carry the square under the pointer, and move the hole in the row to suit.
///
/// Returns whether the gap moved, which is the only time the row has to be laid out
/// again.
fn drag_update(pointer_x: f64, delta: (f64, f64)) -> bool {
    let state = DRAG.with(|drag| drag.borrow().clone());
    let Some(state) = state else {
        return false;
    };
    let Some(tasks) = tasks_box() else {
        return false;
    };
    let Some(overlay) = carried_layer() else {
        return false;
    };
    let _ = &overlay;
    let app = state.borrow().app.clone();
    let Some(square) = carried_square(&app) else {
        return false;
    };

    // From where the square was put down, not from where it is now: adding the total
    // travel onto the current position compounds the rounding of every step, and the
    // square drifts away from the cursor over a long drag.
    let origin = state.borrow().carried_at;
    let (dx, dy) = delta;
    // From the square's position at the start of the drag, not from where it is now:
    // accumulating a delta onto the current position compounds the rounding of every
    // step, and the square drifts away from the cursor over a long drag.
    let (x, y) = (
        (origin.0 + dx - f64::from(SQUARE_SIZE) / 2.0).round(),
        (origin.1 + dy - f64::from(SQUARE_SIZE) / 2.0).round(),
    );
    // `move`, not `put`. `gtk_fixed_put` asserts that the widget has no parent, and this
    // square has been in the fixed since the drag began: every reposition after the first
    // failed the assertion and did nothing. The square froze where it was picked up while
    // the hole in the row kept following the pointer — and, worse, the squares left
    // stranded in the fixed stayed over the bar taking clicks that were not theirs, which
    // is why nothing in the bar could be pressed at all.
    overlay.move_(&square, x, y);
    // The gap is measured against the row as it is *now*, with the placeholder in it, so
    // the hole moves and the gap follows it.
    let gap = row_geometry(&tasks).gap_at(pointer_x);
    if gap == state.borrow().gap {
        return false;
    }
    if let Some(placeholder) = placeholder_for(&tasks) {
        put_child(&tasks, placeholder.upcast_ref(), gap);
    }
    state.borrow_mut().gap = gap;
    if menu_debug() {
        eprintln!("oxide-panel: gap {gap} for {app:?} (pointer {pointer_x:.0})");
    }
    true
}

/// Abandon a drag without moving anything.
///
/// Called before the row is rebuilt. A carried square lives outside the row, so a rebuild
/// that empties the row would leave it stranded in the layer above it — still holding
/// pointer input, over a bar position that no longer means anything. Every button in the
/// row is about to be discarded anyway, so the square is simply let go of.
fn drag_cancel() {
    let running = DRAG.with(|drag| drag.borrow_mut().take());
    if running.is_none() && carried_square_count() == 0 {
        return;
    }
    // Take the whole layer away rather than emptying it. Emptying leaves the widget over
    // the bar taking clicks aimed at the squares; this is what puts the bar back to
    // having nothing between it and the pointer.
    drop_carried_layer();
    // And take the placeholder out of the row, or it is a hole in the bar that nothing
    // will ever fill.
    if let Some(tasks) = tasks_box()
        && let Some(placeholder) = placeholder_for(&tasks)
    {
        tasks.remove(&placeholder);
    }
    if menu_debug() && running.is_some() {
        eprintln!("oxide-panel: drag abandoned by a rebuild");
    }
}

/// How many widgets the carried layer is holding, which should never be more than one.
fn carried_square_count() -> usize {
    let Some(layer) = existing_carried_layer() else {
        return 0;
    };
    let mut count = 0usize;
    let mut cursor = layer.first_child();
    while let Some(widget) = cursor {
        count += 1;
        cursor = widget.next_sibling();
    }
    count
}

/// Put the square down where it was dropped, and remember it.
///
/// Returns whether it actually moved, which is what tells a press that picked a square up
/// and put it straight back from one that rearranged the bar.
fn drag_end() -> bool {
    let state = DRAG.with(|drag| drag.borrow_mut().take());
    let (Some(state), Some(tasks), Some(overlay)) = (state, tasks_box(), carried_layer()) else {
        return false;
    };
    let app = state.borrow().app.clone();
    let gap = state.borrow().gap;

    // The square goes back in the row, in the placeholder's place, and the placeholder
    // goes away. Whatever the row was showing before the drag began is restored by the
    // rebuild below, so this only has to be *correct*, not pretty.
    let square = carried_square(&app);
    if square.is_none() {
        // The square has gone missing from the layer, which means something went wrong
        // mid-drag. Put everything back rather than leaving a half-finished one.
        drag_cancel();
        return false;
    }
    let landed = placeholder_for(&tasks);
    let index = landed
        .as_ref()
        .and_then(|placeholder| child_index(&tasks, placeholder))
        .unwrap_or(gap);
    if let Some(placeholder) = landed {
        tasks.remove(&placeholder);
    }
    let stayed = index == state.borrow().original;
    if let Some(square) = square {
        square.remove_css_class(DRAG_CARRYING);
        overlay.remove(&square);
        put_child(&tasks, square.upcast_ref(), index);
    }
    // The square is back in the row, so the layer has nothing left to be for.
    drop_carried_layer();
    if stayed {
        if menu_debug() {
            eprintln!("oxide-panel: {app:?} put down where it started");
        }
        return false;
    }
    if menu_debug() {
        eprintln!("oxide-panel: {app:?} moved to {index}");
    }
    // The same path a drop took, so there is one way the bar is reordered and saved.
    place_at(&app, index);
    true
}

/// The box the squares are in, while there is a bar.
fn tasks_box() -> Option<GtkBox> {
    LAST_BAR.with(|cell| cell.borrow().as_ref().map(|state| state.0.clone()))
}

/// The layer a carried square is put in, built now if there is not one already.
///
/// Built on demand and torn down afterwards rather than kept: a widget left sitting over
/// the row is a widget sitting between the pointer and the squares, and it takes the
/// click whether or not anything is in it.
fn carried_layer() -> Option<gtk4::Fixed> {
    if let Some(existing) = existing_carried_layer() {
        return Some(existing);
    }
    let overlay = OVERLAY.with(|cell| cell.borrow().clone())?;
    let layer = gtk4::Fixed::new();
    layer.set_halign(gtk4::Align::Start);
    layer.set_valign(gtk4::Align::Start);
    overlay.add_overlay(&layer);
    Some(layer)
}

/// The carried layer, if one is already there.
///
/// Never builds one. A function that only wants to *look* at the layer must not be able to
/// bring it into existence, because a layer over the bar is a layer over the squares and
/// nothing about it should ever be created by a read.
fn existing_carried_layer() -> Option<gtk4::Fixed> {
    let overlay = OVERLAY.with(|cell| cell.borrow().clone())?;
    let mut cursor = overlay.last_child();
    while let Some(widget) = cursor {
        if let Some(layer) = widget.downcast_ref::<gtk4::Fixed>() {
            return Some(layer.clone());
        }
        cursor = widget.prev_sibling();
    }
    None
}

/// Take the carried layer out of the overlay, so nothing is over the bar.
fn drop_carried_layer() {
    let overlay = OVERLAY.with(|cell| cell.borrow().clone());
    let Some(overlay) = overlay else { return };
    let mut cursor = overlay.last_child();
    while let Some(widget) = cursor {
        let next = widget.prev_sibling();
        if widget.downcast_ref::<gtk4::Fixed>().is_some() {
            widget.unparent();
        }
        cursor = next;
    }
}

/// One square, by app.
fn square_for(tasks: &GtkBox, app: &str) -> Option<Button> {
    let mut child = tasks.first_child();
    while let Some(widget) = child {
        let square = widget.upcast::<gtk4::Widget>();
        if let Some(button) = square.downcast_ref::<Button>()
            && app_of(&square).as_deref() == Some(app)
        {
            return Some(button.clone());
        }
        child = square.next_sibling();
    }
    None
}

/// The square currently in the carried layer, if a drag is running.
fn carried_square(app: &str) -> Option<Button> {
    let mut cursor = existing_carried_layer()?.first_child();
    while let Some(widget) = cursor {
        if let Some(button) = widget.downcast_ref::<Button>()
            && app_of(&square_ref(button)).as_deref() == Some(app)
        {
            return Some(button.clone());
        }
        cursor = widget.next_sibling();
    }
    None
}

/// A button as the plain widget its name is read from.
fn square_ref(button: &Button) -> gtk4::Widget {
    button.clone().upcast()
}

/// The placeholder standing in for the carried square.
fn placeholder_for(tasks: &GtkBox) -> Option<gtk4::Widget> {
    let mut child = tasks.first_child();
    while let Some(widget) = child {
        let square = widget.upcast::<gtk4::Widget>();
        if square.widget_name() == GAP_NAME {
            return Some(square.clone());
        }
        child = square.next_sibling();
    }
    None
}

/// Whether two widgets are the same object, without borrowing one to compare it.
///
/// A box can only be walked one child at a time and the child being looked for is held
/// elsewhere, so the pointers are compared rather than the values.
fn same_widget(a: &gtk4::Widget, b: &gtk4::Widget) -> bool {
    std::ptr::eq(a.as_ptr(), b.as_ptr())
}

/// Put a child at an index in a box, moving it if it is already in one.
///
/// `GtkBox` has no `insert`, only `insert_child_after` — so the siblings are read to find
/// who should precede it, and a child going to the front is prepended. Taken at face
/// value, `insert_child_after` with a child still in the box moves nothing at all, which
/// is a quiet way to do nothing for a whole drag.
fn put_child(parent: &GtkBox, child: &gtk4::Widget, index: usize) {
    let child = child.clone();
    // Unparented first, always. `insert_child_after` with a child that is still in a box
    // moves nothing at all, which is a silent way to do nothing.
    if child.parent().is_some() {
        child.unparent();
    }
    if index == 0 {
        parent.prepend(&child);
        return;
    }
    let mut siblings: Vec<gtk4::Widget> = Vec::new();
    let mut cursor = parent.first_child();
    while let Some(widget) = cursor {
        siblings.push(widget.clone());
        cursor = widget.next_sibling();
    }
    match siblings.get(index - 1) {
        Some(before) => parent.insert_child_after(&child, Some(before)),
        // Past the end: the end is the only place left to put it.
        None => parent.append(&child),
    }
}

/// Where a child sits in a box.
fn child_index(parent: &GtkBox, child: &gtk4::Widget) -> Option<usize> {
    let mut index = 0usize;
    let mut cursor = parent.first_child();
    while let Some(widget) = cursor {
        if same_widget(&widget, child) {
            return Some(index);
        }
        index += 1;
        cursor = widget.next_sibling();
    }
    None
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

/// Show or hide one square's tooltip, to match whether a menu is up.
///
/// A tooltip says what a square is, and a menu under the pointer is about to say it
/// better. Left on, it came up underneath the app menu and drew through it: the menus are
/// 35% black over whatever is behind them, so anything behind shows.
fn sync_tooltip(button: &Button, menu: &Rc<Menu>, context: &Rc<ContextMenu>) {
    button.set_has_tooltip(!(menu.shown.get() || context.shown.get()));
}

/// Bring every square's tooltip into line with the menus, from whichever menu just moved.
///
/// Called whenever a menu opens or closes, so the answer does not depend on when the bar
/// happened to be rebuilt — which is what used to leave every square tooltip-less after
/// one menu had been opened, until some window's title changed.
fn sync_tooltips_from(menu: &Rc<Menu>) {
    let context = LAST_BAR.with(|cell| cell.borrow().as_ref().map(|state| state.4.clone()));
    if let Some(context) = context {
        sync_tooltips(menu, &context);
    }
}

fn sync_tooltips_from_context(context: &Rc<ContextMenu>) {
    let menu = LAST_BAR.with(|cell| cell.borrow().as_ref().map(|state| state.1.clone()));
    if let Some(menu) = menu {
        sync_tooltips(&menu, context);
    }
}

/// Bring every square's tooltip into line with the menus.
fn sync_tooltips(menu: &Rc<Menu>, context: &Rc<ContextMenu>) {
    let tasks = LAST_BAR.with(|cell| cell.borrow().as_ref().map(|state| state.0.clone()));
    let Some(tasks) = tasks else {
        return;
    };
    let mut child = tasks.first_child();
    while let Some(widget) = child {
        if let Some(button) = widget.downcast_ref::<Button>() {
            sync_tooltip(button, menu, context);
        }
        child = widget.next_sibling();
    }
}

/// One square (1:1) per app, with indicator dots for its window count.
///
/// With several windows open the square opens a menu of their previews
/// instead of focusing; a lone window is just focused.
fn app_button(
    app_id: &str,
    windows: &[&WindowInfo],
    stream: &Channel,
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
    // Named for the app, which is how a drag finds this square again: the row moves
    // squares by app, and a button carries nothing else that survives a rebuild.
    set_app_of(&button, key);
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
    // The text is always set; whether it is *shown* is not decided here. That used to be
    // decided here, from whether a menu was open at the moment this square was built —
    // and nothing ever turned it back on except another rebuild, which needs a window
    // title to change. So closing a preview menu left every square in the bar without a
    // tooltip until something unrelated happened, and the text computed above was thrown
    // away. See `sync_tooltips`, which both menus call when they open and close.
    button.set_tooltip_text(Some(&tooltip));
    sync_tooltip(&button, menu, context);

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

        // Owned handles for the handler, because a controller can outlive this
        // function. The button is the exception: it is held weakly, because this
        // controller is added *to* that button, and a strong capture would make a cycle
        // that no `unparent` breaks. Every rebuild of the bar would leave one behind.
        let on_enter = token.clone();
        let (enter_menu, enter_key) = (hover_menu.clone(), hover_key.clone());
        let (enter_group, enter_stream, enter_context) = (
            hover_group.clone(),
            hover_stream.clone(),
            hover_context.clone(),
        );
        hover.connect_enter(glib::clone!(
            #[weak]
            button,
            #[upgrade_or]
            return,
            move |_, _, _| {
                let (menu, key, group, stream, context) = (
                    &enter_menu,
                    &enter_key,
                    &enter_group,
                    &enter_stream,
                    &enter_context,
                );
                // Entering anywhere on the row cancels a close scheduled by leaving the
                // square before, whether this square has anything to show or not.
                menu.hover_token.set(menu.hover_token.get() + 1);
                if group.is_empty() {
                    // An app with no windows has no previews to offer, so there is
                    // nothing to switch to and nothing to show. It is not a reason to
                    // leave the previous app's menu on screen: leaving the previews up
                    // while the pointer rests somewhere they have nothing to do with is
                    // how a menu ends up floating over an icon it does not belong to.
                    if menu.shown.get() && menu.app.borrow().is_some() {
                        menu_close(menu);
                    }
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
                // The timer below outlives this handler by up to a quarter of a second,
                // and the bar can be rebuilt underneath it in the meantime: every window
                // title change throws away every square and builds new ones. So the timer
                // must not hold this square — a strong reference would keep the old one
                // alive for the session — but it must equally not give up when it goes,
                // or a rebuild in the middle of a hover cancels the hover and the previews
                // never open. It looks the square up again when it fires instead.
                let pending_key = key.clone();
                let menu = menu.clone();
                let key = key.clone();
                let group = group.clone();
                let stream = stream.clone();
                let token = on_enter.clone();
                let context = context.clone();
                glib::timeout_add_local(HOVER_OPEN, move || {
                    // The square this hover started on may have been rebuilt away. That
                    // is not a reason to abandon the hover: the pointer is still resting
                    // on the same app, which is still in the bar. Found again by name,
                    // because a rebuild replaces the widget and its position with it.
                    let Some(button) = tasks_box()
                        .as_ref()
                        .and_then(|tasks| square_for(tasks, &pending_key))
                    else {
                        // The app really has gone. Nothing to open.
                        return glib::ControlFlow::Break;
                    };
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
        ));

        let on_leave = token.clone();
        let on_leave_menu = menu.clone();
        let on_leave_key = key.to_string();
        hover.connect_leave(move |_| {
            on_leave.set(on_leave.get().wrapping_add(1));
            // Only while this app's own previews are up. Leaving a square whose menu is
            // not showing must not close somebody else's.
            if !on_leave_menu.shown.get()
                || on_leave_menu.app.borrow().as_deref() != Some(on_leave_key.as_str())
            {
                return;
            }
            let menu = on_leave_menu.clone();
            let ticket = menu.hover_token.get().wrapping_add(1);
            menu.hover_token.set(ticket);
            // Deferred, because the pointer may be on its way to the menu or to another
            // square, and either of those bumps the token and cancels this.
            let held_key = on_leave_key.clone();
            glib::timeout_add_local(HOVER_GRACE, move || {
                if menu.hover_token.get() != ticket {
                    return glib::ControlFlow::Break;
                }
                if menu.app.borrow().as_deref() == Some(held_key.as_str()) {
                    menu_close(&menu);
                }
                glib::ControlFlow::Break
            });
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
    let release_context = context.clone();
    // The release handler needs the same handles, and a closure that took them would
    // leave it holding moved values.
    let (rel_stream, rel_key, rel_menu) = (stream.clone(), key.clone(), menu.clone());
    let (rel_owned, rel_button) = (owned.clone(), button.clone());
    gesture.connect_pressed({
        let press_here = press_here.clone();
        let key = key.clone();
        let stream = stream.clone();
        let menu = menu.clone();
        let button = button.clone();
        let owned = owned.clone();
        let context = context.clone();
        move |_, _, _, _| {
            let ticket = press_here.get().wrapping_add(1);
            press_here.set(ticket);
            if menu_debug() {
                eprintln!("oxide-panel: pressed {key:?}");
            }
            // Activate shortly after the press, if it is still held.
            //
            // Not a convenience: this is the only thing that makes a click reliable. The
            // alternative is to wait for the release, and the release is delivered to the
            // square that was pressed — but `rebuild_tasks` throws away every square and
            // builds new ones on every snapshot from the compositor, so a button rebuilt
            // between the press and the release never delivers it. That is not a rare
            // edge: the compositor sends a snapshot whenever a window opens, closes or
            // takes focus, which is most of what happens while you are using a bar. The
            // log showed eighteen presses and not one release.
            //
            // A release still acts at once, and bumps the token, so a plain click is not
            // left waiting; and a drag bumps it too, so picking a square up never also
            // activates it.
            let armed_press = press_here.clone();
            let armed_stream = stream.clone();
            let armed_menu = menu.clone();
            let armed_button = button.clone();
            let armed_owned = owned.clone();
            let armed_key = key.clone();
            let armed_context = context.clone();
            glib::timeout_add_local(CLICK_ARM, move || {
                if armed_press.get() != ticket || drag_active() {
                    // Released already, or a drag took this press instead.
                    return glib::ControlFlow::Break;
                }
                armed_press.set(armed_press.get().wrapping_add(1));
                // The app menu dismisses itself rather than opening the previews behind
                // it, and one menu at a time.
                if armed_context.shown.get() {
                    context_close(&armed_context);
                    return glib::ControlFlow::Break;
                }
                activate(
                    &armed_stream,
                    &armed_key,
                    &armed_owned,
                    &armed_menu,
                    &armed_button,
                    multiple,
                );
                glib::ControlFlow::Break
            });

            // Holding without moving picks the square up, if the drag is on.
            let held_press = press_here.clone();
            let held_key = key.clone();
            if std::env::var_os("OXIDE_PANEL_DRAG").is_some() {
                glib::timeout_add_local(DRAG_HOLD, move || {
                    if held_press.get() != ticket || drag_active() {
                        // Released, already carrying a square, or the drag has begun.
                        return glib::ControlFlow::Break;
                    }
                    held_press.set(held_press.get().wrapping_add(1));
                    drag_begin(&held_key);
                    glib::ControlFlow::Break
                });
            }
        }
    });
    {
        let press = press.clone();
        gesture.connect_released(move |_, _, _, _| {
            let ticket = press.get().wrapping_add(1);
            press.set(ticket);
            if menu_debug() {
                eprintln!(
                    "oxide-panel: released {rel_key:?} drag_active={} context_shown={}",
                    drag_active(),
                    release_context.shown.get()
                );
            }
            // A press that became a drag ends here too, and the release of it must not
            // also count as a click. The square has been put back down by the drag's own
            // end handler by now, so there is nothing left to cancel — only a click that
            // would focus or start the app that was just moved across the bar.
            if drag_active() {
                drag_end();
                return;
            }
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
    // Picking this square up and putting it somewhere else in the bar.
    //
    // A `GestureDrag` of our own rather than GTK's drag-and-drop, because the gesture
    // wanted here is not drop-on-release: the row rearranges *while* the pointer moves,
    // which a drag source cannot express — it reports a beginning, an end and a drop, and
    // nothing in between. It is also why a drag could never be started at all before: a
    // `DragSource` and the click gesture above are both single-pointer gestures on the
    // same button, and the click gesture was added first, so it claimed the sequence and
    // the drag source was never offered one.
    //
    // Two ways in, meaning the same thing: move the pointer, or hold without moving. Both
    // are wanted. A drag that needs movement begins by activating the app when the hand
    // is not perfectly steady, and one that needs a hold cannot be started at all by
    // someone who lifts and puts the square down in one motion.
    // Off unless OXIDE_PANEL_DRAG is set, and *off by default*.
    //
    // The drag rearranges squares by taking one out of the row and putting it in a layer
    // over it. That is the only thing in the panel that moves a widget between parents
    // while the user is mid-gesture, and it is the prime suspect for the preview menu
    // coming up as an empty black box: a square left in the layer, or a placeholder left
    // in the row, is a hole where a cell should be.
    //
    // Rather than keep guessing at which of those it is, the feature is switchable, so
    // one run with it off says whether the drag is involved at all.
    let drag_enabled = std::env::var_os("OXIDE_PANEL_DRAG").is_some();
    let drag = gtk4::GestureDrag::new();
    drag.set_button(gtk4::gdk::BUTTON_PRIMARY);
    if drag_enabled {
    {
        let drag_press = press.clone();
        let drag_key = key.to_string();
        drag.connect_drag_begin(move |_, _, _| {
            // This press is a drag, not a click: cancelling the armed one here is what
            // stops rearranging the bar from also focusing or starting the app.
            drag_press.set(drag_press.get().wrapping_add(1));
            drag_begin(&drag_key);
        });
    }
    {
        let drag_press = press.clone();
        drag.connect_drag_update(move |_, delta_x, _| {
                if !drag_active() {
                return;
            }
                // Where the pointer is in the row, from where it went down: the square's
                // own left edge in the row, plus the centre of the square, plus the
                // distance travelled. The square is centred on the pointer rather than
                // hanging below it, which needs no record of where inside the square the
                // press landed.
                let start_left = DRAG.with(|drag| {
                    drag
                        .borrow()
                        .as_ref()
                        .map(|state| state.borrow().carried_at.0)
                });
                let Some(start_left) = start_left else { return };
                let pointer_x = start_left + f64::from(SQUARE_SIZE) / 2.0 + delta_x;
                if drag_update(pointer_x, (delta_x, 0.0)) {
                    if let Some(tasks) = tasks_box() {
                        tasks.queue_allocate();
                    }
                }
        });
        drag.connect_drag_end(move |_, _, _| {
            drag_press.set(drag_press.get().wrapping_add(1));
            let moved = drag_end();
            // Put down and rebuilt on an idle rather than here: this runs inside the
            // gesture, while the square is still being let go of.
            if moved {
                glib::idle_add_local_once(|| rebuild_bar());
            }
        });
    }
    button.add_controller(drag);
    }

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
            // A second right click on the same square closes it, as a second left
            // click on the previews does.
            if context.shown.get()
                && context.app.borrow().as_deref() == Some(right_key.as_str())
            {
                context_close(&context);
                return;
            }
            // The previews play their own close, and the app menu opens when that has
            // finished. Not overlapped and not cut: overlapping was two surfaces in one
            // place, and cutting threw away an animation the pointer had just earned.
            // One after the other, each doing its own thing.
            let opening = context.clone();
            let key = right_key.clone();
            let windows = right_windows.clone();
            let button = button.clone();
            if menu_open_now(&right_menu) {
                menu_close(&right_menu);
                glib::timeout_add_local(MENU_FADE, move || {
                    if opening.shown.get() {
                        // Something else opened the app menu while the previews were
                        // closing. Two of them at once is what this is avoiding.
                        return glib::ControlFlow::Break;
                    }
                    context_open(&opening, &key, &windows, &button);
                    glib::ControlFlow::Break
                });
                return;
            }
            context_open(&context, &right_key, &right_windows, &button);
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

/// Split a message into fields the way the framer does.
///
/// For tests, which want to hand [`parse_snapshot`] a message as it arrives rather than
/// building a frame around it first.
#[cfg(test)]
fn fields(message: &str) -> Vec<String> {
    panel_proto::parse_text(message.as_bytes()).unwrap_or_default()
}

/// Read the windows out of an already-split `list\t<count>\t<id>\t<focused>\t<minimized>
/// \t<app_id>\t<title>...` message.
///
/// Every field is propagated rather than defaulted, so a snapshot that has been truncated
/// or reordered is dropped whole instead of becoming a list of half-populated windows. The
/// declared count is checked against what arrived: the compositor is the only writer, so a
/// mismatch means the two ends disagree about the format, and quietly using the fields that
/// did arrive would hide that until something looked wrong on screen.
fn parse_snapshot(fields: &[String]) -> Option<Vec<WindowInfo>> {
    if fields.first().map(String::as_str) != Some("list") {
        return None;
    }
    let count: usize = fields.get(1)?.parse().ok()?;
    let rest = &fields[2..];
    // Five fields per window.
    if rest.len() != count * 5 {
        return None;
    }
    let mut windows = Vec::with_capacity(count);
    for window in rest.chunks_exact(5) {
        windows.push(WindowInfo {
            id: window[0].parse().ok()?,
            focused: window[1] == "1",
            minimized: window[2] == "1",
            app_id: window[3].clone(),
            title: window[4].clone(),
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
            parse_snapshot(&fields("list\t2\t7\t1\t0\tfirefox\tMozilla\t8\t0\t1\t\tTerminal")).unwrap();
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
        assert!(parse_snapshot(&fields("list\t1\t7\t1\t0\t\tfirefox\tMozilla")).is_none());
        // One field short, likewise.
        assert!(parse_snapshot(&fields("list\t1\t7\t1\t0\tfirefox")).is_none());
        // A count that does not parse.
        assert!(parse_snapshot(&fields("list\tx\t7\t1\t0\tfirefox\tMozilla")).is_none());
        // And the well-formed line still works, including an empty app id, which is
        // how a window with no app id is sent.
        let windows = parse_snapshot(&fields("list\t2\t7\t1\t0\tfirefox\tMozilla\t8\t0\t1\t\tTerm")).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[1].app_id, "");
    }

    #[test]
    fn ignores_other_messages() {
        assert!(parse_snapshot(&fields("focus\t3")).is_none());
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

    /// Feed bytes as though they had arrived in one read.
    fn deliver(reader: &mut PanelReader, bytes: &[u8]) {
        reader.feed(bytes);
    }

    #[test]
    fn reads_an_image_payload_after_its_header() {
        let mut reader = PanelReader::default();
        // Two pixels, and a 0x0A byte in the payload: the frame length is what keeps that
        // from truncating the message.
        let pixels = vec![1u8, 2, 3, 4, 5, 6, 7, 0x0A];
        deliver(&mut reader, &panel_proto::encode_image(7, 2, 1, &pixels).unwrap());

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
        let frame = panel_proto::encode_image(1, 2, 1, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        deliver(&mut reader, &frame[..frame.len() - 5]);
        assert!(reader.drain().is_empty(), "partial payload is not an event");
        deliver(&mut reader, &frame[frame.len() - 5..]);
        assert_eq!(reader.drain().len(), 1);
    }

    #[test]
    fn a_preview_whose_header_lies_about_its_size_is_dropped_not_misread() {
        // 2x1 is 8 bytes, not the 9 claimed. The frame is framed correctly, so the reader
        // stays in step and the *next* message still arrives.
        let mut reader = PanelReader::default();
        // Frame length 19: the header plus nine bytes of payload for a 2x1 image, which
        // is eight. The frame itself is framed correctly, which is the point.
        let mut wire = b"19\nimg\t9\t2\t1\nabcdefghi".to_vec();
        wire.extend_from_slice(&panel_proto::encode_text("list\t0"));
        deliver(&mut reader, &wire);
        let events = reader.drain();
        assert!(
            !matches!(events.first(), Some(PanelEvent::Image { .. })),
            "a header that lies about its size produced an image"
        );
        assert!(
            matches!(events.last(), Some(PanelEvent::Snapshot(list)) if list.is_empty()),
            "the stream lost its place after a rejected frame: {events:?}"
        );
    }

    #[test]
    fn a_window_list_arrives_as_a_window_list() {
        let mut reader = PanelReader::default();
        deliver(
            &mut reader,
            &panel_proto::encode_text("list\t2\t1\t1\t0\tfirefox\tA Tab\t2\t0\t1\tcode\tmain.rs"),
        );
        let events = reader.drain();
        match events.first() {
            Some(PanelEvent::Snapshot(list)) => {
                assert_eq!(list.len(), 2);
                assert_eq!(list[0].id, 1);
                assert!(list[0].focused);
                assert_eq!(list[0].app_id, "firefox");
                assert_eq!(list[0].title, "A Tab");
                assert!(!list[1].focused);
                assert_eq!(list[1].app_id, "code");
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    #[test]
    fn a_frame_that_is_not_a_message_is_skipped_and_the_next_one_still_arrives() {
        let mut reader = PanelReader::default();
        let mut wire = panel_proto::encode_text("something\tnew\tin\tthis\tversion");
        wire.extend_from_slice(&panel_proto::encode_text("list\t1\t3\t0\t0\tapp\tTitle"));
        deliver(&mut reader, &wire);
        let events = reader.drain();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(events[0], PanelEvent::Snapshot(_)));
    }

    #[test]
    fn a_stream_that_stops_being_readable_is_a_disconnection_too() {
        // A compositor that has spoken nonsense rather than hung up. There is no
        // position to resume from, so this is the same event and the same recovery.
        let mut reader = PanelReader::default();
        deliver(&mut reader, b"not a length\n");
        assert!(matches!(
            reader.drain().first(),
            Some(PanelEvent::Disconnected)
        ));
        deliver(&mut reader, &panel_proto::encode_text("list\t0"));
        assert!(matches!(
            reader.drain().first(),
            Some(PanelEvent::Snapshot(_))
        ));
    }

    #[test]
    fn a_lost_compositor_is_an_event_and_the_panel_starts_over() {
        // The panel used to treat a closed socket as "no more data", spin on a dead file
        // descriptor forever, and never recover: a compositor restart left a bar with an
        // empty task list and no way back.
        let mut reader = PanelReader::default();
        deliver(&mut reader, &panel_proto::encode_text("list\t0"));
        assert!(matches!(
            reader.drain().first(),
            Some(PanelEvent::Snapshot(_))
        ));
        // A stream that stops mid-frame is not a stream we can read to its end.
        let frame = panel_proto::encode_image(1, 2, 1, &[0u8; 8]).unwrap();
        deliver(&mut reader, &frame[..4]);
        assert!(reader.drain().is_empty(), "a partial frame is not an event");
        // The rest of that frame, then the socket going away.
        deliver(&mut reader, &frame[4..]);
        assert_eq!(reader.drain().len(), 1);
        reader.report_lost();
        assert!(matches!(
            reader.drain().first(),
            Some(PanelEvent::Disconnected)
        ));
        // And the reader is usable afterwards, for the panel that reconnects.
        deliver(&mut reader, &panel_proto::encode_text("list\t0"));
        assert!(matches!(
            reader.drain().first(),
            Some(PanelEvent::Snapshot(_))
        ));
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
    /// Run `body` with an empty layout and the config file redirected to a temporary one.
    ///
    /// The redirect is the important half. Anything that changes the layout saves it, and
    /// the save went to the user's real `panel.conf` — so the test suite used to overwrite
    /// the running panel's pins and bar order with these tests' fixtures, on every run,
    /// silently. Kept in one place so a new test cannot reintroduce it by forgetting.
    fn with_empty_layout(body: impl FnOnce()) {
        let path = std::env::temp_dir().join(format!(
            "oxide-panel-test-{}-{:?}.conf",
            std::process::id(),
            std::thread::current().id()
        ));
        let _guard = crate::panel_conf::use_test_path(path.clone());
        LAYOUT.with(|layout| *layout.borrow_mut() = Layout::default());
        body();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_test_suite_cannot_write_the_real_config() {
        // The bug this guards against: `cargo test` silently replacing the user's pins and
        // bar order with test fixtures. It survived because every test passed and the panel
        // merely came back with the wrong bar — nobody looks at a config file after a green
        // test run and concludes it ate their settings.
        with_empty_layout(|| {
            toggle_pin("an-app-that-does-not-exist");
            arrange(&["another-fake-app".to_string()]);
        });
        // Nothing was written to the temporary file either, because both of those record
        // their result in memory and only save on a change that reached disk — but the
        // point is which path was used, so check that directly.
        let path = std::env::temp_dir().join(format!(
            "oxide-panel-probe-{}.conf",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let _guard = crate::panel_conf::use_test_path(path.clone());
        toggle_pin("probe");
        let written = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            written.contains("pin probe"),
            "the override was not honoured: {written:?}"
        );
        let _ = std::fs::remove_file(&path);
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
    fn pinned_idle_apps_keep_the_same_order_every_run() {
        // This used to iterate the pin set, which is a hash set with a per-process
        // random seed, so two or more pinned-but-not-running apps came out in a
        // different order every time the panel started — and the next save wrote that
        // shuffle into panel.conf, so the bar rearranged itself across restarts.
        with_empty_layout(|| {
            LAYOUT.with(|layout| {
                let mut layout = layout.borrow_mut();
                for id in ["alpha", "beta", "gamma", "delta"] {
                    layout.pinned.insert(id.to_string());
                    layout.order.push(id.to_string());
                }
            });
            let groups: Vec<(String, Vec<&WindowInfo>)> = Vec::new();
            let first = pinned_without_windows(&groups);
            assert_eq!(first, ["alpha", "beta", "gamma", "delta"]);
            // Repeated calls, and a fresh layout with the same pins in the same order,
            // agree: nothing here depends on hash iteration.
            for _ in 0..16 {
                assert_eq!(pinned_without_windows(&groups), first);
            }
        });
    }

    #[test]
    fn a_pin_with_nowhere_to_go_still_has_a_stable_place() {
        // A hand-edited file, or an app pinned before it was ever seen: it is in the pin
        // set but not in the order. It gets a square, and that square does not move
        // between runs either.
        with_empty_layout(|| {
            LAYOUT.with(|layout| {
                let mut layout = layout.borrow_mut();
                for id in ["zebra", "yak", "xerus"] {
                    layout.pinned.insert(id.to_string());
                }
            });
            let groups: Vec<(String, Vec<&WindowInfo>)> = Vec::new();
            assert_eq!(
                pinned_without_windows(&groups),
                ["xerus", "yak", "zebra"]
            );
        });
    }

    #[test]
    fn a_window_with_no_app_id_does_not_get_remembered() {
        // The key for one of these names a panel id, and panel ids start again at one in
        // every compositor run. Persisting it would hand an unrelated window in a later
        // session the place this one had — and the file would grow one dead line per such
        // window ever seen.
        with_empty_layout(|| {
            let keys = vec!["browser".to_string(), "#17".to_string()];
            assert_eq!(arrange(&keys), [0, 1]);
            let order = LAYOUT.with(|layout| layout.borrow().order.clone());
            assert_eq!(order, ["browser"]);
            assert!(!order.iter().any(|entry| entry.starts_with('#')));
        });
    }

    /// A `measure` that is proportional to length, so bisection and shaving agree.
    fn wide_measure(text: &str) -> f64 {
        text.chars().count() as f64 * 10.0
    }

    #[test]
    fn a_title_is_cut_to_fit_however_long_it_is() {
        // Bisection has to land on the same answer shaving one character at a time did.
        // Ten a character, so the ellipsis counts against the budget like anything else.
        assert_eq!(truncate_to_width("firefox", 100.0, &wide_measure), "firefox");
        // Below 70 the whole title no longer fits, and the ellipsis spends budget too.
        assert_eq!(truncate_to_width("firefox", 65.0, &wide_measure), "firef\u{2026}");
        assert_eq!(truncate_to_width("firefox", 55.0, &wide_measure), "fire\u{2026}");
        assert_eq!(truncate_to_width("firefox", 45.0, &wide_measure), "fir\u{2026}");
        // Not even the ellipsis fits.
        assert_eq!(truncate_to_width("firefox", 5.0, &wide_measure), "\u{2026}");
        assert_eq!(truncate_to_width("firefox", 0.0, &wide_measure), "\u{2026}");
        // And the whole point: a very long title, measured logarithmically rather than
        // once per character. Counting the measures is the assertion.
        let long = "x".repeat(4096);
        let measures = std::cell::Cell::new(0usize);
        let counting = |text: &str| {
            measures.set(measures.get() + 1);
            wide_measure(text)
        };
        let cut = truncate_to_width(&long, 500.0, &counting);
        assert_eq!(cut.chars().count(), 50);
        // 12 bisection steps plus the initial check, rather than 4096 measures and a few
        // hundred megabytes of temporary strings.
        assert!(
            measures.get() <= 16,
            "{} measurements for a 4096 character title",
            measures.get()
        );
    }

    #[test]
    fn a_close_button_stays_inside_its_own_cell() {
        // Mid-morph a cell can be narrower than the titlebar. The square used to hang off
        // its left edge, over the previous cell — so the paint and the hit test disagreed
        // about what was under the pointer.
        let cell = Rect {
            x: 100,
            y: 6,
            width: 10,
            height: 142,
        };
        let close = close_rect(&cell);
        assert!(close.x >= cell.x, "the close square left its cell");
        assert_eq!(close.x + close.width, cell.x + cell.width);
        assert_eq!(close.width, 10);
        // And at full width it is unchanged.
        let wide = Rect { width: 362, ..cell };
        assert_eq!(close_rect(&wide).width, TITLEBAR_HEIGHT);
    }

    #[test]
    fn a_menu_wider_than_the_output_is_squeezed_onto_it_not_truncated() {
        // Six ordinary 16:9 windows come to more than a 1280px screen between them. The
        // layout is the sum of the row and has no idea how wide the display is.
        let wide: Vec<i32> = (0..6).map(|_| preview_width(1920, 1080) + 2).collect();
        let natural = layout_menu(&wide);
        assert!(natural.surface.width > 1280, "this case is meant to overflow");
        let mut squeezed = natural.clone();
        fit_menu_to_output(&mut squeezed, 1280);
        assert_eq!(squeezed.surface.width, 1280 - MENU_EDGE_GAP * 2);
        // Every cell is still on the surface and still has width, so every window is
        // still visible and still pressable. Clipping the surface instead would have
        // quietly lost the tail of the row.
        assert_eq!(squeezed.cells.len(), 6);
        for cell in &squeezed.cells {
            assert!(cell.width > 0, "a cell was squeezed out of existence");
            assert!(
                cell.x + cell.width <= squeezed.surface.width,
                "a cell runs off the right edge"
            );
        }
        // In order, and in proportion.
        for pair in squeezed.cells.windows(2) {
            assert!(pair[0].x < pair[1].x);
        }
        // A menu that already fits is left exactly as it was.
        let narrow = layout_menu(&wide[..2]);
        let mut untouched = narrow.clone();
        fit_menu_to_output(&mut untouched, 1280);
        assert_eq!(untouched, narrow);
        // A bar too narrow to hold even one preview is left alone rather than collapsed:
        // a menu with no width is not a menu.
        let mut too_narrow = natural.clone();
        fit_menu_to_output(&mut too_narrow, 40);
        assert_eq!(too_narrow, natural);
    }

    #[test]
    fn a_cell_that_has_gone_is_not_there_to_be_pressed() {
        // Geometry alone said yes: a shrinking cell keeps its full width for the rest of
        // the morph after its fade has finished, so a click on a preview that was already
        // closing re-focused — or closed — a window on its way out.
        let widths = [200, 200, 200];
        let layout = layout_menu(&widths);
        let middle = layout.cells[1];
        let x = f64::from(middle.x + 10);
        let y = f64::from(middle.y + 10);
        // With every cell live, this is the middle preview.
        assert_eq!(layout.hit_where(x, y, |_| true), Hit::Preview(1));
        // With the middle one gone, the point is dead rather than falling through to its
        // neighbours — pressing one window while aiming at another would be worse.
        assert_eq!(layout.hit_where(x, y, |index| index != 1), Hit::None);
        // And with only it live, it is still pressable.
        assert_eq!(layout.hit_where(x, y, |index| index == 1), Hit::Preview(1));
        // A cell that is still growing has its full footprint but is not yet there.
        assert_eq!(layout.hit_where(x, y, |_| false), Hit::None);
    }

    fn window(id: u64, app: &str) -> WindowInfo {
        WindowInfo {
            id,
            focused: false,
            minimized: false,
            app_id: app.to_string(),
            title: format!("{app} window {id}"),
        }
    }

    /// The row as four squares, 36px each with a 2px gap, so middles at 18, 56, 94, 132.
    fn four_squares() -> RowGeometry {
        RowGeometry {
            middles: vec![18.0, 56.0, 94.0, 132.0],
        }
    }

    #[test]
    fn the_gap_under_the_pointer_is_measured_against_the_row_with_the_placeholder_in_it() {
        // The placeholder is a child of the row like any other, so it contributes a
        // middle of its own. That is deliberate: the hole the carried square leaves is
        // exactly as wide as the square, and the pointer has to cross it to move the gap
        // on by one place.
        let row = four_squares();
        // Before the first square, after the last, and either side of each middle.
        assert_eq!(row.gap_at(4.0), 0);
        assert_eq!(row.gap_at(17.0), 0);
        assert_eq!(row.gap_at(19.0), 1);
        assert_eq!(row.gap_at(56.0), 1);
        assert_eq!(row.gap_at(58.0), 2);
        assert_eq!(row.gap_at(200.0), 4);
        // The row is re-measured as the placeholder moves, so the same x can name
        // different gaps as the drag runs. That is the whole of "rearrange live".
        let moved = RowGeometry {
            middles: vec![18.0, 56.0, 94.0, 132.0],
        };
        assert_eq!(moved.gap_at(58.0), 2);
    }

    #[test]
    fn a_row_of_one_square_is_never_a_reorder() {
        let row = RowGeometry {
            middles: vec![18.0],
        };
        assert_eq!(row.gap_at(-50.0), 0);
        assert_eq!(row.gap_at(500.0), 1);
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
        // Not the previews' 0.35, and the reason is in the rule itself: a cell is
        // 0.35 black *around an opaque capture*, so only its padding is see-through,
        // while every part of the app menu is background. Matching the number would
        // have matched the transparency and not the appearance, which is what
        // "more transparent than the thumbnail menu" was.
        // The bar's own panel colour, at the bar's own alpha, from the one place it is
        // defined. A square has no background of its own, so this is what a square is
        // made of, and it is a dark *grey* rather than the near-black it was.
        assert!(
            rule.contains(&format!(
                "rgba({}, {}, {}, {})",
                BAR_PANEL.0, BAR_PANEL.1, BAR_PANEL.2, BAR_PANEL_ALPHA
            )),
            "the app menu is not the colour a square is: {rule}"
        );
        // And nothing painted over the top of it. The theme's window background image
        // is the difference between "the same 35% black" and "a more solid panel than
        // the previews", and it is invisible in a stylesheet.
        assert!(
            rule.contains("background-image: none"),
            "the theme's window background is back over the app menu's fill: {rule}"
        );
        assert_eq!(CELL_FILL, (0.0, 0.0, 0.0, 0.35), "and the previews moved");
        // The mini-CSD adds no fill of its own. The cell's fill is already under it,
        // and setting the strip to the same value does not make it the same colour —
        // it stacks, and two 35% blacks are 58%, so the strip came out darker than
        // the cell it belongs to.
        assert_eq!(TITLEBAR_FILL.3, 0.0, "the strip must not paint over the cell");
        // Which is the one case where a fill *would* have been right: if the cell were
        // not already filled, the strip would need the colour itself.
        assert!(CELL_FILL.3 > 0.0, "the cell underneath is what shows through");
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
