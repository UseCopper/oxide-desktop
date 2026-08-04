// SPDX-License-Identifier: GPL-2.0-only
#define _POSIX_C_SOURCE 200809L
#include "settings/settings.h"

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <glib.h>

#include "common/mem.h"
#include "common/string-helpers.h"

/*
 * GKeyFile-backed settings store. See include/settings/settings.h for the
 * contract. One global; the compositor process has exactly one settings
 * store and the CLI/GUI invocations each have their own.
 */

static void settings_log_err(const char *fmt, ...);

static void
settings_log_err(const char *fmt, ...)
{
	va_list ap;
	va_start(ap, fmt);
	fputs("oxide-desktop settings: ", stderr);
	vfprintf(stderr, fmt, ap);
	fputc('\n', stderr);
	va_end(ap);
}

struct oxide_settings g_settings;
static char *g_settings_path;
static bool g_initialized;

/* ------------------------------------------------------------------ path */

const char *
oxide_settings_path(void)
{
	if (g_settings_path) {
		return g_settings_path;
	}
	const char *xdg = getenv("XDG_CONFIG_HOME");
	char *base;
	if (xdg && *xdg) {
		base = xstrdup(xdg);
	} else {
		const char *home = getenv("HOME");
		if (!home || !*home) {
			return NULL;
		}
		base = strdup_printf("%s/.config", home);
	}
	char *out = strdup_printf("%s/oxide-desktop/settings.conf", base);
	free(base);
	g_settings_path = out;
	return g_settings_path;
}

/* --------------------------------------------------------------- defaults */

static void
set_defaults(struct oxide_settings *s)
{
	s->panel_height = 36;
	s->panel_position = xstrdup("top");
	s->panel_output = xstrdup("all");
	s->theme_name = NULL; /* labwc default */
}

static void
clear_fields(struct oxide_settings *s)
{
	zfree(s->panel_position);
	zfree(s->panel_output);
	zfree(s->theme_name);
}

/* ----------------------------------------------------------------- load */

static char *
kf_string(GKeyFile *kf, const char *group, const char *key, const char *def)
{
	GError *err = NULL;
	gchar *v = g_key_file_get_string(kf, group, key, &err);
	if (err) {
		g_error_free(err);
		return xstrdup(def);
	}
	char *out = xstrdup(v);
	g_free(v);
	return out;
}

static int
kf_int(GKeyFile *kf, const char *group, const char *key, int def)
{
	GError *err = NULL;
	gint v = g_key_file_get_integer(kf, group, key, &err);
	if (err) {
		g_error_free(err);
		return def;
	}
	return (int)v;
}

static bool
load_from_keyfile(struct oxide_settings *s, GKeyFile *kf)
{
	clear_fields(s);
	s->panel_height = kf_int(kf, "Shell", "panel-height", 36);
	s->panel_position = kf_string(kf, "Shell", "panel-position", "top");
	s->panel_output = kf_string(kf, "Shell", "panel-output", "all");
	s->theme_name = kf_string(kf, "Theme", "name", "");
	if (s->theme_name && !*s->theme_name) {
		zfree(s->theme_name); /* empty == unset */
	}
	return true;
}

static bool
ensure_parent_dir(const char *path)
{
	char *dup = xstrdup(path);
	bool ok = true;
	char *slash = strrchr(dup, '/');
	if (slash) {
		*slash = '\0';
		if (g_mkdir_with_parents(dup, 0700) != 0) {
			settings_log_err("cannot create %s: %m", dup);
			ok = false;
		}
	}
	free(dup);
	return ok;
}

