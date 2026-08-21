// SPDX-License-Identifier: GPL-2.0-only
/*
 * oxide-panel - out-of-process GTK4 taskbar for oxide-desktop.
 *
 * This is a normal Wayland client of the compositor (just like Firefox),
 * so it can never deadlock the compositor the way an in-process GTK panel
 * could. It uses gtk4-layer-shell for the panel surface and the
 * wlr-foreign-toplevel-management protocol to list/activate windows.
 */
#define _POSIX_C_SOURCE 200809L
#include <gtk/gtk.h>
#include <gtk4-layer-shell.h>
#include <gdk/wayland/gdkwayland.h>
#include <wayland-client.h>

#include <stdlib.h>
#include <string.h>

#include "wlr-foreign-toplevel-management-unstable-v1-client-protocol.h"

struct toplevel {
	struct zwlr_foreign_toplevel_handle_v1 *handle;
	char *title;
	char *app_id;
	gboolean activated;
	GtkWidget *button;
};

static struct wl_display *wl_display;
static struct zwlr_foreign_toplevel_manager_v1 *manager;
static GSList *toplevels;          /* list of struct toplevel * */
static GtkWidget *button_box;
static int panel_height = 36;
static gboolean panel_on_bottom = FALSE;
static guint rebuild_idle_id;

/* ----------------------------------------------------------------- settings */

static void
load_settings(void)
{
	const char *cfg = getenv("XDG_CONFIG_HOME");
	char *base = cfg && *cfg ? g_strdup(cfg)
		: g_strdup_printf("%s/.config", getenv("HOME"));
	char *path = g_strdup_printf("%s/oxide-desktop/settings.conf", base);

	GKeyFile *kf = g_key_file_new();
	if (g_key_file_load_from_file(kf, path, G_KEY_FILE_NONE, NULL)) {
		if (g_key_file_has_key(kf, "Shell", "panel-height", NULL)) {
			panel_height = g_key_file_get_integer(kf,
				"Shell", "panel-height", NULL);
		}
		if (panel_height < 1) {
			panel_height = 1;
		}
		if (g_key_file_has_key(kf, "Shell", "panel-position", NULL)) {
			char *pos = g_key_file_get_string(kf,
				"Shell", "panel-position", NULL);
			panel_on_bottom = pos && g_strcmp0(pos, "bottom") == 0;
			g_free(pos);
		}
	}
	g_key_file_free(kf);
	g_free(path);
	g_free(base);
}

/* ----------------------------------------------------------------- ui */

static void
activate_toplevel(struct toplevel *t)
{
	if (!t || !t->handle) {
		return;
	}
	GdkDisplay *display = gdk_display_get_default();
	GdkSeat *seat = gdk_display_get_default_seat(display);
	struct wl_seat *wl_seat =
		gdk_wayland_seat_get_wl_seat(seat);
	/* labwc focuses the toplevel on activate; serial is unused. */
	zwlr_foreign_toplevel_handle_v1_activate(t->handle, wl_seat);
}

static void
on_button_clicked(GtkButton *button, gpointer data)
{
	(void)button;
	activate_toplevel((struct toplevel *)data);
}

static void
rebuild_ui(void)
{
	rebuild_idle_id = 0;

	/* Clear existing buttons. */
	GtkWidget *child = gtk_widget_get_first_child(button_box);
	while (child) {
		GtkWidget *next = gtk_widget_get_next_sibling(child);
		gtk_box_remove(GTK_BOX(button_box), child);
		child = next;
	}

	for (GSList *l = toplevels; l; l = l->next) {
		struct toplevel *t = l->data;
		const char *label = t->title ? t->title
			: (t->app_id ? t->app_id : "?");
		t->button = gtk_button_new_with_label(label);
		gtk_widget_set_margin_start(t->button, 4);
		gtk_widget_set_margin_end(t->button, 4);
		if (t->activated) {
			gtk_widget_add_css_class(t->button, "active");
		}
		gtk_box_append(GTK_BOX(button_box), t->button);
		g_signal_connect(t->button, "clicked",
			G_CALLBACK(on_button_clicked), t);
	}
}

static gboolean
rebuild_idle(gpointer data)
{
	(void)data;
	rebuild_ui();
	return G_SOURCE_REMOVE;
}

