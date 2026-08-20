// SPDX-License-Identifier: GPL-2.0-only
#define _POSIX_C_SOURCE 200809L
#include "shell/panel.h"

#include <gtk4-layer-shell.h>
#include <gtk/gtk.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <wayland-util.h>

#include "buffer.h"
#include "common/list.h"
#include "config.h"
#include "desktop-entry.h"
#include "img/img.h"
#include "oxide-desktop.h"
#include "settings/settings.h"
#include "view.h"
#include "window-rules.h"

#define PANEL_ICON_SIZE 22

struct oxide_panel {
	GtkWindow *window;
	GtkWidget *taskbar;   /* GtkBox holding one button per open view */
	struct wl_list views; /* struct panel_view::link */
};

struct panel_view {
	struct wl_list link;
	struct view *view;
	GtkWidget *button;
	GtkImage *image;

	struct wl_listener on_set_icon;
	struct wl_listener on_new_title;
	struct wl_listener on_new_app_id;
	struct wl_listener on_destroy;
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
		"window.oxide-panel button.oxide-taskbar-button {"
		"	background: none;"
		"	border: none;"
		"	border-radius: 4px;"
		"	padding: 2px 4px;"
		"}"
		"window.oxide-panel button.oxide-taskbar-button:hover {"
		"	background-color: rgba(255, 255, 255, 0.12);"
		"}");
	gtk_style_context_add_provider_for_display(
		gdk_display_get_default(), GTK_STYLE_PROVIDER(provider),
		GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
}

/* ------------------------------------------------------------ taskbar */

/*
 * Pick the icon buffer closest to PANEL_ICON_SIZE, preferring an oversized
 * buffer over an undersized one (same heuristic as scaled-icon-buffer).
 * Only buffers provided by the client itself are considered.
 */
static struct lab_data_buffer *
choose_best_icon_buffer(struct view *view)
{
	int best_dist = -INT_MAX;
	struct lab_data_buffer *best_buffer = NULL;

	struct lab_data_buffer **buffer;
	wl_array_for_each(buffer, &view->icon.buffers) {
		int curr_dist = (int)(*buffer)->logical_width - PANEL_ICON_SIZE;
		bool curr_is_better;
		if ((curr_dist < 0 && best_dist > 0)
				|| (curr_dist > 0 && best_dist < 0)) {
			curr_is_better = curr_dist > 0;
		} else {
			curr_is_better = abs(curr_dist) < abs(best_dist);
		}
		if (curr_is_better) {
			best_dist = curr_dist;
			best_buffer = *buffer;
		}
	}
	return best_buffer;
}

/*
 * Copy the buffer's pixels into a GdkTexture. lab_data_buffer data is
 * DRM_FORMAT_ARGB8888 (premultiplied), which matches the byte layout of
 * GDK_MEMORY_B8G8R8A8_PREMULTIPLIED, so no repacking is needed regardless
 * of host endianness.
 */
static GdkTexture *
icon_buffer_to_texture(struct lab_data_buffer *buf)
{
	gsize size = (gsize)buf->stride * buf->logical_height;
	gpointer copy = g_malloc(size);
	memcpy(copy, buf->data, size);
	GBytes *bytes = g_bytes_new_take(copy, size);
	GdkTexture *texture = gdk_memory_texture_new(
		buf->logical_width, buf->logical_height,
		GDK_MEMORY_B8G8R8A8_PREMULTIPLIED,
		bytes, buf->stride);
	g_bytes_unref(bytes);
	return texture;
}

/* Visible generic-app glyph for clients that provide no icon buffers. */
static float
round_rect_sdf(float x, float y, float cx, float cy, float hw, float hh, float r)
{
	float qx = fabsf(x - cx) - (hw - r);
	float qy = fabsf(y - cy) - (hh - r);
	float ex = fmaxf(qx, 0.0f);
	float ey = fmaxf(qy, 0.0f);
	return sqrtf(ex * ex + ey * ey) + fminf(fmaxf(qx, qy), 0.0f) - r;
}

