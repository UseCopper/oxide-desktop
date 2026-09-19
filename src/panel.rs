//! The desktop panel: a GTK4 client that uses the `wlr-layer-shell` protocol to
//! pin itself to the top of an output.
//!
//! The compositor starts this as a child of itself (`oxide-desktop --panel`),
//! pointed at its own Wayland socket. The list of open windows and focus
//! requests travel over a Unix socket (see [`crate::panel_ipc`]).

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
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
/// Fallback accent (neutral gray) when the desktop provides none.
const DEFAULT_ACCENT: &str = "#d8d8d8";
const MUTED_COLOR: (f64, f64, f64) = (0.78, 0.78, 0.78);
/// How often the panel drains the compositor socket.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const FALLBACK_ICON: &str = "application-x-executable";

thread_local! {
    /// The current accent, shared by the CSS and the cairo indicator dots.
    static ACCENT: Cell<(f64, f64, f64)> = const { Cell::new((0.847, 0.847, 0.847)) };
    static STYLE_PROVIDER: RefCell<Option<gtk4::CssProvider>> = const { RefCell::new(None) };
}

fn accent_rgb() -> (f64, f64, f64) {
    ACCENT.with(Cell::get)
}

fn style_sheet((r, g, b): (u8, u8, u8)) -> String {
    format!(
        "
window.panel-window {{
    background-color: rgba(32, 32, 32, 0.82);
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
button.task.focused {{
    background-color: rgba({r}, {g}, {b}, 0.28);
}}
button.task.minimized {{
    opacity: 0.45;
}}
"
    )
}

/// One open window as reported by the compositor.
struct WindowInfo {
    id: u64,
    focused: bool,
    minimized: bool,
    app_id: String,
    title: String,
}

pub fn run_panel() {
    let app = Application::builder()
        .application_id("dev.oxide.Panel")
        .build();

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

    let window = ApplicationWindow::builder().application(app).build();
    window.set_decorated(false);
    window.add_css_class("panel-window");
    window.init_layer_shell();
    window.set_namespace(Some("oxide-panel"));
    window.set_layer(Layer::Top);
    // Put it on the main (first) display.
    if let Some(display) = gdk::Display::default()
        && let Some(monitor) = display
            .monitors()
            .item(0)
            .and_then(|obj| obj.downcast::<gdk::Monitor>().ok())
    {
        window.set_monitor(Some(&monitor));
    }
    for edge in [Edge::Top, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    // Reserve space so maximized windows stop below the panel.
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

    if let Some(stream) = stream {
        send_accent(&stream);
        start_polling(&tasks, stream);
    }

    window.set_child(Some(&row));
    window.present();
}

/// Tell the compositor the accent so it can tint the snap preview.
fn send_accent(stream: &Rc<UnixStream>) {
    let (red, green, blue) = accent_rgb();
    let message = format!("accent\t{red:.6}\t{green:.6}\t{blue:.6}\n");
    let _ = (&**stream).write_all(message.as_bytes());
}

/// Drain the compositor socket and rebuild the task list on every snapshot.
fn start_polling(tasks: &GtkBox, stream: Rc<UnixStream>) {
    let tasks = tasks.clone();
    let pending = RefCell::new(Vec::<u8>::new());
    glib::timeout_add_local(POLL_INTERVAL, move || {
        let mut buf = [0u8; 4096];
        loop {
            match (&*stream).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => pending.borrow_mut().extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        loop {
            let Some(newline) = pending.borrow().iter().position(|&b| b == b'\n') else {
                break;
            };
            let line = {
                let mut pending = pending.borrow_mut();
                let line = String::from_utf8_lossy(&pending[..newline]).to_string();
                pending.drain(..=newline);
                line
            };
            if let Some(windows) = parse_snapshot(&line) {
                rebuild_tasks(&tasks, &windows, &stream);
            }
        }

        glib::ControlFlow::Continue
    });
}

fn rebuild_tasks(tasks: &GtkBox, windows: &[WindowInfo], stream: &Rc<UnixStream>) {
    while let Some(child) = tasks.first_child() {
        tasks.remove(&child);
    }

    // Group the windows by app, keeping first-seen order. Windows with no app
    // id get a key of their own so they don't all collapse together.
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

    for (key, group) in groups {
        let app_id = if key.starts_with('#') { "" } else { key.as_str() };
        tasks.append(&app_button(app_id, &group, stream));
    }
}

/// One square (1:1) per app, with indicator dots for its window count.
fn app_button(app_id: &str, windows: &[&WindowInfo], stream: &Rc<UnixStream>) -> Button {
    let focused = windows.iter().any(|window| window.focused);
    let minimized = windows.iter().all(|window| window.minimized);
    let count = windows.len();

    let button = Button::new();
    button.add_css_class("task");
    if focused {
        button.add_css_class("focused");
    }
    if minimized {
        button.add_css_class("minimized");
    }
    // Without this the HBox stretches the button to the panel height.
    button.set_valign(gtk4::Align::Center);
    button.set_halign(gtk4::Align::Center);
    button.set_size_request(SQUARE_SIZE, SQUARE_SIZE);

    let tooltip = match count {
        1 => window_tooltip(windows[0]),
        _ => format!("{count} windows"),
    };
    button.set_tooltip_text(Some(&tooltip));

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

    // Repeated clicks cycle through the app's windows.
    let target = next_window_id(windows);
    let stream = stream.clone();
    button.connect_clicked(move |_| {
        let message = format!("focus\t{target}\n");
        let _ = (&*stream).write_all(message.as_bytes());
    });

    button
}

/// The window a click on an app square should focus: the next one after the
/// focused window, or the first if none of them is focused.
fn next_window_id(windows: &[&WindowInfo]) -> u64 {
    match windows.iter().position(|window| window.focused) {
        Some(index) => windows[(index + 1) % windows.len()].id,
        None => windows[0].id,
    }
}

fn window_tooltip(info: &WindowInfo) -> String {
    let title = if info.title.is_empty() {
        info.app_id.clone()
    } else {
        info.title.clone()
    };
    if info.minimized {
        format!("{title} (minimized)")
    } else {
        title
    }
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
    if let Some(icon) = desktop_icon(app_id) {
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

thread_local! {
    /// Desktop-file id -> `Icon=` value, loaded once from the XDG data dirs.
    static DESKTOP_ICONS: RefCell<Option<HashMap<String, String>>> = const { RefCell::new(None) };
}

fn desktop_icon(app_id: &str) -> Option<String> {
    DESKTOP_ICONS.with(|cell| {
        let mut cache = cell.borrow_mut();
        let icons = cache.get_or_insert_with(load_desktop_icons);
        icons
            .get(app_id)
            .or_else(|| icons.get(&app_id.to_lowercase()))
            .cloned()
    })
}

/// Build the desktop-file id -> icon map from every XDG applications dir.
/// Earlier dirs win, matching the freedesktop lookup order.
fn load_desktop_icons() -> HashMap<String, String> {
    let mut icons = HashMap::new();
    for dir in application_dirs() {
        collect_desktop_icons(&dir, &dir, &mut icons);
    }
    icons
}

fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':').filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(dir).join("applications"));
    }
    dirs
}

fn collect_desktop_icons(root: &Path, dir: &Path, icons: &mut HashMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_desktop_icons(root, &path, icons);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let id = relative.to_string_lossy().replace('/', "-");
        let id = id.strip_suffix(".desktop").unwrap_or(&id).to_string();
        if icons.contains_key(&id) {
            continue;
        }
        if let Some(icon) = desktop_file_icon(&path) {
            icons.entry(id.clone()).or_insert_with(|| icon.clone());
            icons.entry(id.to_lowercase()).or_insert(icon);
        }
    }
}

/// Read the `Icon=` key from a desktop file's `[Desktop Entry]` group.
fn desktop_file_icon(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        // Ignore localized keys like `Icon[de]=...`.
        if let Some(value) = line.strip_prefix("Icon=") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
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
    fn ignores_other_messages() {
        assert!(parse_snapshot("focus\t3").is_none());
    }

    #[test]
    fn cycles_through_an_apps_windows() {
        let make = |id, focused| WindowInfo {
            id,
            focused,
            minimized: false,
            app_id: "alacritty".to_string(),
            title: String::new(),
        };
        let windows = [make(1, false), make(2, true), make(3, false)];
        let refs: Vec<&WindowInfo> = windows.iter().collect();
        assert_eq!(next_window_id(&refs), 3, "next after the focused window");

        let windows = [make(4, false), make(5, false)];
        let refs: Vec<&WindowInfo> = windows.iter().collect();
        assert_eq!(next_window_id(&refs), 4, "first when none is focused");
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

    #[test]
    fn reads_icon_from_desktop_entry() {
        let path = std::env::temp_dir().join(format!("oxide-panel-{}.desktop", std::process::id()));
        std::fs::write(
            &path,
            "[Desktop Entry]\nName=Foo\nIcon[de]=lokal\nIcon=foo-icon\n",
        )
        .unwrap();
        assert_eq!(desktop_file_icon(&path).as_deref(), Some("foo-icon"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn maps_nested_desktop_files_to_ids() {
        let dir = std::env::temp_dir().join(format!("oxide-panel-dirs-{}", std::process::id()));
        let nested = dir.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("foo.desktop"), "[Desktop Entry]\nIcon=foo\n").unwrap();

        let mut icons = HashMap::new();
        collect_desktop_icons(&dir, &dir, &mut icons);
        assert_eq!(icons.get("sub-foo").map(String::as_str), Some("foo"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