static void
schedule_rebuild(void)
{
	if (rebuild_idle_id == 0) {
		rebuild_idle_id = g_idle_add(rebuild_idle, NULL);
	}
}

/* --------------------------------------------------- toplevel callbacks */

static void
toplevel_title(void *data, struct zwlr_foreign_toplevel_handle_v1 *h,
	const char *title)
{
	(void)h;
	struct toplevel *t = data;
	g_free(t->title);
	t->title = title ? g_strdup(title) : NULL;
}

static void
toplevel_app_id(void *data, struct zwlr_foreign_toplevel_handle_v1 *h,
	const char *app_id)
{
	(void)h;
	struct toplevel *t = data;
	g_free(t->app_id);
	t->app_id = app_id ? g_strdup(app_id) : NULL;
}

static void
toplevel_state(void *data, struct zwlr_foreign_toplevel_handle_v1 *h,
	struct wl_array *states)
{
	(void)h;
	struct toplevel *t = data;
	t->activated = FALSE;
	uint32_t *s;
	wl_array_for_each(s, states) {
		if (*s == ZWLR_FOREIGN_TOPLEVEL_HANDLE_V1_STATE_ACTIVATED) {
			t->activated = TRUE;
		}
	}
}

static void
toplevel_done(void *data, struct zwlr_foreign_toplevel_handle_v1 *h)
{
	(void)data;
	(void)h;
	schedule_rebuild();
}

static void
toplevel_closed(void *data, struct zwlr_foreign_toplevel_handle_v1 *h)
{
	(void)h;
	struct toplevel *t = data;
	toplevels = g_slist_remove(toplevels, t);
	g_free(t->title);
	g_free(t->app_id);
	g_free(t);
	schedule_rebuild();
}

static void
toplevel_output_enter(void *data, struct zwlr_foreign_toplevel_handle_v1 *h,
	struct wl_output *output)
{
	(void)data;
	(void)h;
	(void)output;
}

static void
toplevel_output_leave(void *data, struct zwlr_foreign_toplevel_handle_v1 *h,
	struct wl_output *output)
{
	(void)data;
	(void)h;
	(void)output;
}

static void
toplevel_parent(void *data, struct zwlr_foreign_toplevel_handle_v1 *h,
	struct zwlr_foreign_toplevel_handle_v1 *parent)
{
	(void)data;
	(void)h;
	(void)parent;
}

/*
 * NOTE: every listener entry must be non-NULL. libwayland dispatches events
 * straight through the listener table, and a NULL entry aborts the process
 * the moment the compositor sends that event (we crashed on output_enter).
 */
static const struct zwlr_foreign_toplevel_handle_v1_listener handle_listener = {
	.title = toplevel_title,
	.app_id = toplevel_app_id,
	.output_enter = toplevel_output_enter,
	.output_leave = toplevel_output_leave,
	.state = toplevel_state,
	.done = toplevel_done,
	.closed = toplevel_closed,
	.parent = toplevel_parent,
};

static void
manager_toplevel(void *data,
	struct zwlr_foreign_toplevel_manager_v1 *mgr,
	struct zwlr_foreign_toplevel_handle_v1 *handle)
{
	(void)data;
	(void)mgr;
	struct toplevel *t = g_new0(struct toplevel, 1);
	t->handle = handle;
	t->title = NULL;
	t->app_id = NULL;
	t->activated = FALSE;
	t->button = NULL;
	toplevels = g_slist_append(toplevels, t);
	g_message("oxide-panel: new toplevel handle %p", (void *)handle);
	zwlr_foreign_toplevel_handle_v1_add_listener(handle,
		&handle_listener, t);
}

static void
manager_finished(void *data, struct zwlr_foreign_toplevel_manager_v1 *mgr)
{
	(void)data;
	(void)mgr;
	g_warning("oxide-panel: compositor finished the toplevel manager");
}

static const struct zwlr_foreign_toplevel_manager_v1_listener manager_listener = {
	.toplevel = manager_toplevel,
	.finished = manager_finished,
};

/* ------------------------------------------------------- registry glue */

