// SPDX-License-Identifier: GPL-2.0-only
#define _POSIX_C_SOURCE 200809L
#include "shell/shell.h"

#include <glib.h>
#include <gtk/gtk.h>
#include <wayland-server-core.h>

#include "oxide-desktop.h" /* struct server / server.wl_event_loop */

/*
 * Integrate the wlroots event loop into the GLib main loop.
 *
 * wlroots runs an epoll loop (wl_event_loop). GTK4 runs a GLib main context.
 * Both want to own poll(). We resolve this by handing the wlroots epoll fd to
 * GLib as a GSource: whenever GLib sees that fd ready, we call
 * wl_event_loop_dispatch() to drain wlroots work, then flush client replies.
 * Input/output from both stacks is processed in a single thread,
 * cooperatively. No threads, no separate event loop.
 *
 * The server's client sockets are event-loop fd sources, so the dispatch
 * reads + processes client requests (including our own in-process GTK panel,
 * which is just another Wayland client of this compositor) and runs wlroots'
 * callbacks. Together with wl_display_flush_clients() this mirrors what
 * wl_display_run() does, just driven from GLib.
 *
 * Phase 0: the GSource is registered but the compositor still runs
 * wl_display_run(), so the shell is built but not yet pumped. Phase 1 will
 * replace wl_display_run() with g_main_loop_run() and make handle_sigterm()
 * quit the GLib loop (wl_display_terminate() only makes wl_display_run()
 * return; there is no public "is the display exiting" server API to poll).
 */

struct oxide_loop_source {
	GSource source;
	GPollFD poll_fd;
	struct wl_event_loop *loop;
};

static gboolean
oxide_loop_prepare(GSource *source, gint *timeout)
{
	(void)source;
	/*
	 * We have no cheap way to ask wlroots for its next-timer deadline, so
	 * let GLib pick the minimum from its other sources (paint clocks, GTK
	 * idles, etc.) and wake us when the wlroots epoll fd becomes ready.
	 */
	*timeout = -1;
	return FALSE;
}

static gboolean
oxide_loop_check(GSource *source)
{
	struct oxide_loop_source *s = (struct oxide_loop_source *)source;
	return (s->poll_fd.revents & (G_IO_IN | G_IO_HUP | G_IO_ERR)) != 0;
}

static gboolean
oxide_loop_dispatch(GSource *source, GSourceFunc callback, gpointer user_data)
{
	struct oxide_loop_source *s = (struct oxide_loop_source *)source;
	/* Drain wlroots work without blocking, then push buffered replies. */
	wl_event_loop_dispatch(s->loop, 0);
	wl_display_flush_clients(server.wl_display);
	(void)callback;
	(void)user_data;
	return G_SOURCE_CONTINUE;
}

static GSourceFuncs oxide_loop_funcs = {
	.prepare = oxide_loop_prepare,
	.check = oxide_loop_check,
	.dispatch = oxide_loop_dispatch,
	.finalize = NULL,
};

static struct oxide_loop_source *g_loop_source;

void
shell_loop_init(void)
{
	g_assert(!g_loop_source);

	GSource *src = g_source_new(&oxide_loop_funcs,
		sizeof(struct oxide_loop_source));
	struct oxide_loop_source *s = (struct oxide_loop_source *)src;
	s->loop = server.wl_event_loop;
	s->poll_fd.fd = wl_event_loop_get_fd(s->loop);
	s->poll_fd.events = G_IO_IN | G_IO_HUP | G_IO_ERR;
	s->poll_fd.revents = 0;
	g_source_add_poll(src, &s->poll_fd);
	g_source_set_priority(src, G_PRIORITY_DEFAULT);
	g_source_set_can_recurse(src, FALSE);
	g_source_attach(src, g_main_context_default());
	g_loop_source = s;
}

void
shell_loop_finish(void)
{
	if (!g_loop_source) {
		return;
	}
	g_source_destroy(&g_loop_source->source);
	g_source_unref(&g_loop_source->source);
	g_loop_source = NULL;
}
