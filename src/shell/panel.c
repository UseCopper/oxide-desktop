// SPDX-License-Identifier: GPL-2.0-only
#define _POSIX_C_SOURCE 200809L
#include "shell/panel.h"

#include <gtk4-layer-shell.h>
#include <gtk/gtk.h>

#include "settings/settings.h"

struct oxide_panel {
	GtkWindow *window;
	GtkWidget *label;
};

static void
on_map(GtkWidget *widget, gpointer data)
{
	struct oxide_panel *p = data;
	int w = gtk_widget_get_width(widget);
	int h = gtk_widget_get_height(widget);
	int ex = gtk_layer_get_exclusive_zone(p->window);
	g_message("oxide panel mapped: %dx%d exclusive=%d",
		w, h, ex);
}

static void
apply_geometry(struct oxide_panel *p)
{
	const struct oxide_settings *s = oxide_settings_get();
	int h = s->panel_height;
	if (h < 1) {
		h = 1;
	}

	GtkWindow *win = p->window;

	/* Anchor to top, span left->right. Edges default to FALSE so we only
	 * set the ones we want. */
	gtk_layer_set_anchor(win, GTK_LAYER_SHELL_EDGE_TOP, TRUE);
	gtk_layer_set_anchor(win, GTK_LAYER_SHELL_EDGE_LEFT, TRUE);
	gtk_layer_set_anchor(win, GTK_LAYER_SHELL_EDGE_RIGHT, TRUE);
	gtk_layer_set_anchor(win, GTK_LAYER_SHELL_EDGE_BOTTOM, FALSE);

	/* Flip to the bottom edge if configured. */
	if (s->panel_position && g_str_equal(s->panel_position, "bottom")) {
		gtk_layer_set_anchor(win, GTK_LAYER_SHELL_EDGE_TOP, FALSE);
		gtk_layer_set_anchor(win, GTK_LAYER_SHELL_EDGE_BOTTOM, TRUE);
	}
	gtk_layer_set_layer(win, GTK_LAYER_SHELL_LAYER_TOP);

	/* Reserve space so tiled windows don't go under us. */
	gtk_layer_set_exclusive_zone(win, h);
	gtk_window_set_default_size(win, -1, h);
}

static void
install_css(void)
{
	static GtkCssProvider *provider;

	if (provider) {
		return;
	}
	provider = gtk_css_provider_new();
	gtk_css_provider_load_from_string(provider,
		"window.oxide-panel {"
		"	background-color: rgba(40, 42, 48, 0.92);"
		"	color: #e6e6e6;"
		"}"
		"window.oxide-panel label {"
		"	padding: 0 8px;"
		"	font-size: 13px;"
		"}");
	gtk_style_context_add_provider_for_display(
		gdk_display_get_default(), GTK_STYLE_PROVIDER(provider),
		GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
}

struct oxide_panel *
oxide_panel_new(void)
{
	struct oxide_panel *p = g_new0(struct oxide_panel, 1);

	install_css();

	p->window = GTK_WINDOW(gtk_window_new());
	gtk_widget_add_css_class(GTK_WIDGET(p->window), "oxide-panel");

	GtkWidget *box = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 0);
	gtk_widget_set_margin_start(box, 8);
	gtk_widget_set_margin_end(box, 8);
	gtk_window_set_child(p->window, box);

	p->label = gtk_label_new("Oxide Desktop");
	gtk_widget_set_halign(p->label, GTK_ALIGN_START);
	gtk_widget_set_valign(p->label, GTK_ALIGN_CENTER);
	gtk_box_append(GTK_BOX(box), p->label);

	/* Make it a layer surface. Must happen before realize. */
	gtk_layer_init_for_window(p->window);
	gtk_layer_set_namespace(p->window, "oxide-desktop-panel");

	apply_geometry(p);

	g_signal_connect(p->window, "map", G_CALLBACK(on_map), p);

	gtk_widget_set_visible(GTK_WIDGET(p->window), TRUE);
	return p;
}

void
oxide_panel_destroy(struct oxide_panel *p)
{
	if (!p) {
		return;
	}
	if (p->window) {
		gtk_window_destroy(p->window);
	}
	g_free(p);
}

void
oxide_panel_reconfigure(struct oxide_panel *p)
{
	if (!p) {
		return;
	}
	apply_geometry(p);
	const struct oxide_settings *s = oxide_settings_get();
	g_message("oxide panel reconfigured: height=%d position=%s",
		s->panel_height, s->panel_position ? s->panel_position : "top");
}
