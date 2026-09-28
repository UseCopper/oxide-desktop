//! Compositor side of the panel IPC.
//!
//! The panel is a separate GTK process, so it cannot read the compositor's
//! window list directly. A tiny newline/tab-delimited protocol over a Unix
//! socket connects the two:
//!
//! * compositor -> panel: `list\t<count>\t<id>\t<focused>\t<app_id>\t<title>...`
//! * panel -> compositor: `focus\t<id>`
//!
//! The socket is polled from [`AnvilState::tick_panel`] (driven by the frame
//! loop), so no calloop source is needed.

use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

use smithay::utils::SERIAL_COUNTER as SCOUNTER;
use tracing::{debug, warn};

use crate::{
    focus::KeyboardFocusTarget,
    shell::WindowElement,
    state::{AnvilState, Backend},
};

/// How many window previews may be rendered in a single frame.
///
/// A preview is a bounded GPU downscale plus a small readback and an area
/// average, which is far too much to repeat for every window the panel happens
/// to be asking about at once. Capping it keeps the frame time flat; the panel
/// re-asks, so the previews take turns and each settles at frame_rate / n fps.
///
/// Two fits a 60Hz frame with room to spare, and is what lets four changing
/// windows each update at the panel's 30Hz. Drop it to 1 on a slower machine
/// with `OXIDE_PANEL_THUMBNAILS_PER_TICK`.
const PANEL_THUMBNAILS_PER_TICK: usize = 2;

/// The per-tick preview budget, overridable for a faster GPU.
/// How many queued requests one tick may look at, over and above the render budget.
///
/// Only a backstop: a window that needs no render is dropped from the queue as it is
/// seen, so this is not normally reached.
const PANEL_PREVIEWS_EXAMINED_PER_TICK: usize = 32;

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
    /// The panel is showing a picker and wants a preview of this window, scaled
    /// to fit inside the given box.
    Preview {
        id: u64,
        max_width: i32,
        max_height: i32,
        /// The panel has nothing for this window, so answer even if the pixels have
        /// not changed.
        ///
        /// A window that cannot be rendered — minimized, so unmapped and with nothing
        /// to capture — can only be answered from the last frame captured for it, and
        /// the panel is asking precisely because it does not have that frame. Without
        /// this the request would be dropped as already-current and the panel would
        /// never get one, so its cell would stay blank and, since the menu waits for
        /// a preview of everything it is about to show, the menu would not open.
        wanted: bool,
    },
    /// The desktop accent color, as linear-ish sRGB components.
    Accent([f32; 3]),
}

/// A window preview on its way to the panel: a header line naming the window
/// and the pixel size, followed by exactly `len` bytes of image data.
///
/// The payload is length-prefixed rather than newline-terminated because raw
/// pixels contain newlines, which would otherwise truncate the message.
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
    read_buf: Vec<u8>,
    last_snapshot: String,
    /// Bytes not yet accepted by the kernel.
    ///
    /// The compositor's end of this socket is non-blocking, so a write that
    /// outruns the panel is a `WouldBlock` rather than a failure. Buffering the
    /// rest and retrying each tick is what stops a burst of previews from being
    /// mistaken for a dead connection.
    out: VecDeque<u8>,
}

/// Ceiling on unsent preview bytes before new previews are dropped.
///
/// Imagery is the first thing worth losing: a late preview is worth more than a
/// stale one, and the window list is what the panel needs to stay usable.
const MAX_OUT_BACKLOG: usize = 4 << 20;

impl PanelIpc {
    pub fn new(listener: UnixListener) -> Self {
        Self {
            listener,
            conn: None,
            read_buf: Vec::new(),
            last_snapshot: String::new(),
            out: VecDeque::new(),
        }
    }

