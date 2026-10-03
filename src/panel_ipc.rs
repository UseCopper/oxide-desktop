//! Compositor side of the panel IPC.
//!
//! The panel is a separate GTK process and cannot read the compositor's window list
//! directly, so the two talk over a Unix socket speaking [`crate::panel_proto`] — the
//! length-prefixed framing defined there, which both halves of this connection share.
//!
//! Everything about *when* to speak lives here; everything about *how a message is
//! framed* lives in the protocol module. That split is deliberate: framing is the part
//! where a mistake is invisible until the panel stops working entirely.
//!
//! The socket is polled from [`AnvilState::tick_panel`] on the frame loop, so no calloop
//! source is needed and no panel message can arrive while the compositor is mid-frame.

use std::{
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

use smithay::utils::SERIAL_COUNTER as SCOUNTER;
use tracing::{debug, warn};

use crate::{
    focus::KeyboardFocusTarget,
    panel_proto::{self, FrameWriter},
    shell::WindowElement,
    state::{AnvilState, Backend},
};

/// How many window previews may be rendered in a single frame.
///
/// A preview is a bounded GPU downscale plus a small readback and an area average,
/// which is far too much to repeat for every window the panel happens to be asking about
/// at once. Capping it keeps the frame time flat; the panel re-asks, so the previews
/// take turns and each settles at frame_rate / n fps.
///
/// Two fits a 60Hz frame with room to spare, and is what lets four changing windows each
/// update at the panel's 30Hz. Drop it to 1 on a slower machine with
/// `OXIDE_PANEL_THUMBNAILS_PER_TICK`.
const PANEL_THUMBNAILS_PER_TICK: usize = 2;

/// How many queued requests one tick may look at, over and above the render budget.
///
/// Only a backstop: a window that needs no render is dropped from the queue as it is
/// seen, so this is not normally reached.
const PANEL_PREVIEWS_EXAMINED_PER_TICK: usize = 32;

/// Ceiling on unsent preview bytes before new previews are dropped.
///
/// Imagery is the first thing worth losing: a late preview is worth more than a stale
/// one, and the window list is what the panel needs to stay usable. The panel also
/// re-asks for anything it did not get, so a dropped frame is a delay rather than a
/// permanent blank.
const MAX_OUT_BACKLOG: usize = 4 << 20;

/// How long to wait before accepting a connection again after one has gone.
///
/// Without this, a panel that is restarted in a tight loop has the compositor accepting
/// and dropping a connection every tick. It also bounds how quickly a second client can
/// take over the socket, which matters because the socket has no authentication: see
/// [`PanelIpc::poll`].
const RECONNECT_GRACE_MS: u64 = 250;

fn panel_thumbnails_per_tick() -> usize {
    std::env::var("OXIDE_PANEL_THUMBNAILS_PER_TICK")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(PANEL_THUMBNAILS_PER_TICK)
}

/// The socket the panel connects to for a given Wayland display name.
pub fn socket_path(socket_name: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join(format!("oxide-panel-{socket_name}.sock"))
}

/// A message from the panel.
#[derive(Debug)]
pub enum PanelMessage {
    /// Focus the window with this id.
    Focus(u64),
    /// Close the window with this id, and only that one.
    Close(u64),
    /// Start the app with this id.
    ///
    /// An id rather than a command line, so the panel cannot ask for something to be run
    /// that was never an installed application: the compositor looks the id up in the
    /// desktop entries itself and runs what it finds, or nothing.
    Launch(String),
    /// The panel is showing a picker and wants a preview of this window, scaled to fit
    /// inside the given box.
    Preview {
        id: u64,
        max_width: i32,
        max_height: i32,
        /// The panel has nothing for this window, so answer even if the pixels have not
        /// changed.
        ///
        /// A window that cannot be rendered — minimized, so unmapped and with nothing to
        /// capture — can only be answered from the last frame captured for it, and the
        /// panel is asking precisely because it does not have that frame. Without this
        /// the request would be dropped as already-current and the panel would never get
        /// one, so its cell would stay blank and, since the menu waits for a preview of
        /// everything it is about to show, the menu would not open.
        wanted: bool,
    },
    /// The desktop accent color, as linear-ish sRGB components.
    Accent([f32; 3]),
}

/// A window preview on its way to the panel.
#[derive(Debug, Clone)]
pub struct PanelImage {
    pub id: u64,
    pub width: i32,
    pub height: i32,
    /// Tightly packed R, G, B, A rows, top-down, `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

/// The compositor's end of the panel connection.
#[derive(Debug)]
pub struct PanelIpc {
    listener: UnixListener,
    conn: Option<UnixStream>,
    reader: panel_proto::FrameReader,
    last_snapshot: String,
    /// Whole frames waiting to be handed to the kernel.
    out: FrameWriter,
    /// When a connection was last lost, so a flapping client cannot spin the compositor.
    last_disconnect: Option<std::time::Instant>,
}

/// What one parsed frame turned out to be.
#[derive(Debug)]
enum Incoming {
    Message(PanelMessage),
    /// Well-framed but not a message we know, or not one we will act on.
    ///
    /// Never fatal. A frame that is framed correctly has, by definition, been read past
    /// exactly: the length said where it ended, so a message we cannot interpret costs
    /// that one message and nothing else. Only the framer itself can report that a stream
    /// is unreadable, and it does so separately.
    Ignored,
}

impl PanelIpc {
    pub fn new(listener: UnixListener) -> Self {
        Self {
            listener,
            conn: None,
            reader: panel_proto::FrameReader::new(),
            last_snapshot: String::new(),
            out: FrameWriter::new(),
            last_disconnect: None,
        }
    }

    /// Whether a panel is attached.
    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// Accept a pending connection and return the messages received since the last call.
    pub fn poll(&mut self) -> Vec<PanelMessage> {
        self.accept();
        let Some(conn) = self.conn.as_mut() else {
            return Vec::new();
        };

        // Read everything the socket has, then hand it to the framer. Read and parse are
        // separate steps because a frame may be split across any number of reads; the
        // framer holds the partial one and this function does not need to care.
        let mut buf = [0u8; 64 * 1024];
        let mut fatal = None;
        loop {
            match conn.read(&mut buf) {
                Ok(0) => {
                    debug!("panel disconnected");
                    fatal = Some(panel_proto::FrameError::Closed);
                    break;
                }
                Ok(n) => self.reader.feed(&buf[..n]),
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    debug!(%err, "panel: read failed");
                    fatal = Some(panel_proto::FrameError::Closed);
                    break;
                }
            }
        }

        if fatal.is_some() {
            self.drop_connection();
            return Vec::new();
        }

        let mut messages = Vec::new();
        loop {
            let Some(frame) = self.reader.next_frame() else {
                break;
            };
            match frame {
                Err(err) => {
                    // A stream we cannot parse has no recoverable position, so there is
                    // nothing to do but close it and wait for the panel to come back.
                    debug!(%err, "panel: dropping connection");
                    self.drop_connection();
                    return Vec::new();
                }
                Ok(frame) => match parse_frame(&frame) {
                    Incoming::Message(message) => messages.push(message),
                    Incoming::Ignored => {}
                },
            }
        }
        messages
    }

    /// Take a pending connection, if the grace period has passed.
    ///
    /// The socket is unauthenticated, so anything that can `connect()` to it becomes
    /// *the* panel and the real one is displaced — and with it the ability to ask the
    /// compositor for a preview of any size it likes. That is a same-user nuisance
    /// rather than a privilege boundary (the socket is in the user's runtime directory,
    /// so only the user can reach it at all), but it is also a bug for the ordinary
    /// case: two panels started at once, or one restarting, would fight over the
    /// connection every frame. So the first connection wins and holds it until it
    /// drops.
    fn accept(&mut self) {
        if self.conn.is_some() {
            return;
        }
        if let Some(last) = self.last_disconnect
            && last.elapsed().as_millis() < RECONNECT_GRACE_MS as u128
        {
            return;
        }
        match self.listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(true).is_err() {
                    warn!("panel: could not make the connection non-blocking");
                    return;
                }
                self.conn = Some(stream);
                self.reader.reset();
                self.last_snapshot.clear();
                self.out.clear();
                tracing::info!("Panel connected");
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => debug!(%err, "panel: accept failed"),
        }
    }

    /// Forget the current panel, so a new one can take the socket.
    fn drop_connection(&mut self) {
        self.conn = None;
        self.reader.reset();
        // Whatever was queued was for a panel that is no longer listening.
        self.out.clear();
        self.last_snapshot.clear();
        self.last_disconnect = Some(std::time::Instant::now());
    }

    /// Send a window preview, if it fits the backlog and the frame is well-formed.
    ///
    /// Returns whether it was queued. A caller that has marked a window's preview as
    /// delivered must only do so when this says yes: a preview dropped here is one the
    /// panel never sees, and the panel has no way to ask again except by reporting that
    /// it has nothing.
    pub fn send_image(&mut self, image: &PanelImage) -> bool {
        if self.conn.is_none() {
            return false;
        }
        let Some(frame) = panel_proto::encode_image(
            image.id,
            image.width,
            image.height,
            &image.pixels,
        ) else {
            // Our own renderer produced something the framing cannot describe. Not
            // recoverable by retrying, and not the panel's fault.
            warn!(
                id = image.id,
                width = image.width,
                height = image.height,
                len = image.pixels.len(),
                "Refusing to send a preview whose size does not match its pixels",
            );
            return false;
        };
        if self.out.queued() + frame.len() > MAX_OUT_BACKLOG {
            debug!("panel: dropping preview, too far behind");
            return false;
        }
        self.out.push_image(frame);
        true
    }

    /// Hand as much of the backlog to the kernel as it will take.
    ///
    /// A short write or `WouldBlock` only means the panel has not caught up yet, so the
    /// remainder stays queued for the next tick — and stays a *whole frame*, since
    /// [`FrameWriter`] never lets a frame be split across the queue. Only a real error
    /// drops the connection.
    pub fn flush(&mut self) {
        let Some(conn) = self.conn.as_mut() else {
            self.out.clear();
            return;
        };
        loop {
            let Some(chunk) = self.out.next_chunk() else {
                return;
            };
            // The kernel cannot block here (the stream is non-blocking) and the chunk
            // borrows the writer, so the borrow is released before the write is recorded.
            let result = {
                let chunk = chunk.to_vec();
                conn.write(&chunk)
            };
            match result {
                Ok(0) => return,
                Ok(written) => self.out.advance(written),
                Err(err) if err.kind() == ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    debug!(?err, "panel: write failed");
                    self.drop_connection();
                    return;
                }
            }
        }
    }

    /// Queue a window-list snapshot if it differs from the last one sent.
    ///
    /// It supersedes whatever is queued. A queued preview is stale the moment a window
    /// moves, and an older window list is stale the moment this one exists, so both go:
    /// the window list is what the panel needs in order to stay usable, and imagery is
    /// the part it can afford to wait for. The one thing that cannot be dropped is a
    /// frame already part way into the socket — see [`FrameWriter::push_priority`].
    pub fn send(&mut self, snapshot: &str) {
        if snapshot == self.last_snapshot || self.conn.is_none() {
            return;
        }
        self.out.push_priority(snapshot);
        self.last_snapshot.clear();
        self.last_snapshot.push_str(snapshot);
    }
}

