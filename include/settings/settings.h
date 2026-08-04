/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_SETTINGS_H
#define OXIDE_SETTINGS_H

#include <stdbool.h>

/*
 * oxide-desktop user settings.
 *
 * Replaces the labwc rc.xml for the keys we own. Stored as a GKeyFile at
 *  ${XDG_CONFIG_HOME:-~/.config}/oxide-desktop/settings.conf
 *
 * Lifecycle: settings_load() parses the file (or writes a default one on first
 * run, optionally seeded from a legacy rc.xml). After load, gets are plain
 * struct field reads. settings_save() writes the current struct back.
 * settings_reload() is the SIGHUP path: drop and reload, then the caller
 * re-applies whatever depends on settings (panel geometry, etc.).
 *
 * This module has no compositor / GTK dependencies on purpose so the same
 * object file links into the compositor, the settings GUI, and the settings
 * CLI.
 */

struct oxide_settings {
	/* [Shell] */
	int panel_height;          /* px */
	char *panel_position;      /* "top" | "bottom" */
	char *panel_output;        /* "all" or an output name */

	/* [Theme] */
	char *theme_name;          /* may be NULL = labwc default */
};

/* path to the settings file (resolved at first load); NULL if it can't be. */
const char *oxide_settings_path(void);

/* One-time init of the module. Idempotent. */
void oxide_settings_init(void);
void oxide_settings_finish(void);

/*
 * (Re)load from disk into the in-memory struct. Returns true on success. On
 * first run with no file, writes a default settings.conf. If a legacy labwc
 * rc.xml is found and no settings.conf exists, the default is seeded from it
 * (best-effort).
 */
bool oxide_settings_reload(void);

/*
 * Get a pointer to the live settings struct. Only valid between
 * oxide_settings_init() and oxide_settings_finish(); reload() may replace its
 * contents. Caller must not free returned strings.
 */
const struct oxide_settings *oxide_settings_get(void);

/*
 * Set helpers used by the settings GUI/CLI. These mutate the in-memory struct
 * only; call oxide_settings_save() to persist.
 */
void oxide_settings_set_panel_height(int px);
void oxide_settings_set_panel_position(const char *pos);
void oxide_settings_set_panel_output(const char *out);
void oxide_settings_set_theme_name(const char *name);

/* Persist the in-memory struct to settings.conf. Returns true on success. */
bool oxide_settings_save(void);

#endif /* OXIDE_SETTINGS_H */