    /// Accept a pending connection and return the focus requests received since
    /// the last call.
    pub fn poll(&mut self) -> Vec<PanelMessage> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(true);
                self.conn = Some(stream);
                // Force a fresh snapshot for the new connection, and drop
                // anything still queued for the previous one.
                self.last_snapshot.clear();
                self.out.clear();
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) => debug!(%err, "panel: accept failed"),
        }

        if let Some(conn) = self.conn.as_mut() {
            let mut buf = [0u8; 1024];
            loop {
                match conn.read(&mut buf) {
                    Ok(0) => {
                        debug!("panel disconnected");
                        self.conn = None;
                        break;
                    }
                    Ok(n) => self.read_buf.extend_from_slice(&buf[..n]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                    Err(err) => {
                        debug!(%err, "panel: read failed");
                        self.conn = None;
                        break;
                    }
                }
            }
        }

        let mut messages = Vec::new();
        while let Some(newline) = self.read_buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.read_buf.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            if let Some(rest) = line.strip_prefix("focus\t") {
                if let Ok(id) = rest.trim().parse::<u64>() {
                    messages.push(PanelMessage::Focus(id));
                }
            } else if let Some(rest) = line.strip_prefix("close\t") {
                if let Ok(id) = rest.trim().parse::<u64>() {
                    messages.push(PanelMessage::Close(id));
                }
            } else if let Some(rest) = line.strip_prefix("preview\t") {
                let mut parts = rest.split('\t');
                if let (Some(id), Some(w), Some(h)) = (parts.next(), parts.next(), parts.next())
                    && let (Ok(id), Ok(w), Ok(h)) = (id.parse(), w.parse(), h.parse())
                    && w > 0
                    && h > 0
                {
                    // A fourth field: whether the panel already has an image for
                    // this window. Absent means no, which is the safe reading — the
                    // cost of answering with the last frame is one cached image.
                    let wanted = parts.next().map(|flag| flag == "1").unwrap_or(true);
                    messages.push(PanelMessage::Preview {
                        id,
                        max_width: w,
                        max_height: h,
                        wanted,
                    });
                }
            } else if let Some(rest) = line.strip_prefix("accent\t") {
                let mut parts = rest.split('\t');
                if let (Some(r), Some(g), Some(b)) = (parts.next(), parts.next(), parts.next())
                    && let (Ok(r), Ok(g), Ok(b)) =
                        (r.parse::<f32>(), g.parse::<f32>(), b.parse::<f32>())
                {
                    messages.push(PanelMessage::Accent([r, g, b]));
                }
            }
        }
        messages
    }

    /// Send a window preview: a header line, then the pixel payload.
    pub fn send_image(&mut self, image: PanelImage) {
        if self.conn.is_none() {
            return;
        }
        // Backpressure: a preview that would not be delivered for a long time is
        // not worth the buffer it would take from the next one.
        if self.out.len() > MAX_OUT_BACKLOG {
            debug!("panel: dropping preview, too far behind");
            return;
        }
        let header = format!(
            "img\t{}\t{}\t{}\t{}\n",
            image.id,
            image.width,
            image.height,
            image.pixels.len()
        );
        self.out.extend(header.as_bytes());
        self.out.extend(&image.pixels);
    }

    /// Hand as much of the backlog to the kernel as it will take.
    ///
    /// A short write or `WouldBlock` only means the panel has not caught up yet,
    /// so the remainder stays queued for the next tick. Only a real error drops
    /// the connection.
    pub fn flush(&mut self) {
        let Some(conn) = self.conn.as_mut() else {
            self.out.clear();
            return;
        };
        while !self.out.is_empty() {
            // The queue may wrap, in which case the head is the second slice.
            let (head, tail) = self.out.as_slices();
            let chunk = if head.is_empty() { tail } else { head };
            match conn.write(chunk) {
                Ok(0) => break,
                Ok(written) => {
                    self.out.drain(..written);
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    debug!(?err, "panel: write failed");
                    self.conn = None;
                    self.out.clear();
                    return;
                }
            }
        }
    }

    /// Queue a window-list snapshot if it differs from the last one sent.
    ///
    /// It goes to the front of the backlog: any queued previews are stale by now,
    /// and the window list is what the panel needs in order to stay responsive,
    /// so imagery is dropped in favour of fresh state.
    pub fn send(&mut self, snapshot: &str) {
        if snapshot == self.last_snapshot || self.conn.is_none() {
            return;
        }
        self.out.clear();
        self.out.extend(snapshot.as_bytes());
        self.out.push_back(b'\n');
        self.last_snapshot.clear();
        self.last_snapshot.push_str(snapshot);
    }
}