/// Read one frame into a message, or say why it is not one.
///
/// Everything here is written to be *total*: no input reaches the state machine without
/// going through a parse that has checked it, because these are the only values that
/// reach the rest of the compositor. An app id is a string a client chose; a window id
/// and a preview size are numbers that arrive over a socket with no authentication on
/// it. Anything that cannot be understood is dropped, and anything that cannot be read
/// *past* takes the connection with it.
fn parse_frame(frame: &[u8]) -> Incoming {
    // A preview is the only frame carrying pixels, and it is distinguished by the header
    // inside it rather than by the frame length.
    if frame.starts_with(b"img\t") {
        let Some(image) = panel_proto::decode_image(frame) else {
            // Well-framed, so the stream is fine — but this frame says nothing true about
            // a preview, and skipping it costs one image rather than the connection.
            debug!("panel: ignoring a malformed preview frame");
            return Incoming::Ignored;
        };
        // Refuse to decode pixels the panel could not possibly use. A header this large
        // can only come from a client that is not the panel.
        if panel_proto::image_len(image.width, image.height).is_none() {
            debug!(width = image.width, height = image.height, "panel: preview out of range");
            return Incoming::Ignored;
        }
        return Incoming::Ignored;
    }

    let Some(fields) = panel_proto::parse_text(frame) else {
        return Incoming::Ignored;
    };
    match fields.first().map(String::as_str) {
        Some("focus") => match one_number(&fields) {
            Some(id) => Incoming::Message(PanelMessage::Focus(id)),
            None => Incoming::Ignored,
        },
        Some("close") => match one_number(&fields) {
            Some(id) => Incoming::Message(PanelMessage::Close(id)),
            None => Incoming::Ignored,
        },
        Some("launch") => match fields.get(1) {
            // Already sanitized by the framing rule on the sending side, and bounded
            // here in case it was not.
            Some(id) if !id.is_empty() => {
                Incoming::Message(PanelMessage::Launch(panel_proto::truncate(id).to_string()))
            }
            _ => Incoming::Ignored,
        },
        Some("accent") => match parse_accent(&fields) {
            Some(rgb) => Incoming::Message(PanelMessage::Accent(rgb)),
            None => Incoming::Ignored,
        },
        Some("preview") => match parse_preview(&fields) {
            Some(preview) => Incoming::Message(preview),
            None => Incoming::Ignored,
        },
        // A verb from a future version, or one that has been retired. Both are fine: the
        // framing held, so the next frame is still readable.
        _ => Incoming::Ignored,
    }
}

