# Working on this repository

## The panel needs a real display to test

`src/panel.rs` is a GTK4 widget tree. The unit tests in it are arithmetic and cannot see
a widget; the bugs that actually reached a running session were all invisible to them.

There is a widget-level suite that drives a real GTK:

    xvfb-run -a cargo test --test panel_widgets

Run it after any change to the panel's widgets. It asserts, against real GTK:

* nothing is left sitting over the bar's squares between drags
* a drag gives the carried square back, with no placeholder left behind
* reordering does not lose or duplicate a square
* **GTK emits no complaints at all** — the last of these is what caught a
  `gtk_fixed_put` on an already-parented widget and a negative margin on a 36px square,
  both of which had shipped to a running session

The GTK-warning assertion is the important one. Reintroduce either past bug and the suite
fails with the exact message the compositor printed.

Notes on the harness:

* Xvfb has no window manager, so GTK never maps anything. `tests/panel_widgets.rs` drives
  layout with `Widget::allocate` directly. Exact pixel positions are therefore not
  meaningful here and are deliberately not asserted — parentage, ordering and GTK's own
  complaints are.
* `Widget::put` on `GtkFixed` asserts the widget has no parent and does *nothing* if it
  has one. Use `move_` to reposition.
* `GtkBox` has no `insert`. Use `insert_child_after`, and unparent first.
* Never move a widget with a size request by giving it a negative margin: GTK rejects it.

## Tests must never write the user's real config

`cargo test` used to overwrite `~/.config/oxide-desktop/panel.conf` with test fixtures, on
every run, silently — for as long as the pinning feature existed. `panel_conf::path()`
honours a thread-local override that exists only in test builds, and the panel's
`with_empty_layout` installs one. Keep it that way: anything that saves the layout in a
test must go through it.
