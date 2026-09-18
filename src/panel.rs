//! The desktop panel: a GTK4 client that uses the `wlr-layer-shell` protocol to
//! pin itself to the top of an output.
//!
//! The compositor starts this as a child of itself (`oxide-desktop --panel`),
//! pointed at its own Wayland socket.

use gtk4::{gdk, glib, prelude::*};
use gtk4::{Application, ApplicationWindow, Box as GtkBox, Label, Orientation};
use gtk4_layer_shell::{Edge, Layer, LayerShell};

const PANEL_HEIGHT: i32 = 30;

const STYLE: &str = "
window.panel-window {
    background-color: rgba(20, 22, 28, 0.92);
    border: none;
    box-shadow: none;
}
.panel {
    background-color: rgba(20, 22, 28, 0.92);
    padding: 0 10px;
}
.panel label {
    color: #e8e8ea;
    font-weight: 500;
}
.panel .title {
    color: #8ab4f8;
    font-weight: 700;
}
";

pub fn run_panel() {
    let app = Application::builder()
        .application_id("dev.oxide.Panel")
        .build();

    app.connect_activate(|app| {
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

        let clock = Label::new(None);
        clock.set_hexpand(true);
        clock.set_halign(gtk4::Align::End);

        row.append(&title);
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
        window.present();
    });

    // Ignore the `--panel` argument passed by the compositor.
    app.run_with_args::<&str>(&[]);
}

fn install_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(STYLE);
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_USER,
    );
}

fn update_clock(label: &Label) {
    let text = glib::DateTime::now_local()
        .and_then(|now| now.format("%a %H:%M:%S"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    label.set_text(&text);
}