/// The window id in a one-argument message.
///
/// Exactly two fields, verb and id. A message carrying more is not one this version
/// knows how to read, and guessing which field it meant is how a protocol starts
/// meaning different things to each end.
fn one_number(fields: &[String]) -> Option<u64> {
    match fields {
        [verb, id] if !verb.is_empty() => id.parse::<u64>().ok(),
        _ => None,
    }
}

/// `accent\t<red>\t<green>\t<blue>`.
fn parse_accent(fields: &[String]) -> Option<[f32; 3]> {
    let [_verb, red, green, blue, ..] = fields else {
        return None;
    };
    let components = [
        red.parse::<f32>().ok()?,
        green.parse::<f32>().ok()?,
        blue.parse::<f32>().ok()?,
    ];
    // A component outside 0..=1 is a NaN waiting to reach a shader, and `f32::parse`
    // accepts "inf" and "nan" outright.
    if components.iter().any(|c| !c.is_finite() || !(0.0..=1.0).contains(c)) {
        return None;
    }
    Some(components)
}

/// `preview\t<id>\t<width>\t<height>\t<wanted>`.
///
/// The size is checked against the ceiling rather than clamped: clamping would render at
/// one size and announce another, and the panel would then be waiting forever for a frame
/// whose length does not match its header.
fn parse_preview(fields: &[String]) -> Option<PanelMessage> {
    // `wanted` is optional; absent means the panel has nothing, which is the reading that
    // costs at most one cached image.
    let (Some(id), Some(width), Some(height)) = (
        fields.get(1)?.parse::<u64>().ok(),
        fields.get(2)?.parse::<i32>().ok(),
        fields.get(3)?.parse::<i32>().ok(),
    ) else {
        return None;
    };
    panel_proto::image_len(width, height)?;
    let wanted = match fields.get(4) {
        Some(flag) => flag == "1",
        None => true,
    };
    Some(PanelMessage::Preview {
        id,
        max_width: width,
        max_height: height,
        wanted,
    })
}

