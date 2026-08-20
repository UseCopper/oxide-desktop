// SPDX-License-Identifier: GPL-2.0-only
#define _POSIX_C_SOURCE 200809L
#include "panel-launch.h"

#include <stdlib.h>
#include <wlr/util/log.h>

#include "common/spawn.h"

void
oxide_launch_panel(void)
{
	/*
	 * Allow overriding the panel command (e.g. a full path to the build
	 * tree binary, or a wrapper) for development via OXIDE_PANEL_CMD.
	 */
	const char *cmd = getenv("OXIDE_PANEL_CMD");
	if (!cmd || !*cmd) {
		cmd = "oxide-panel";
	}
	wlr_log(WLR_INFO, "launching panel: %s", cmd);
	spawn_async_no_shell(cmd);
}
