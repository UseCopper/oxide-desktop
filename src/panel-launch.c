// SPDX-License-Identifier: GPL-2.0-only
#define _POSIX_C_SOURCE 200809L
#include "panel-launch.h"

#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <glib.h>

#include <wlr/util/log.h>

#include "common/spawn.h"

/*
 * Directory of the running compositor executable, resolved via
 * /proc/self/exe. Used to find the panel next to us regardless of the
 * install prefix (or whether we were launched via PATH).
 */
static char *
self_dir(void)
{
	char buf[PATH_MAX];
	ssize_t n = readlink("/proc/self/exe", buf, sizeof(buf) - 1);
	if (n <= 0) {
		return NULL;
	}
	buf[n] = '\0';
	char *slash = strrchr(buf, '/');
	if (!slash) {
		return NULL;
	}
	*slash = '\0';
	return g_strdup(buf);
}

void
oxide_launch_panel(void)
{
	char *cmd = NULL;

	/*
	 * 1. Explicit override for development (e.g. a path into the build
	 *    tree, or a wrapper script).
	 */
	const char *env = getenv("OXIDE_PANEL_CMD");
	if (env && *env) {
		cmd = g_strdup(env);
	}

	/*
	 * 2. Look for the panel binary sitting next to the compositor. After
	 *    a normal `meson install` both live in the same bindir, so this
	 *    works no matter where that prefix is.
	 */
	if (!cmd) {
		char *dir = self_dir();
		if (dir) {
			char *candidate =
				g_strdup_printf("%s/oxide-panel", dir);
			if (access(candidate, X_OK) == 0) {
				cmd = candidate;
			} else {
				g_free(candidate);
			}
			g_free(dir);
		}
	}

	/*
	 * 3. Fall back to PATH (e.g. when run from a build tree whose panel
	 *    lives elsewhere).
	 */
	if (!cmd) {
		cmd = g_strdup("oxide-panel");
	}

	wlr_log(WLR_INFO, "launching panel: %s", cmd);
	spawn_async_no_shell(cmd);
	g_free(cmd);
}
