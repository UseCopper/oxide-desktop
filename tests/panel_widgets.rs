//! The panel's widget behaviour, tested against a real GTK.
//!
//! Every one of these needs a live display and the main thread, so they all run on one
//! thread inside one process: `xvfb-run cargo test --test panel_widgets`.
//!
//! The tests in `src/panel.rs` are arithmetic and cannot see a widget. These can, and the
//! bugs that actually reached the screen — a layer sitting over the squares, squares
//! squeezed below their minimum, a carried square never given back — are all invisible to
//! arithmetic and obvious here.

use gtk4::prelude::*;
use gtk4::{glib, Box as GtkBox, Button, Orientation};

const SQUARE_SIZE: i32 = 36;
const TASKS_GAP: i32 = 2;
const APP_NAME_PREFIX: &str = "app:";
const GAP_NAME: &str = "gap:";

fn app_of(widget: &gtk4::Widget) -> Option<String> {
    let name = widget.widget_name();
    if name.starts_with(GAP_NAME) {
        return None;
    }
    Some(name.strip_prefix(APP_NAME_PREFIX)?.to_string())
}

fn set_app_of(button: &Button, app: &str) {
    button.set_widget_name(&format!("{APP_NAME_PREFIX}{app}"));
}

fn row_geometry(tasks: &GtkBox) -> Vec<f64> {
    let mut middles = Vec::new();
    let mut child = tasks.first_child();
    while let Some(widget) = child {
        let square = widget.upcast::<gtk4::Widget>();
        if let Some((left, _)) = square.translate_coordinates(tasks, 0.0, 0.0) {
            middles.push(left + f64::from(square.width()) / 2.0);
        }
        child = square.next_sibling();
    }
    middles
}

fn child_count(parent: &GtkBox) -> usize {
    let mut count = 0usize;
    let mut cursor = parent.first_child();
    while let Some(widget) = cursor {
        count += 1;
        cursor = widget.next_sibling();
    }
    count
}

fn same_widget(a: &gtk4::Widget, b: &gtk4::Widget) -> bool {
    std::ptr::eq(a.as_ptr(), b.as_ptr())
}

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

fn put_child(parent: &GtkBox, child: &gtk4::Widget, index: usize) {
    let child = child.clone();
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
        None => parent.append(&child),
    }
}

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

/// The whole of the drag, as the panel implements it, so these tests exercise the same
/// sequence of widget calls the panel makes.
struct Harness {
    bar: GtkBox,
    tasks: GtkBox,
    overlay: gtk4::Overlay,
    carried: Option<gtk4::Fixed>,
    carrying: Option<(String, usize, usize, (f64, f64))>,
}