static GdkTexture *
make_placeholder_texture(void)
{
	int s = PANEL_ICON_SIZE;
	gsize stride = (gsize)s * 4;
	gsize size = stride * s;
	guchar *data = g_malloc0(size);
	uint32_t *px = (uint32_t *)data;

	for (int y = 0; y < s; y++) {
		for (int x = 0; x < s; x++) {
			/* base rounded square (dark neutral fill) */
			float d_base = round_rect_sdf(x + 0.5f, y + 0.5f,
				s / 2.0f, s / 2.0f,
				(s - 2) / 2.0f, (s - 2) / 2.0f, 5.0f);
			float a_base = CLAMP(0.5f - d_base, 0.0f, 1.0f);
			if (a_base <= 0) {
				continue;
			}
			uint8_t r = 104, g = 106, b = 114;
			uint8_t a = (uint8_t)(a_base * 255);

			/* lighter "window" rectangle inside */
			float d_win = round_rect_sdf(x + 0.5f, y + 0.5f,
				s / 2.0f, (s - 1) / 2.0f,
				6.5f, 6.0f, 2.0f);
			if (d_win < 0) {
				r = 225; g = 227; b = 233;
				a = 255;
			} else {
				float a_win = CLAMP(-d_win + 0.5f, 0.0f, 1.0f);
				if (a_win > 0) {
					r = (uint8_t)((1 - a_win) * r + a_win * 225);
					g = (uint8_t)((1 - a_win) * g + a_win * 227);
					b = (uint8_t)((1 - a_win) * b + a_win * 233);
				}
			}

			/* darker "title bar" strip */
			float d_bar = round_rect_sdf(x + 0.5f, y + 0.5f,
				s / 2.0f, (s - 5.5f) / 2.0f,
				6.5f, 2.5f, 2.0f);
			if (d_bar < 0) {
				r = 130; g = 133; b = 144;
				a = 255;
			} else if (d_bar < 0.5f) {
				float t = 0.5f - d_bar;
				r = (uint8_t)((1 - t) * r + t * 130);
				g = (uint8_t)((1 - t) * g + t * 133);
				b = (uint8_t)((1 - t) * b + t * 144);
			}

			/* premultiply */
			uint8_t pa = a;
			uint8_t pr = (uint8_t)((uint16_t)r * pa / 255);
			uint8_t pg = (uint8_t)((uint16_t)g * pa / 255);
			uint8_t pb = (uint8_t)((uint16_t)b * pa / 255);
			px[y * s + x] = pa << 24 | pr << 16 | pg << 8 | pb;
		}
	}

	GBytes *bytes = g_bytes_new_take(data, size);
	GdkTexture *texture = gdk_memory_texture_new(
		s, s, GDK_MEMORY_B8G8R8A8_PREMULTIPLIED, bytes, stride);
	g_bytes_unref(bytes);
	return texture;
}

static void
panel_view_update_icon(struct panel_view *pv)
{
	GdkTexture *texture = NULL;

	/* 1. Client-provided icon buffers (xdg-toplevel-icon / _NET_WM_ICON). */
	struct lab_data_buffer *buf = choose_best_icon_buffer(pv->view);
	if (buf) {
		g_message("oxide panel: icon %s %ux%u",
			pv->view->app_id, buf->logical_width, buf->logical_height);
		texture = icon_buffer_to_texture(buf);
	}

#if HAVE_LIBSFDO
	/* 2. Icon theme lookup by app_id (same path as labwc titlebar icons). */
	if (!texture && pv->view->app_id && *pv->view->app_id) {
		struct lab_img *img = desktop_entry_load_icon_from_app_id(
			pv->view->app_id, PANEL_ICON_SIZE, 1.0f);
		if (img) {
			struct lab_data_buffer *ibuf = lab_img_render(
				img, PANEL_ICON_SIZE, PANEL_ICON_SIZE, 1.0f);
			lab_img_destroy(img);
			if (ibuf) {
				g_message("oxide panel: icon theme %s",
					pv->view->app_id);
				texture = icon_buffer_to_texture(ibuf);
				wlr_buffer_drop(&ibuf->base);
			}
		}
	}
#endif

	if (!texture) {
		texture = make_placeholder_texture();
	}
	gtk_image_set_from_paintable(pv->image, GDK_PAINTABLE(texture));
	gtk_image_set_pixel_size(pv->image, PANEL_ICON_SIZE);
	g_object_unref(texture);
}

static void
panel_view_update_tooltip(struct panel_view *pv)
{
	const char *title = pv->view->title;
	const char *tip = (title && *title) ? title : pv->view->app_id;
	gtk_widget_set_tooltip_text(pv->button, tip);
}

static void
on_view_button_clicked(GtkButton *button, gpointer data)
{
	struct panel_view *pv = data;
	(void)button;
	desktop_focus_view(pv->view, /*raise*/ true);
}

static void
panel_view_free(struct panel_view *pv)
{
	wl_list_remove(&pv->on_set_icon.link);
	wl_list_remove(&pv->on_new_title.link);
	wl_list_remove(&pv->on_new_app_id.link);
	wl_list_remove(&pv->on_destroy.link);
	wl_list_remove(&pv->link);
	gtk_widget_unparent(pv->button);
	g_free(pv);
}