impl<BackendData: Backend> AnvilState<BackendData> {
    /// Publish the window list to the panel and handle its messages.
    /// Called once per frame; does nothing when no panel is connected.
    pub fn tick_panel(&mut self) {
        let messages = match self.panel_ipc.as_mut() {
            Some(ipc) => ipc.poll(),
            None => return,
        };
        if self.panel_ipc.as_ref().is_some_and(|ipc| !ipc.is_connected()) {
            self.forget_panel_previews();
            return;
        }

        // Windows the panel said it had nothing for.
        let mut panel_wants: Vec<u64> = Vec::new();
        // Preview renders are budgeted per tick. One costs a readback of the window's
        // whole content plus an area-average over it, so answering every request that has
        // piled up would put all of that in a single frame and blow the frame time.
        //
        // Requests are *queued*, coalesced by window, rather than answered from the socket
        // in the order they arrived. The panel asks for every window on a timer, so
        // answering in arrival order meant the same first couple of windows were served on
        // every tick and everything after them was never answered at all — one preview
        // updating and the rest frozen. Queued, each window waits its turn, and the whole
        // set settles at frame_rate / n fps.
        for message in messages {
            match message {
                PanelMessage::Focus(id) => self.focus_panel_window(id),
                PanelMessage::Close(id) => self.close_panel_window(id),
                PanelMessage::Launch(id) => self.launch_app(&id),
                PanelMessage::Accent(rgb) => crate::shell::set_preview_color(rgb),
                PanelMessage::Preview {
                    id,
                    max_width,
                    max_height,
                    wanted,
                } => {
                    if wanted {
                        panel_wants.push(id);
                    }
                    if self.panel_preview_queued.insert(id) {
                        self.panel_preview_queue.push_back(id);
                    }
                    // The most recent ask wins for the size. Kept here, on the state, rather
                    // than in a map rebuilt every tick: a window asked for now is usually
                    // served several ticks later, by which time a per-tick map had forgotten
                    // it and the request was dropped unanswered.
                    self.panel_preview_sizes.insert(id, (max_width, max_height));
                }
            }
        }

        // The budget counts *renders*, not requests looked at. A window already showing
        // the right pixels costs nothing to skip and must not use a slot: otherwise the
        // same windows at the front of the queue are examined every tick, render nothing
        // because they are static, and a window that has actually changed behind them is
        // never reached at all. That is what a multi-window menu looks like from here —
        // the frames never arrive.
        let budget = panel_thumbnails_per_tick();
        // The window list goes out *before* any preview is rendered this tick.
        //
        // The order matters, and it was the wrong way round: a new snapshot supersedes
        // whatever is still queued, so rendering first meant every preview produced on a
        // tick where the window list also changed was discarded before it left. Worse, the
        // window had already been recorded as previewed by then, so it was never asked for
        // again — the cell stayed blank until the menu was closed and reopened. That is the
        // "previews never appear" symptom, and it was worst exactly when the panel had the
        // most to redraw.
        let snapshot = self.panel_snapshot();
        if let Some(ipc) = self.panel_ipc.as_mut() {
            ipc.send(&snapshot);
        }

        let mut rendered = 0;
        let mut examined = 0;
        while rendered < budget && examined < PANEL_PREVIEWS_EXAMINED_PER_TICK {
            let Some(id) = self.panel_preview_queue.front().copied() else {
                break;
            };
            examined += 1;
            let forget = |state: &mut Self, id: u64| {
                state.panel_preview_queue.pop_front();
                state.panel_preview_queued.remove(&id);
                state.panel_preview_sizes.remove(&id);
            };
            // No size recorded means a stale entry left over from an earlier request.
            let Some((max_width, max_height)) = self.panel_preview_sizes.get(&id).copied() else {
                forget(self, id);
                continue;
            };
            // A window that has gone is not worth a special case: popping it and finding
            // nothing to render is the same outcome, and this way there is one place that
            // knows what a live window is. Forget it and drop its cached pixels while we
            // are here, so a closed window does not sit in the cache for the rest of the
            // session.
            if self.panel_window(id).is_none() {
                forget(self, id);
                self.panel_cached_previews.remove(&id);
                continue;
            }
            if self.panel_preview_is_current(id) && !panel_wants.contains(&id) {
                // Already right on the panel's screen. Drop the request, and do not spend
                // one of the renders on finding that out.
                forget(self, id);
                continue;
            }
            forget(self, id);
            self.answer_preview_request(id, max_width, max_height, &mut rendered);
        }

        if let Some(ipc) = self.panel_ipc.as_mut() {
            ipc.flush();
        }
    }