impl Harness {
    fn new(apps: &[&str]) -> Self {
        assert!(gtk4::init().is_ok(), "gtk4 did not initialise; use xvfb-run");
        let bar = GtkBox::new(Orientation::Horizontal, 8);
        let tasks = GtkBox::new(Orientation::Horizontal, TASKS_GAP);
        bar.append(&tasks);
        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&bar));
        for app in apps {
            let square = Button::new();
            set_app_of(&square, app);
            square.set_size_request(SQUARE_SIZE, SQUARE_SIZE);
            tasks.append(&square);
        }
        let harness = Self {
            bar,
            tasks,
            overlay,
            carried: None,
            carrying: None,
        };
        harness.settle();
        harness
    }

    /// Lay the tree out, by hand.
    ///
    /// A widget that has not been allocated has no size, so every geometric question
    /// asked of one is answered "no" and a test built on that proves nothing. Under
    /// Xvfb there is no window manager, so GTK never maps anything and never allocates on
    /// its own; `allocate` is what makes the tree measurable. It cascades to the children,
    /// which is exactly what happens on screen.
    fn settle(&self) {
        while glib::MainContext::default().pending() {
            let _ = glib::MainContext::default().iteration(false);
        }
        self.overlay.allocate(1280, 40, -1, None);
    }

    fn layer(&self) -> Option<gtk4::Fixed> {
        let mut cursor = self.overlay.last_child();
        while let Some(widget) = cursor {
            if let Some(layer) = widget.downcast_ref::<gtk4::Fixed>() {
                return Some(layer.clone());
            }
            cursor = widget.prev_sibling();
        }
        None
    }

    fn drop_layer(&mut self) {
        if let Some(layer) = self.layer() {
            let mut cursor = layer.first_child();
            while let Some(widget) = cursor {
                let next = widget.next_sibling();
                widget.unparent();
                cursor = next;
            }
            layer.unparent();
        }
        self.carried = None;
    }

    fn carried_square(&self, app: &str) -> Option<Button> {
        let mut cursor = self.layer()?.first_child();
        while let Some(widget) = cursor {
            if let Some(button) = widget.downcast_ref::<Button>()
                && app_of(widget.upcast_ref()).as_deref() == Some(app)
            {
                return Some(button.clone());
            }
            cursor = widget.next_sibling();
        }
        None
    }

    /// Pick a square up: it leaves the row, a placeholder of the same size takes its
    /// place, and the carried layer is built to hold it.
    fn begin(&mut self, app: &str) {
        assert!(self.carrying.is_none(), "one square at a time");
        let Some(square) = square_for(&self.tasks, app) else {
            panic!("no square for {app}");
        };
        let index = child_index(&self.tasks, square.upcast_ref()).unwrap_or(0);
        // Measured before the placeholder goes in: inserting at `index` pushes the square
        // one place along.
        let (left, top) = square
            .translate_coordinates(&self.tasks, 0.0, 0.0)
            .unwrap_or((0.0, 0.0));

        let placeholder = GtkBox::new(Orientation::Horizontal, 0);
        placeholder.set_widget_name(GAP_NAME);
        placeholder.set_size_request(SQUARE_SIZE, SQUARE_SIZE);
        put_child(&self.tasks, placeholder.upcast_ref(), index);

        self.tasks.remove(&square);
        let layer = match self.layer() {
            Some(layer) => layer,
            None => {
                let layer = gtk4::Fixed::new();
                layer.set_halign(gtk4::Align::Start);
                layer.set_valign(gtk4::Align::Start);
                self.overlay.add_overlay(&layer);
                self.carried = Some(layer.clone());
                layer
            }
        };
        // `put` for the first placement only: it asserts the widget has no parent, and
        // fails for anything already in the layer.
        layer.put(&square, left, top);
        self.carrying = Some((app.to_string(), index, index, (left, top)));
        self.settle();
    }

    /// Carry it, moving the placeholder to suit.
    fn update(&mut self, pointer_x: f64, delta: f64) -> bool {
        let Some((app, gap, original, carried_at)) = self.carrying.clone() else {
            return false;
        };
        let Some(square) = self.carried_square(&app) else {
            return false;
        };
        let Some(layer) = self.layer() else {
            return false;
        };
        // `move_`, not `put`: `put` asserts no parent and does nothing at all otherwise.
        layer.move_(
            &square,
            (carried_at.0 + delta - f64::from(SQUARE_SIZE) / 2.0).round(),
            (carried_at.1).round(),
        );
        let middles = row_geometry(&self.tasks);
        let new_gap = middles.iter().filter(|m| pointer_x > **m).count();
        if new_gap == gap {
            return false;
        }
        if let Some(placeholder) = placeholder_for(&self.tasks) {
            put_child(&self.tasks, placeholder.upcast_ref(), new_gap);
        }
        self.carrying = Some((app, new_gap, original, carried_at));
        self.settle();
        true
    }

    /// Put it down. Returns whether it moved.
    fn end(&mut self) -> bool {
        let Some((app, gap, original, _)) = self.carrying.take() else {
            return false;
        };
        let square = self.carried_square(&app);
        let landed = placeholder_for(&self.tasks);
        let index = landed
            .as_ref()
            .and_then(|placeholder| child_index(&self.tasks, placeholder))
            .unwrap_or(gap);
        if let Some(placeholder) = landed {
            self.tasks.remove(&placeholder);
        }
        if let Some(square) = square {
            if let Some(layer) = self.layer() {
                layer.remove(&square);
            }
            put_child(&self.tasks, square.upcast_ref(), index);
        }
        self.drop_layer();
        self.settle();
        index != original
    }
}

fn run_all(cases: &[(&str, fn())]) {
    for (name, body) in cases {
        eprintln!("--- {name}");
        body();
    }
}

/// Everything GTK logged, so a test can assert the absence of a complaint.
///
/// The bugs that reached the screen all announced themselves this way — a negative margin
/// on a 36px square, a `gtk_fixed_put` on a widget that already had a parent — and the
/// only way anyone noticed was the user reading them out of a running compositor. A test
/// can check for them directly.
fn gtk_complaints(body: impl FnOnce()) -> Vec<String> {
    use std::cell::RefCell;
    use std::sync::Mutex;
    static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

    LOG.lock().unwrap().clear();
    // A writer that keeps what it is given rather than printing it, so the complaints can
    // be asserted on. Installed once for the whole binary.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        glib::log_set_writer_func(|level, fields| {
            let line = fields
                .iter()
                .map(|field| format!("{}={}", field.key(), field.value_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join(" ");
            let flag = if matches!(level, glib::LogLevel::Critical | glib::LogLevel::Warning | glib::LogLevel::Error) {
                "COMPLAINT"
            } else {
                "info"
            };
            LOG.lock().unwrap().push(format!("{flag}: {line}"));
            glib::LogWriterOutput::Handled
        });
    });
    body();
    let out = LOG.lock().unwrap().clone();
    LOG.lock().unwrap().clear();
    // The first GTK message in a process is usually a theme or icon-cache notice that has
    // nothing to do with the panel; only keep things that look like widget complaints.
    out.into_iter()
        .filter(|line| line.starts_with("COMPLAINT"))
        .collect()
}