static void
handle_view_set_icon(struct wl_listener *listener, void *data)
{
	struct panel_view *pv = wl_container_of(listener, pv, on_set_icon);
	(void)data;
	panel_view_update_icon(pv);
}

static void
handle_view_new_title(struct wl_listener *listener, void *data)
{
	struct panel_view *pv = wl_container_of(listener, pv, on_new_title);
	(void)data;
	panel_view_update_tooltip(pv);
}

static void
handle_view_new_app_id(struct wl_listener *listener, void *data)
{
	struct panel_view *pv = wl_container_of(listener, pv, on_new_app_id);
	(void)data;
	panel_view_update_icon(pv);
	panel_view_update_tooltip(pv);
}

static void
handle_view_destroy(struct wl_listener *listener, void *data)
{
	struct panel_view *pv = wl_container_of(listener, pv, on_destroy);
	(void)data;
	panel_view_free(pv);
}

void
oxide_panel_add_view(struct oxide_panel *panel, struct view *view)
{
	struct panel_view *pv;
	wl_list_for_each(pv, &panel->views, link) {
		if (pv->view == view) {
			return;
		}
	}

	pv = g_new0(struct panel_view, 1);
	pv->view = view;

	pv->button = gtk_button_new();
	gtk_widget_add_css_class(pv->button, "oxide-taskbar-button");
	pv->image = GTK_IMAGE(gtk_image_new());
	gtk_button_set_child(GTK_BUTTON(pv->button), GTK_WIDGET(pv->image));
	gtk_box_append(GTK_BOX(panel->taskbar), pv->button);

	panel_view_update_icon(pv);
	panel_view_update_tooltip(pv);

	g_signal_connect(pv->button, "clicked",
		G_CALLBACK(on_view_button_clicked), pv);

	pv->on_set_icon.notify = handle_view_set_icon;
	wl_signal_add(&view->events.set_icon, &pv->on_set_icon);
	pv->on_new_title.notify = handle_view_new_title;
	wl_signal_add(&view->events.new_title, &pv->on_new_title);
	pv->on_new_app_id.notify = handle_view_new_app_id;
	wl_signal_add(&view->events.new_app_id, &pv->on_new_app_id);
	pv->on_destroy.notify = handle_view_destroy;
	wl_signal_add(&view->events.destroy, &pv->on_destroy);

	wl_list_append(&panel->views, &pv->link);
	g_message("oxide panel: taskbar + %s", view->app_id);
}

void
oxide_panel_remove_view(struct oxide_panel *panel, struct view *view)
{
	struct panel_view *pv;
	wl_list_for_each(pv, &panel->views, link) {
		if (pv->view == view) {
			g_message("oxide panel: taskbar - %s", view->app_id);
			panel_view_free(pv);
			return;
		}
	}
}

/* Catch views that were mapped before the panel was constructed. */
static void
panel_add_existing_views(struct oxide_panel *p)
{
	struct view *view;
	for_each_view(view, &server.views, LAB_VIEW_CRITERIA_NONE) {
		if (view->mapped && view_is_focusable(view)
				&& window_rules_get_property(view, "skipTaskbar")
					!= LAB_PROP_TRUE) {
			oxide_panel_add_view(p, view);
		}
	}
}

/* ------------------------------------------------------------ lifecycle */

struct oxide_panel *
oxide_panel_new(void)
{
	struct oxide_panel *p = g_new0(struct oxide_panel, 1);

	install_css();
	wl_list_init(&p->views);

	p->window = GTK_WINDOW(gtk_window_new());
	gtk_widget_add_css_class(GTK_WIDGET(p->window), "oxide-panel");

	GtkWidget *box = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 0);
	gtk_widget_set_margin_start(box, 8);
	gtk_widget_set_margin_end(box, 8);
	gtk_window_set_child(p->window, box);

	p->taskbar = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 2);
	gtk_box_append(GTK_BOX(box), p->taskbar);

	/* Make it a layer surface. Must happen before realize. */
	gtk_layer_init_for_window(p->window);
	gtk_layer_set_namespace(p->window, "oxide-desktop-panel");

	apply_geometry(p);

	g_signal_connect(p->window, "map", G_CALLBACK(on_map), p);

	gtk_widget_set_visible(GTK_WIDGET(p->window), TRUE);

	panel_add_existing_views(p);
	return p;
}

void
oxide_panel_destroy(struct oxide_panel *p)
{
	if (!p) {
		return;
	}
	struct panel_view *pv, *tmp;
	wl_list_for_each_safe(pv, tmp, &p->views, link) {
		panel_view_free(pv);
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
