// SPDX-License-Identifier: GPL-2.0-only
#define _POSIX_C_SOURCE 200809L
#include "shell/shell.h"

#include <glib.h>
#include <gtk/gtk.h>
#include <pthread.h>
#include <wayland-server-core.h>

#include "oxide-desktop.h"
#include "settings/settings.h"
#include "shell/panel.h"

/*
 * The shell is created after the compositor is up (server_start has produced a
 * WAYLAND_DISPLAY). GTK connects to that display as a Wayland client of our
 * own compositor — yes, in-process. The gtk4-layer-shell library makes the
 * panel window a wlr_layer_surface_v1, which our compositor's layer-shell
 * implementation binds to the scene graph like any other client's layer
 * surface; it just happens to be us.
 *
 * Startup ordering problem: gtk_init_check() does blocking round-trips to the
 * compositor (registry globals, etc.), but the compositor's loop is not
 * running yet at that point — we are still in shell_init(), before
 * g_main_loop_run(). A single thread cannot both wait for the reply and pump
 * the loop that produces it, so we run a short-lived pump thread during GTK
 * init that drains the wlroots loop. The main thread is fully blocked in
 * gtk_init_check() meanwhile, so the pump thread is the only one touching
 * compositor state — no concurrency. After init the thread is joined and
 * everything runs single-threaded on the GLib main loop.
 */

static GMainLoop *g_loop;
static struct oxide_panel *g_panel;

/* ------------------------------------------------------------------ pump */

static pthread_t g_init_pump_thread;
static gint g_init_pump_running;

static void *
init_pump_main(void *arg)
{
	(void)arg;
	while (g_atomic_int_get(&g_init_pump_running)) {
		wl_event_loop_dispatch(server.wl_event_loop, 50);
		wl_display_flush_clients(server.wl_display);
	}
	return NULL;
}

static bool
start_init_pump(void)
{
	g_atomic_int_set(&g_init_pump_running, 1);
	if (pthread_create(&g_init_pump_thread, NULL, init_pump_main, NULL) != 0) {
		g_atomic_int_set(&g_init_pump_running, 0);
		return false;
	}
	return true;
}

static void
stop_init_pump(void)
{
	g_atomic_int_set(&g_init_pump_running, 0);
	pthread_join(g_init_pump_thread, NULL);
}

/* ------------------------------------------------------------------ init */

void
shell_init(void)
{
	/* The compositor keeps running even if GTK can't start (e.g. no usable
	 * display); the wlroots pump source in loop.c is attached regardless. */
	shell_loop_init();
	g_loop = g_main_loop_new(NULL, FALSE);

	g_set_prgname("oxide-desktop");
	if (!start_init_pump()) {
		g_warning("shell: cannot start init pump thread");
		return;
	}
	/* gtk_init_check() AND panel construction do blocking round-trips to the
	 * compositor (registry globals, layer-shell global, etc.), so the pump
	 * stays alive until every blocking handshake is done. */
	bool gtk_ok = gtk_init_check();
	if (gtk_ok) {
		g_panel = oxide_panel_new();
	}
	stop_init_pump();
	if (!gtk_ok) {
		g_warning("shell: gtk_init failed; running shell-less");
		return;
	}
}

void
shell_finish(void)
{
	if (g_panel) {
		oxide_panel_destroy(g_panel);
		g_panel = NULL;
	}
	shell_loop_finish();
	if (g_loop) {
		g_main_loop_unref(g_loop);
		g_loop = NULL;
	}
}

void
shell_main_loop_run(void)
{
	g_assert(g_loop);
	g_main_loop_run(g_loop);
}

void
shell_main_loop_quit(void)
{
	if (g_loop) {
		g_main_loop_quit(g_loop);
	}
}

void
shell_reconfigure(void)
{
	if (g_panel) {
		oxide_panel_reconfigure(g_panel);
	}
}

void
shell_view_mapped(struct view *view)
{
	if (g_panel) {
		oxide_panel_add_view(g_panel, view);
	}
}

void
shell_view_unmapped(struct view *view)
{
	if (g_panel) {
		oxide_panel_remove_view(g_panel, view);
	}
}