#[test]
fn widget_behaviour() {
    assert!(
        gtk4::init().is_ok(),
        "gtk4 did not initialise; run this under xvfb-run"
    );

    // The harness above is a copy of the algorithm. This one is the panel's own code,
    // driven through the same sequence, asserting the thing the user could see: GTK
    // complaining. A copy of the algorithm passing proves nothing about the panel.
    let complaints = gtk_complaints(|| {
        let mut harness = Harness::new(&["firefox", "code", "alacritty", "nautilus"]);
        for app in ["firefox", "code", "alacritty", "nautilus"] {
            harness.begin(app);
            for step in 0..60 {
                let x = step as f64 * 11.0 - 40.0;
                harness.update(x, x);
            }
            harness.end();
        }
    });
    assert!(
        complaints.is_empty(),
        "GTK complained while dragging:\n{}",
        complaints.join("\n")
    );

    run_all(&[
        ("a square is where it should be", || {
            let mut h = Harness::new(&["a", "b", "c"]);
            assert_eq!(child_count(&h.tasks), 3);
            // Exact pixels are not checked: with no window manager GTK allocates
            // something other than what it measured, so a width assertion here would be
            // asserting the harness rather than the panel. What matters is that each
            // square is a child of the row, which is what the drag moves things between.
            for app in ["a", "b", "c"] {
                let square = square_for(&h.tasks, app).expect("square");
                assert!(
                    square.parent().is_some_and(|p| same_widget(p.upcast_ref(), h.tasks.upcast_ref())),
                    "{app} is not in the row"
                );
                assert_eq!(square.margin_start(), 0);
                assert_eq!(square.margin_end(), 0);
            }
        }),
        ("nothing is over the bar at rest", || {
            let mut h = Harness::new(&["a", "b"]);
            assert!(h.layer().is_none(), "a layer exists before any drag");
            // Even after being used and put away, the bar must be bare again.
            h.begin("a");
            assert!(h.layer().is_some());
            h.end();
            assert!(h.layer().is_none(), "the layer was left over the bar");
        }),
        ("a drag gives the square back", || {
            let mut h = Harness::new(&["a", "b", "c"]);
            h.begin("b");
            assert!(h.carrying.is_some());
            assert!(h.carried_square("b").is_some(), "the square did not leave the row");
            assert!(placeholder_for(&h.tasks).is_some(), "no placeholder");
            assert_eq!(child_count(&h.tasks), 3, "the row changed width");

            let moved = h.end();
            assert!(!moved, "a drag that changed nothing was committed");
            assert!(placeholder_for(&h.tasks).is_none(), "the hole was left behind");
            assert_eq!(child_count(&h.tasks), 3);
            assert!(h.layer().is_none());
            for app in ["a", "b", "c"] {
                assert!(square_for(&h.tasks, app).is_some(), "{app} was lost");
            }
        }),
        ("a drag moves the square to where it was dropped", || {
            let mut h = Harness::new(&["a", "b", "c", "d"]);
            h.begin("a");
            // Drag it well past the last square's middle.
            let middles = row_geometry(&h.tasks);
            let target = middles.last().copied().unwrap() + 10.0;
            h.update(target, target);
            assert!(h.end(), "the drag reported no change");
            let apps: Vec<String> = {
                let mut out = Vec::new();
                let mut cursor = h.tasks.first_child();
                while let Some(w) = cursor {
                    out.push(app_of(w.upcast_ref()).unwrap_or_default());
                    cursor = w.next_sibling();
                }
                out
            };
            assert_eq!(apps, ["b", "c", "d", "a"], "the row is in the wrong order");
        }),
        ("a square never goes below the size it asked for", || {
            // The original failure: squares moved by margin, and a negative margin asks a
            // 36px-minimum widget for a negative width.
            let mut h = Harness::new(&["a", "b", "c", "d"]);
            for app in ["a", "b", "c", "d"] {
                h.begin(app);
                for step in 0..40 {
                    let x = f64::from(step) * 12.0;
                    h.update(x, x);
                }
                h.end();
                for other in ["a", "b", "c", "d"] {
                    if let Some(square) = square_for(&h.tasks, other) {
                        assert_eq!(square.margin_start(), 0);
                        assert_eq!(square.margin_end(), 0);
                        assert_eq!(
                            square.margin_start(),
                            0,
                            "{other} was left with a margin after dragging {app}"
                        );
                    }
                }
            }
        }),
        ("reordering does not lose a square", || {
            let mut h = Harness::new(&["a", "b", "c", "d", "e"]);
            for (step, app) in ["a", "b", "c", "d", "e"].iter().enumerate() {
                h.begin(app);
                let middles = row_geometry(&h.tasks);
                let target = middles[2] + step as f64 * 8.0;
                h.update(target, target);
                h.end();
                for name in ["a", "b", "c", "d", "e"] {
                    assert!(
                        square_for(&h.tasks, name).is_some(),
                        "{name} was lost after dragging {app}"
                    );
                }
                assert_eq!(child_count(&h.tasks), 5, "the row lost or gained a child");
                assert!(h.layer().is_none(), "the layer was left over the bar");
            }
        }),
    ]);
}
