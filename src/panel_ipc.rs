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
    },
    /// The desktop accent color, as linear-ish sRGB components.
    Accent([f32; 3]),
}

/// A window preview on its way to the panel: a header line naming the window
/// and the pixel size, followed by exactly `len` bytes of image data.
///
/// The payload is length-prefixed rather than newline-terminated because raw
/// pixels contain newlines, which would otherwise truncate the message.
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
                    messages.push(PanelMessage::Preview {
                        id,
                        max_width: w,
                        max_height: h,
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
                } => {
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
        for _ in 0..panel_thumbnails_per_tick() {
            let Some(id) = self.panel_preview_queue.pop_front() else {
                break;
            };
            self.panel_preview_queued.remove(&id);
            // No size means the window was dropped while queued; the panel asks again
            // on its next tick, so there is nothing to do but move on.
            let Some((max_width, max_height)) = self.panel_preview_sizes.remove(&id) else {
                continue;
            };
            let Some(image) = self.render_panel_thumbnail(id, max_width, max_height) else {
                continue;
            };
            if let Some(ipc) = self.panel_ipc.as_mut() {
                ipc.send_image(image);
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
        // Skip a window whose contents have not moved on: the panel is already
        // showing the right pixels, and rendering a preview means reading the
        // window's full content back off the GPU. This is what lets the picker
        // poll fast enough to look live.
        let generation = window.decoration_state().content_generation;
        if window.decoration_state().previewed_generation == Some(generation) {
            return None;
        }
        let max = smithay::utils::Size::<i32, smithay::utils::Buffer>::from((max_width, max_height));
        let Some((size, pixels)) = self.backend_data.panel_thumbnail(&window, max) else {
            return None;
        };
        window.decoration_state().previewed_generation = Some(generation);
        Some(PanelImage {
            id,
            width: size.w,
            height: size.h,
            pixels,
        })
    }

    /// The window with this panel id, if it is still open (minimized windows
    /// included, since the picker lists those too).
    fn panel_window(&self, id: u64) -> Option<crate::shell::WindowElement> {
        self.space
            .elements()
            .chain(self.minimized.iter().map(|(window, _)| window))
            .find(|window| {
                !window.is_ghosting()
                    && window.decoration_state().panel_id == Some(id)
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
    /// ones (which are unmapped from `Space`). Each window gets its own entry,
    /// so several instances of the same app stay distinct.
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
            let id = if let Some(id) = window.decoration_state().panel_id {
                id
            } else {
                let id = self.next_panel_id;
                self.next_panel_id += 1;
                window.decoration_state().panel_id = Some(id);
                id
            };

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
            .find(|window| window.decoration_state().panel_id == Some(id))
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