impl<BackendData: Backend> AnvilState<BackendData> {
    /// Publish the window list to the panel and handle its focus requests.
    /// Called once per frame; does nothing when no panel is connected.
    pub fn tick_panel(&mut self) {
        let messages = match self.panel_ipc.as_mut() {
            Some(ipc) => ipc.poll(),
            None => return,
        };
        // Windows the panel said it had nothing for.
        let mut panel_wants: Vec<u64> = Vec::new();
        // Preview renders are budgeted per tick. One costs a readback of the
        // window's whole content plus an area-average over it, so answering every
        // request that has piled up would put all of that in a single frame and
        // blow the frame time.
        //
        // Requests are *queued*, coalesced by window, rather than answered from the
        // socket in the order they arrived. The panel asks for every window on a
        // timer, so answering in arrival order meant the same first couple of
        // windows were served on every tick and everything after them was never
        // answered at all — one preview updating and the rest frozen. Queued, each
        // window waits its turn, and the whole set settles at frame_rate / n fps.
        for message in messages {
            match message {
                PanelMessage::Focus(id) => self.focus_panel_window(id),
                PanelMessage::Close(id) => self.close_panel_window(id),
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
                    // The most recent ask wins for the size. Kept here, on the state,
                    // rather than in a map rebuilt every tick: a window asked for now
                    // is usually served several ticks later, by which time a per-tick
                    // map had forgotten it and the request was dropped unanswered.
                    self.panel_preview_sizes.insert(id, (max_width, max_height));
                }
            }
        }
        // A window that has gone is not worth a special case here: popping it and
        // finding nothing to render is the same outcome, and this way there is one
        // place that knows what a live window is.
        // The budget counts *renders*, not requests looked at. A window already
        // showing the right pixels costs nothing to skip and must not use a slot:
        // otherwise the same windows at the front of the queue are examined every
        // tick, render nothing because they are static, and a window that has
        // actually changed behind them is never reached at all. That is what a
        // multi-window menu looks like from here — the frames never arrive.
        //
        // Bounded so that a queue full of unchanged windows cannot spin: past this
        // many the rest waits for the next tick, which is the same as before.
        let budget = panel_thumbnails_per_tick();
        // Carried out of the message pass: which of the queued windows the panel
        // said it had no image for, and so must be answered even if the pixels have
        // not moved.
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
            if self.panel_preview_is_current(id) && !panel_wants.contains(&id) {
                // Already right on the panel's screen. Drop the request, and do not
                // spend one of the renders on finding that out.
                forget(self, id);
                continue;
            }
            forget(self, id);
            // A live render if there is one, and the last captured frame if there is
            // not: a minimized window is unmapped, so there is nothing to capture and
            // the panel is left showing whatever it last had, which for a window it
            // has never seen is nothing at all. Holding the frame here is what lets a
            // minimized window keep a preview instead of going blank.
            let rendered_fresh = self.render_panel_thumbnail(id, max_width, max_height);
            let image = match rendered_fresh {
                Some(image) => {
                    self.panel_cached_previews.insert(id, image.clone());
                    Some(image)
                }
                // Nothing to capture — a minimized window is unmapped — so answer
                // from the last frame that was captured for it.
                None => self.panel_cached_preview(id),
            };
            if let Some(image) = image {
                rendered += 1;
                if let Some(ipc) = self.panel_ipc.as_mut() {
                    ipc.send_image(image);
                }
            }
        }
        let snapshot = self.panel_snapshot();
        if let Some(ipc) = self.panel_ipc.as_mut() {
            ipc.send(&snapshot);
            ipc.flush();
        }
    }

    /// Render a preview of the window the panel asked about, if it is still
    /// around.
    ///
    /// At most one is rendered per tick: the panel asks for a preview when it
    /// opens its picker, and a burst of requests for windows that have since
    /// closed would otherwise stall the frame loop on a readback each.
    fn render_panel_thumbnail(
        &mut self,
        id: u64,
        max_width: i32,
        max_height: i32,
    ) -> Option<PanelImage> {
        let window = self.panel_window(id)?;
        let max = smithay::utils::Size::<i32, smithay::utils::Buffer>::from((max_width, max_height));
        let Some((size, pixels)) = self.backend_data.panel_thumbnail(&window, max) else {
            return None;
        };
        let generation = window.with_state(|state| state.content_generation);
        window.with_state(|state| state.previewed_generation = Some(generation));
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
    /// Rendering a preview reads the window's full content back off the GPU, so a
    /// window whose contents have not moved on is skipped. That is what lets the
    /// picker poll fast enough to look live — and it is why skipping must not cost
    /// the same as rendering.
    fn panel_preview_is_current(&self, id: u64) -> bool {
        let Some(window) = self.panel_window(id) else {
            return false;
        };
        // Through `with_state`, so the borrow cannot span anything. Two
        // `decoration_state()` calls in a single expression are two `RefMut`s alive
        // at once and the second panics, which is what opening a menu with more than
        // one window did.
        window.with_state(|state| state.previewed_generation == Some(state.content_generation))
    }

    /// The window with this panel id, if it is still open (minimized windows
    /// included, since the picker lists those too).
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

    /// Close one specific window, as asked by a picker entry's close button.
    fn close_panel_window(&mut self, id: u64) {
        let Some(window) = self.panel_window(id) else {
            return;
        };
        self.close_window(window);
    }

    /// Build the `list\t...` snapshot of every open window, including minimized
    /// ones (which are unmapped from `Space`). Each window gets its own entry, so
    /// several instances of the same app stay distinct.
    ///
    /// Minimized windows are sent, and flagged. They are not gone — the app still has
    /// them, and the bar should say so — but there is nothing on screen to capture a
    /// preview from, so the panel must not offer one. Sending them and letting the
    /// panel decide is the difference between "minimized" and "closed"; hiding them
    /// here made the two look the same.
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
            // `parse_snapshot` reads: id, focused, minimized, app id, title. Removing
            // one of these means removing its leading tab too, or the panel reads the
            // gap as a field and rejects the whole snapshot.
            let app_id = window.app_id().unwrap_or_default();
            let title = window.title().unwrap_or_default();
            entries.push('\t');
            entries.push_str(&id.to_string());
            entries.push('\t');
            entries.push(if focused.as_ref() == Some(window) { '1' } else { '0' });
            entries.push('\t');
            entries.push(if *minimized { '1' } else { '0' });
            entries.push('\t');
            push_sanitized(&mut entries, &app_id);
            entries.push('\t');
            push_sanitized(&mut entries, &title);
            count += 1;
        }
        format!("list\t{count}{entries}")
    }

    /// Focus and raise the window the panel asked for.
    fn focus_panel_window(&mut self, id: u64) {
        let window = self
            .space
            .elements()
            .chain(self.minimized.iter().map(|(window, _)| window))
            .find(|window| window.with_state(|state| state.panel_id) == Some(id))
            .cloned();
        let Some(window) = window else {
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

/// Append `value` with tabs/newlines replaced so it can't break the framing.
fn push_sanitized(out: &mut String, value: &str) {
    for c in value.chars() {
        out.push(match c {
            '\t' | '\n' | '\r' => ' ',
            other => other,
        });
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
