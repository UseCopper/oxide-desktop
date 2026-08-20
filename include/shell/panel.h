/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_SHELL_PANEL_H
#define OXIDE_SHELL_PANEL_H

#include <gtk/gtk.h>

/* The top panel: a 36px (configurable) GtkWindow on the Top layer. */

struct oxide_panel;
struct view;

struct oxide_panel *oxide_panel_new(void);
void oxide_panel_destroy(struct oxide_panel *panel);

/* Re-read settings and resize / reposition / reanchor the panel. */
void oxide_panel_reconfigure(struct oxide_panel *panel);

/*
 * Taskbar: add/remove a button for a mapped view. Icons are taken from the
 * buffers the client itself provides (xdg-toplevel-icon / _NET_WM_ICON) and,
 * when libsfdo is available, fall back to an icon-theme lookup by app_id.
 */
void oxide_panel_add_view(struct oxide_panel *panel, struct view *view);
void oxide_panel_remove_view(struct oxide_panel *panel, struct view *view);

#endif /* OXIDE_SHELL_PANEL_H */
