# Oxide Desktop

Oxide Desktop is a Desktop Environment build in rust with smithay, designed for the upcoming CopperOS linux distro i'm making.

It is a work in progress: a Wayland compositor and its panel, both in this repository.

## What's here

**The compositor.** Built on [smithay](https://github.com/Smithay/smithay), descended
from anvil. It runs on a tty through udev, or nested inside an existing X11 or Wayland
session through winit or X11, and handles both server-side and client-side decorations.

- Server-side decorations: titlebars, borders and interactive edge resizing; CSD clients
  keep their own.
- Edge snapping, snap groups that share a divider grid, and a drag out of maximize that
  keeps the point you grabbed and puts the window back where you picked it up.
- Window animations: a crossfade out of maximize, a fade and centre scale for opening and
  closing.
- Output handling: the best mode a display offers, and a layout that scales with it.
- XWayland.

**The panel.** A GTK4 layer-shell bar, started by the compositor:

- Pinned apps and a task bar, reorderable by dragging a square.
- An app menu at the left of the bar, or on the Super key: every installed application,
  searchable by name, started with a click or with the keyboard. Its foot has the
  user's picture and name on one side, and log out and shut down on the other — both of
  which ask first, and then ask every app to close itself.
- Live window previews, and a per-app menu with pin, quit and launch.
- The bar slides in, squares trade places over 250 ms, and the wallpaper fades up behind
  everything.
- Pins and bar order survive a restart.

## Building

Rust 1.85 or newer (the workspace is edition 2024), a C toolchain, `pkg-config`, GTK4 and
`gtk4-layer-shell`, and the usual EGL/Mesa development packages.

```sh
cargo build --release                    # nested, for development
cargo build --release --features udev    # tty: DRM/KMS, libinput, libseat
cargo build --release --features x11     # X11 client
```

Dependencies are built with `opt-level = 3` even in debug builds (see `Cargo.toml`):
unoptimised image and font crates are slow enough that a debug build is not worth
debugging. The first build therefore takes a while.

## Running

From an X11 or Wayland session:

```sh
cargo run --release                 # auto-detect: udev on a bare tty (if built in), else x11 or winit
cargo run --release -- --winit      # explicitly nested
```

On a tty, with the udev feature built:

```sh
cargo run --release --features udev -- --tty-udev
```

`--tty-udev` needs the seat permissions logind normally hands out; running as root
without logind also works. The panel is a child process of the compositor
(`oxide-desktop --panel`), and `--panel` on its own is how it is developed.

## Configuration

The panel remembers pins and bar order in `~/.config/oxide-desktop/panel.conf`. A missing
or malformed file means no pins, never a bar that refuses to start.

The app menu's foot shows the user's picture if there is one: `~/.config/oxide-desktop/face.png`
(or `.jpg`, `.jpeg`, `.webp`), `~/.face`, or the copy AccountsService keeps. Without one it
shows Material's account mark.

The wallpaper is the first of these that exists, unless `$OXIDE_WALLPAPER` names a file
outright:

```
$XDG_CONFIG_HOME/oxide/wallpaper.{png,jpg,jpeg,webp}
$XDG_CONFIG_HOME/oxide/background.{png,jpg,jpeg,webp}
$XDG_DATA_HOME/oxide/wallpaper.{png,jpg,jpeg,webp}
$XDG_DATA_HOME/oxide/background.{png,jpg,jpeg,webp}
```

## Development

```sh
cargo test                    # 165 tests, no display required
cargo test --features udev
cargo test --features x11
```

## Status

Early, and moving. It is not finished: expect things to change, and expect some of them
to change back.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