static void
registry_global(void *data, struct wl_registry *registry, uint32_t id,
	const char *interface, uint32_t version)
{
	(void)data;
	if (strcmp(interface, zwlr_foreign_toplevel_manager_v1_interface.name)
			== 0) {
		uint32_t v = version > 3 ? 3 : version;
		manager = wl_registry_bind(registry, id,
			&zwlr_foreign_toplevel_manager_v1_interface, v);
		zwlr_foreign_toplevel_manager_v1_add_listener(manager,
			&manager_listener, NULL);
		g_message("oxide-panel: bound zwlr_foreign_toplevel_manager_v1 (v%u)", v);
	}
}

static void
registry_global_remove(void *data, struct wl_registry *registry, uint32_t id)
{
	(void)data;
	(void)registry;
	(void)id;
}

static const struct wl_registry_listener registry_listener = {
	.global = registry_global,
	.global_remove = registry_global_remove,
};

static void
setup_foreign_toplevel(void)
{
	wl_display = gdk_wayland_display_get_wl_display(
		gdk_display_get_default());
	if (!wl_display) {
		g_warning("oxide-panel: failed to get wl_display from GDK");
		return;
	}
	struct wl_registry *registry = wl_display_get_registry(wl_display);
	wl_registry_add_listener(registry, &registry_listener, NULL);
	/* Round-trip so the manager global is bound before we return. */
	wl_display_roundtrip(wl_display);
	if (!manager) {
		g_warning("oxide-panel: foreign-toplevel manager not available; "
			"is oxide-desktop advertising it?");
	}
}

static void
apply_css(GtkWidget *win)
{
	gtk_widget_add_css_class(win, "oxide-panel");
	GtkCssProvider *prov = gtk_css_provider_new();
	gtk_css_provider_load_from_string(prov,
		"window.oxide-panel {"
		"	background-color: rgba(24, 26, 29, 0.96);"
		"	color: #e6e6e6;"
		"	font-size: 12px;"
		"}"
		"window.oxide-panel button {"
		"	background: none;"
		"	border: none;"
		"	border-radius: 0;"
		"	padding: 2px 8px;"
		"	color: #b8babf;"
		"}"
		"window.oxide-panel button:hover {"
		"	background-color: rgba(255, 255, 255, 0.14);"
		"	color: #ffffff;"
		"}"
		"window.oxide-panel button.active {"
		"	background-color: rgba(255, 255, 255, 0.10);"
		"	color: #ffffff;"
		"}");
	gtk_style_context_add_provider_for_display(gdk_display_get_default(),
		GTK_STYLE_PROVIDER(prov),
		GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
	g_object_unref(prov);
}

int
main(int argc, char **argv)
{
	gtk_init();
	load_settings();

	GtkWidget *win = gtk_window_new();
	gtk_window_set_title(GTK_WINDOW(win), "oxide-panel");
	gtk_layer_init_for_window(GTK_WINDOW(win));
	gtk_layer_set_namespace(GTK_WINDOW(win), "oxide-panel");
	gtk_layer_set_layer(GTK_WINDOW(win), GTK_LAYER_SHELL_LAYER_TOP);
	gtk_layer_set_anchor(GTK_WINDOW(win), GTK_LAYER_SHELL_EDGE_TOP,
		!panel_on_bottom);
	gtk_layer_set_anchor(GTK_WINDOW(win), GTK_LAYER_SHELL_EDGE_BOTTOM,
		panel_on_bottom);
	gtk_layer_set_anchor(GTK_WINDOW(win), GTK_LAYER_SHELL_EDGE_LEFT, TRUE);
	gtk_layer_set_anchor(GTK_WINDOW(win), GTK_LAYER_SHELL_EDGE_RIGHT, TRUE);
	gtk_layer_set_exclusive_zone(GTK_WINDOW(win), panel_height);
	gtk_window_set_default_size(GTK_WINDOW(win), -1, panel_height);

	apply_css(win);

	button_box = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 0);
	gtk_widget_set_margin_start(button_box, 6);
	gtk_widget_set_margin_end(button_box, 6);
	gtk_window_set_child(GTK_WINDOW(win), button_box);

	gtk_window_present(GTK_WINDOW(win));

	setup_foreign_toplevel();

	GMainLoop *loop = g_main_loop_new(NULL, FALSE);
	g_main_loop_run(loop);

	return 0;
}
