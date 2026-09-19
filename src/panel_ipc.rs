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
    /// The desktop accent color, as linear-ish sRGB components.
    Accent([f32; 3]),
}

/// The compositor's end of the panel connection.
#[derive(Debug)]
pub struct PanelIpc {
    listener: UnixListener,
    conn: Option<UnixStream>,
    read_buf: Vec<u8>,
    last_snapshot: String,
}

impl PanelIpc {
    pub fn new(listener: UnixListener) -> Self {
        Self {
            listener,
            conn: None,
            read_buf: Vec::new(),
            last_snapshot: String::new(),
        }
    }

    /// Accept a pending connection and return the focus requests received since
    /// the last call.
    pub fn poll(&mut self) -> Vec<PanelMessage> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(true);
                self.conn = Some(stream);
                // Force a fresh snapshot for the new connection.
                self.last_snapshot.clear();
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

    /// Send a window-list snapshot if it differs from the last one sent.
    pub fn send(&mut self, snapshot: &str) {
        if snapshot == self.last_snapshot {
            return;
        }
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let mut line = String::with_capacity(snapshot.len() + 1);
        line.push_str(snapshot);
        line.push('\n');
        match conn.write_all(line.as_bytes()) {
            Ok(()) => {
                self.last_snapshot.clear();
                self.last_snapshot.push_str(snapshot);
            }
            Err(err) => {
                debug!(%err, "panel: write failed");
                self.conn = None;
            }
        }
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
        for message in messages {
            match message {
                PanelMessage::Focus(id) => self.focus_panel_window(id),
                PanelMessage::Accent(rgb) => crate::shell::set_preview_color(rgb),
            }
        }
        let snapshot = self.panel_snapshot();
        if let Some(ipc) = self.panel_ipc.as_mut() {
            ipc.send(&snapshot);
        }
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