static bool
write_defaults(const char *path)
{
	if (!ensure_parent_dir(path)) {
		return false;
	}
	GKeyFile *kf = g_key_file_new();
	g_key_file_set_integer(kf, "Shell", "panel-height", 36);
	g_key_file_set_string(kf, "Shell", "panel-position", "top");
	g_key_file_set_string(kf, "Shell", "panel-output", "all");
	g_key_file_set_string(kf, "Theme", "name", "");

	GError *err = NULL;
	gsize len = 0;
	gchar *data = g_key_file_to_data(kf, &len, NULL);
	gboolean ok = g_file_set_contents(path, data, len, &err);
	g_free(data);
	g_key_file_free(kf);
	if (!ok) {
		settings_log_err("cannot write default %s: %s", path,
			err && err->message ? err->message : "?");
		if (err) {
			g_error_free(err);
		}
		return false;
	}
	return true;
}

bool
oxide_settings_reload(void)
{
	const char *path = oxide_settings_path();
	if (!path) {
		settings_log_err("cannot resolve settings path (no HOME/XDG_CONFIG_HOME)");
		return false;
	}

	GKeyFile *kf = g_key_file_new();
	GError *err = NULL;
	if (!g_key_file_load_from_file(kf, path, G_KEY_FILE_NONE, &err)) {
		g_error_free(err);
		err = NULL;
		if (!write_defaults(path)) {
			g_key_file_free(kf);
			return false;
		}
		if (!g_key_file_load_from_file(kf, path, G_KEY_FILE_NONE, &err)) {
			settings_log_err("cannot read %s: %s", path,
				err && err->message ? err->message : "?");
			if (err) {
				g_error_free(err);
			}
			g_key_file_free(kf);
			return false;
		}
	}

	bool r = load_from_keyfile(&g_settings, kf);
	g_key_file_free(kf);
	return r;
}

/* ----------------------------------------------------------------- save */

bool
oxide_settings_save(void)
{
	const char *path = oxide_settings_path();
	if (!path) {
		return false;
	}
	if (!ensure_parent_dir(path)) {
		return false;
	}

	GKeyFile *kf = g_key_file_new();
	g_key_file_set_integer(kf, "Shell", "panel-height",
		g_settings.panel_height);
	g_key_file_set_string(kf, "Shell", "panel-position",
		g_settings.panel_position ? g_settings.panel_position : "top");
	g_key_file_set_string(kf, "Shell", "panel-output",
		g_settings.panel_output ? g_settings.panel_output : "all");
	g_key_file_set_string(kf, "Theme", "name",
		g_settings.theme_name ? g_settings.theme_name : "");

	GError *err = NULL;
	gsize len = 0;
	gchar *data = g_key_file_to_data(kf, &len, NULL);
	gboolean ok = g_file_set_contents(path, data, len, &err);
	g_free(data);
	g_key_file_free(kf);
	if (!ok) {
		settings_log_err("cannot write %s: %s", path,
			err && err->message ? err->message : "?");
		if (err) {
			g_error_free(err);
		}
		return false;
	}
	return true;
}

/* --------------------------------------------------------------- getters */

const struct oxide_settings *
oxide_settings_get(void)
{
	return &g_settings;
}

/* --------------------------------------------------------------- setters */

void
oxide_settings_set_panel_height(int px)
{
	if (px < 1) {
		px = 1;
	}
	g_settings.panel_height = px;
}

void
oxide_settings_set_panel_position(const char *pos)
{
	zfree(g_settings.panel_position);
	g_settings.panel_position = xstrdup(pos ? pos : "top");
}

void
oxide_settings_set_panel_output(const char *out)
{
	zfree(g_settings.panel_output);
	g_settings.panel_output = xstrdup(out ? out : "all");
}

void
oxide_settings_set_theme_name(const char *name)
{
	zfree(g_settings.theme_name);
	g_settings.theme_name = (name && *name) ? xstrdup(name) : NULL;
}

/* ----------------------------------------------------------------- init */

void
oxide_settings_init(void)
{
	if (g_initialized) {
		return;
	}
	set_defaults(&g_settings);
	g_initialized = true;
}

void
oxide_settings_finish(void)
{
	if (!g_initialized) {
		return;
	}
	clear_fields(&g_settings);
	zfree(g_settings_path);
	g_initialized = false;
}