    /// Render, or recall, the preview one queued request asked for.
    ///
    /// Split out of [`Self::tick_panel`] so the one thing that has to be right — a window
    /// is not marked as previewed until the panel is actually going to be sent the pixels
    /// — is in one place.
    fn answer_preview_request(
        &mut self,
        id: u64,
        max_width: i32,
        max_height: i32,
        rendered: &mut usize,
    ) {
        // A live render if there is one, and the last captured frame if there is not: a
        // minimized window is unmapped, so there is nothing to capture and the panel is
        // left showing whatever it last had, which for a window it has never seen is
        // nothing at all. Holding the frame here is what lets a minimized window keep a
        // preview instead of going blank.
        let image = match self.render_panel_thumbnail(id, max_width, max_height) {
            Some(image) => Some(image),
            None => self.panel_cached_preview(id),
        };
        let Some(image) = image else {
            return;
        };
        let answered_from_cache = !self.panel_preview_is_current(id);
        // Only a real render costs a render slot. Answering from the cache hands over bytes
        // that are already in memory, and charging it for one would let a row of minimized
        // windows starve the live ones behind them.
        if !answered_from_cache {
            *rendered += 1;
        }

        // The window's pixels must be marked as sent only once they are on their way. It
        // used to be marked at the moment of the render, before the backlog check, so a
        // preview dropped for being too far behind was never asked for again and the cell
        // stayed blank until something else happened to rebuild it.
        let delivered = if answered_from_cache {
            // Nothing was rendered, so nothing needs marking: the cached frame is being
            // handed over precisely because the pixels have not changed.
            self.panel_ipc
                .as_mut()
                .is_some_and(|ipc| ipc.send_image(&image))
        } else {
            let delivered = self
                .panel_ipc
                .as_mut()
                .is_some_and(|ipc| ipc.send_image(&image));
            // Marked only now, once the frame is on its way. Marking it at the moment of
            // the render — which is what this used to do — meant a preview dropped for
            // being too far behind was never asked for again, and the cell stayed blank
            // until something unrelated rebuilt it.
            if delivered {
                self.mark_previewed(id);
            }
            delivered
        };
        if !delivered {
            return;
        }
        // Our copy, which is what a later request for a now-unmappable window is answered
        // from. It is also the only reason a minimized window keeps a preview at all.
        self.panel_cached_previews.insert(id, image);
    }

    /// Render a preview of the window the panel asked about, if it is still around.
    fn render_panel_thumbnail(
        &mut self,
        id: u64,
        max_width: i32,
        max_height: i32,
    ) -> Option<PanelImage> {
        let window = self.panel_window(id)?;
        let max = smithay::utils::Size::<i32, smithay::utils::Buffer>::from((max_width, max_height));
        let (size, pixels) = self.backend_data.panel_thumbnail(&window, max)?;
        Some(PanelImage {
            id,
            width: size.w,
            height: size.h,
            pixels,
        })
    }

    /// Answer a request from the last frame captured for that window, if any.
    fn panel_cached_preview(&self, id: u64) -> Option<PanelImage> {
        self.panel_cached_previews.get(&id).cloned()
    }

