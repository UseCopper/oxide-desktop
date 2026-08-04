/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_SHELL_H
#define OXIDE_SHELL_H

/*
 * The Oxide Desktop shell: the in-process panel / tray / thumbnailer built
 * with GTK4 + gtk4-layer-shell. Only compiled in when the build found
 * gtk4-layer-shell (HAVE_SHELL).
 *
 * Lifecycle: shell_init() is called from main() AFTER server_start() so the
 * Wayland display exists and GTK can grab it. shell_finish() tears down GTK
 * before the server finishes.
 */

/*
 * Bring the GTK main loop and the wlroots event loop into one cooperative
 * dispatch loop. Must be called before any GTK widget is realized.
 */
void shell_loop_init(void);
void shell_loop_finish(void);

/*
 * Initialize the whole shell subsystem (loop + panel).
 */
void shell_init(void);
void shell_finish(void);

/*
 * Run the compositor's main loop. With the shell built this is the GLib main
 * loop (the wlroots loop is pumped by a GSource); without it we fall back to
 * wl_display_run(). shell_main_loop_quit() makes it return (called from the
 * SIGTERM/SIGINT path, which still calls wl_display_terminate() too).
 */
void shell_main_loop_run(void);
void shell_main_loop_quit(void);

/*
 * Re-apply settings that affect the shell (panel height/position/output).
 * Called from the SIGHUP reload path after settings_reload().
 */
void shell_reconfigure(void);

#endif /* OXIDE_SHELL_H */
