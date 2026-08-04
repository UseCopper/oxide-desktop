/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_SHELL_PANEL_H
#define OXIDE_SHELL_PANEL_H

#include <gtk/gtk.h>

/* The top panel. Phase 1: a 36px (configurable) GtkWindow on the Top layer. */

struct oxide_panel;

struct oxide_panel *oxide_panel_new(void);
void oxide_panel_destroy(struct oxide_panel *panel);

/* Re-read settings and resize / reposition / reanchor the panel. */
void oxide_panel_reconfigure(struct oxide_panel *panel);

#endif /* OXIDE_SHELL_PANEL_H */