    /// Whether the panel is already showing the right pixels for this window.
    ///
    /// Rendering a preview reads the window's full content back off the GPU, so a window
    /// whose contents have not moved on is skipped. That is what lets the picker poll fast
    /// enough to look live — and it is why skipping must not cost the same as rendering.
    fn panel_preview_is_current(&self, id: u64) -> bool {
        let Some(window) = self.panel_window(id) else {
            return false;
        };
        // Through `with_state`, so the borrow cannot span anything. Two
        // `decoration_state()` calls in a single expression are two `RefMut`s alive at
        // once and the second panics, which is what opening a menu with more than one
        // window did.
        window.with_state(|state| state.previewed_generation == Some(state.content_generation))
    }

    /// Note that the panel has been sent this window's current pixels.
    fn mark_previewed(&self, id: u64) {
        if let Some(window) = self.panel_window(id) {
            let generation = window.with_state(|state| state.content_generation);
            window.with_state(|state| state.previewed_generation = Some(generation));
        }
    }

    /// The window with this panel id, if it is still open (minimized windows included,
    /// since the picker lists those too).
    fn panel_window(&self, id: u64) -> Option<crate::shell::WindowElement> {
        self.space
            .elements()
            .chain(self.minimized.iter().map(|(window, _)| window))
            .find(|window| {
                !window.is_ghosting()
                    && window.with_state(|state| state.panel_id) == Some(id)
            })
            .cloned()
    }

    /// Drop everything remembered about a panel that is no longer there.
    fn forget_panel_previews(&mut self) {
        if self.panel_cached_previews.is_empty()
            && self.panel_preview_queue.is_empty()
            && self.panel_preview_sizes.is_empty()
            && self.panel_preview_queued.is_empty()
        {
            return;
        }
        self.panel_cached_previews.clear();
        self.panel_preview_queue.clear();
        self.panel_preview_sizes.clear();
        self.panel_preview_queued.clear();
        // The windows' own marks, too: the pixels are gone with the cached frames, so a
        // window that thinks it has been previewed would never be sent again. Marking them
        // unpreviewed is what makes the panel's first request after a reconnect — which is
        // every window, since the panel starts with nothing — actually render.
        for window in self
            .space
            .elements()
            .chain(self.minimized.iter().map(|(window, _)| window))
        {
            window.with_state(|state| state.previewed_generation = None);
        }
    }

    /// Close one specific window, as asked by a picker entry's close button.
    fn close_panel_window(&mut self, id: u64) {
        let Some(window) = self.panel_window(id) else {
            return;
        };
        self.close_window(window);
    }

    /// Start the app the panel named, from its desktop entry.
    ///
    /// Spawned here rather than by the panel, for two reasons. The app becomes a child of
    /// the session rather than of the panel, which the compositor can outlive; and the
    /// command that runs is one the compositor looked up itself, out of the installed
    /// entries, rather than a string the panel sent over a socket.
    ///
    /// An id with no entry, or an entry with nothing runnable in it, is reported and
    /// otherwise ignored. A pinned app whose entry has since been uninstalled is the case
    /// that actually happens.
    fn launch_app(&mut self, id: &str) {
        use std::process::{Command, Stdio};

        let Some(app) = crate::desktop::lookup(id) else {
            warn!(app = %id, "Panel asked to launch an app with no desktop entry");
            return;
        };
        let Some((program, arguments)) = app.command() else {
            warn!(app = %id, exec = %app.exec, "Desktop entry has nothing runnable");
            return;
        };

        let mut command = Command::new(&program);
        command.args(&arguments);
        // The app has to land on *this* compositor, not on whatever this process inherited.
        // A nested session's compositor is itself a Wayland client of the one above it, so
        // its `WAYLAND_DISPLAY` is the outer display and an app launched with it would open
        // a window somewhere else entirely.
        //
        // Unless the entry sets one itself — `Exec=env WAYLAND_DISPLAY=foo app` is a thing
        // people write, and overriding that would break it.
        if !crate::desktop::split_command(&app.exec)
            .iter()
            .any(|word| word.starts_with("WAYLAND_DISPLAY="))
            && let Some(socket_name) = self.socket_name.clone()
        {
            command.env("WAYLAND_DISPLAY", &socket_name);
        }
        let spawned = command
            // Nothing of ours on its standard streams: the app would otherwise inherit this
            // process's terminal and be killed with it.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => {
                tracing::info!(app = %id, name = %app.label(), pid = child.id(), "Launched");
                // Deliberately not waited on. Reaping it would mean holding a list of
                // children for a panel that is a UI and has better things to do, and a child
                // that exits on its own is reaped by init either way.
                drop(child);
            }
            Err(err) => warn!(app = %id, program = %program, %err, "Failed to launch app"),
        }
    }

    /// Build the `list\t...` snapshot of every open window, including minimized ones
    /// (which are unmapped from `Space`). Each window gets its own entry, so several
    /// instances of the same app stay distinct.
    ///
    /// Minimized windows are sent, and flagged. They are not gone — the app still has them,
    /// and the bar should say so — but there is nothing on screen to capture a preview
    /// from, so the panel must not offer one. Sending them and letting the panel decide is
    /// the difference between "minimized" and "closed"; hiding them here made the two look
    /// the same.
    fn panel_snapshot(&mut self) -> String {
        let mut windows: Vec<(WindowElement, bool)> = self
            .space
            .elements()
            .cloned()
            .map(|window| (window, false))
            .collect();
        windows.extend(self.minimized.iter().map(|(window, _)| (window.clone(), true)));

        // Highlight whatever actually has keyboard focus, not the newest window.
        let focused = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|target| match target {
                KeyboardFocusTarget::Window(window) => Some(WindowElement(window)),
                _ => None,
            });

        let mut entries = String::new();
        let mut count = 0u32;
        for (window, minimized) in &windows {
            if window.is_ghosting() {
                continue;
            }
            // Assign a stable id the first time the window is published.
            let id = match window.with_state(|state| state.panel_id) {
                Some(id) => id,
                None => {
                    let id = self.next_panel_id;
                    self.next_panel_id += 1;
                    window.with_state(|state| state.panel_id = Some(id));
                    id
                }
            };

            // One tab-separated field each, in this order, which is what
            // `parse_snapshot` reads: focused, minimized, app id, title. Bounded, because
            // a title is whatever a client passed to `set_title` and the panel measures it
            // with Pango on every repaint.
            entries.push('\t');
            entries.push_str(&id.to_string());
            entries.push('\t');
            entries.push(if focused.as_ref() == Some(window) { '1' } else { '0' });
            entries.push('\t');
            entries.push(if *minimized { '1' } else { '0' });
            entries.push('\t');
            entries.push_str(&panel_proto::sanitize(&panel_proto::truncate(
                &window.app_id().unwrap_or_default(),
            )));
            entries.push('\t');
            entries.push_str(&panel_proto::sanitize(&panel_proto::truncate(
                &window.title().unwrap_or_default(),
            )));
            count += 1;
        }
        format!("list\t{count}{entries}")
    }

    /// Focus and raise the window the panel asked for.
    fn focus_panel_window(&mut self, id: u64) {
        let Some(window) = self.panel_window(id) else {
            return;
        };

        if self.minimized.iter().any(|(w, _)| w == &window) {
            self.unminimize_request(window.clone());
        }
        self.space.raise_element(&window, true);
        #[cfg(feature = "xwayland")]
        if let Some(surface) = window.0.x11_surface()
            && let Some(xwm) = self.xwm.as_mut()
        {
            let _ = xwm.raise_window(surface);
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(window.into()), SCOUNTER.next_serial());
        }
    }
}

/// Create the panel's listening socket and start the panel process.
pub fn spawn_panel(state: &mut AnvilState<impl Backend>) {
    use std::process::{Command, Stdio};

    if std::env::var_os("OXIDE_NO_PANEL").is_some() {
        return;
    }
    let Some(socket_name) = state.socket_name.clone() else {
        return;
    };

    let path = socket_path(&socket_name);
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            warn!(path = %path.display(), %err, "Failed to bind panel socket");
            return;
        }
    };
    if let Err(err) = listener.set_nonblocking(true) {
        warn!(%err, "Failed to set panel socket non-blocking");
    }
    state.panel_ipc = Some(PanelIpc::new(listener));

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            warn!("Failed to locate the panel executable: {}", err);
            return;
        }
    };

    match Command::new(exe)
        .arg("--panel")
        .env("WAYLAND_DISPLAY", &socket_name)
        .env("GDK_BACKEND", "wayland")
        .env("OXIDE_PANEL_SOCKET", &path)
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(_) => tracing::info!("Started the panel on {}", socket_name),
        Err(err) => warn!("Failed to start the panel: {}", err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body of a frame carrying `message`, which is what the framer hands to
    /// [`parse_frame`] once it has taken the length prefix off.
    fn body(message: &str) -> Vec<u8> {
        let framed = panel_proto::encode_text(message);
        let newline = framed.iter().position(|&b| b == b'\n').unwrap();
        framed[newline + 1..].to_vec()
    }

    fn message(frame: &str) -> Option<PanelMessage> {
        match parse_frame(&body(frame)) {
            Incoming::Message(message) => Some(message),
            other => panic!("{frame:?} was not a message: {other:?}"),
        }
    }

    fn ignored(frame: &str) -> bool {
        parse_frame(&body(frame)).is_ignored()
    }

    #[test]
    fn the_panel_can_ask_for_the_things_it_needs() {
        assert!(matches!(message("focus\t7"), Some(PanelMessage::Focus(7))));
        assert!(matches!(message("close\t7"), Some(PanelMessage::Close(7))));
        assert!(matches!(message("launch\tfirefox"), Some(PanelMessage::Launch(a)) if a == "firefox"));
        assert!(matches!(
            message("preview\t7\t360\t118\t0"),
            Some(PanelMessage::Preview { id: 7, max_width: 360, max_height: 118, wanted: false })
        ));
        assert!(matches!(
            message("accent\t0.5\t0.5\t0.5"),
            Some(PanelMessage::Accent([0.5, 0.5, 0.5]))
        ));
    }

    #[test]
    fn an_absent_wanted_flag_means_the_panel_has_nothing() {
        // The safe reading: the cost of answering from the last frame is one cached image.
        assert!(matches!(
            message("preview\t7\t360\t118"),
            Some(PanelMessage::Preview { wanted: true, .. })
        ));
    }

    #[test]
    fn a_request_for_an_impossible_preview_is_refused_rather_than_honoured() {
        // A preview is a GPU readback of the window's whole content plus an area average,
        // on the frame loop. A client that can reach this socket could otherwise ask for a
        // 100000x100000 image.
        for absurd in [
            "preview\t1\t100000\t100000\t1",
            "preview\t1\t100000\t100\t1",
            "preview\t1\t0\t118\t1",
            "preview\t1\t-360\t118\t1",
            "preview\t1\t360\t100000\t1",
        ] {
            assert!(
                ignored(absurd),
                "{absurd} should not have been honoured"
            );
        }
        // And the size the panel itself asks for still works.
        assert!(message("preview\t1\t360\t118\t1").is_some());
    }

    #[test]
    fn a_malformed_message_is_dropped_and_the_stream_is_not() {
        // Every one of these is a well-framed frame that says nothing actionable. The
        // frame length held, so the message after it still arrives — which is the property
        // that matters, and the reason these are `Ignored` rather than `Fatal`.
        for bad in [
            "",
            "focus",
            "focus\t",
            "focus\tseven",
            "focus\t-1",
            "focus\t99999999999999999999",
            "close",
            "close\tnope",
            "launch",
            "launch\t",
            "preview",
            "preview\t1",
            "preview\tx\t360\t118",
            "preview\t1\t360",
            "accent",
            "accent\t1",
            "accent\tx\ty\tz",
            "nonsense\t1\t2\t3",
            "focus\t1\t2\t3\t4",
        ] {
            assert!(
                ignored(bad),
                "{bad:?} should have been ignored"
            );
        }
    }

    #[test]
    fn an_accent_that_would_reach_a_shader_as_nan_is_refused() {
        // `f32::parse` accepts these three outright.
        for bad in [
            "accent\tnan\tnan\tnan",
            "accent\tinf\t0\t0",
            "accent\t-0.5\t0\t0",
            "accent\t1.5\t0\t0",
        ] {
            assert!(
                ignored(bad),
                "{bad} should have been ignored"
            );
        }
        // The extremes that *are* in range are accepted.
        assert!(message("accent\t0\t0\t0").is_some());
        assert!(message("accent\t1\t1\t1").is_some());
    }

    #[test]
    fn an_app_id_arrives_as_one_field_however_awkward_it_is() {
        // The panel sanitizes before sending, so this is belt and braces — but an id that
        // reached `Command::new` with a newline in it would be worth refusing.
        match parse_frame(&body("launch\tan\tapp\twith\ttabs")) {
            Incoming::Message(PanelMessage::Launch(id)) => assert_eq!(id, "an"),
            other => panic!("{other:?}"),
        }
        // And a long one is bounded rather than passed on whole.
        let long = "x".repeat(panel_proto::MAX_FIELD_BYTES * 2);
        match parse_frame(&body(&format!("launch\t{long}"))) {
            Incoming::Message(PanelMessage::Launch(id)) => {
                assert_eq!(id.len(), panel_proto::MAX_FIELD_BYTES)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_preview_frame_the_panel_could_not_use_is_ignored() {
        // The compositor never sends one of these, but the panel's reader shares this
        // parser and must not allocate for a header that claims an enormous image.
        let huge = format!(
            "img\t1\t{}\t{}\n",
            panel_proto::MAX_IMAGE_DIM + 1,
            panel_proto::MAX_IMAGE_DIM + 1
        );
        assert!(parse_frame(huge.as_bytes()).is_ignored());
        // Short payload for the size claimed.
        assert!(parse_frame(b"img\t1\t4\t4\nabcd").is_ignored());
        // And a good one is still just ignored here, because the compositor's own reader
        // never receives one: images only go the other way.
        assert!(parse_frame(&panel_proto::encode_image(1, 2, 2, &[0u8; 16]).unwrap()).is_ignored());
    }

    impl Incoming {
        fn is_ignored(&self) -> bool {
            matches!(self, Incoming::Ignored)
        }

        }
}