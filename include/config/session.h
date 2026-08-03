/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_SESSION_H
#define OXIDE_DESKTOP_SESSION_H

struct server;

/**
 * session_run_script - run a named session script (or, in merge-config mode,
 * all named session scripts) from the XDG path.
 */
void session_run_script(const char *script);

/**
 * session_environment_init - set environment variables based on <key>=<value>
 * pairs in `${XDG_CONFIG_DIRS:-/etc/xdg}/oxide-desktop/environment` with user override
 * in `${XDG_CONFIG_HOME:-$HOME/.config}`
 */
void session_environment_init(void);

/**
 * session_autostart_init - run autostart file as shell script
 * Note: Same as `sh ~/.config/oxide-desktop/autostart` (or equivalent XDG config dir)
 */
void session_autostart_init(void);

/**
 * session_shutdown - run session shutdown file as shell script
 * Note: Same as `sh ~/.config/oxide-desktop/shutdown` (or equivalent XDG config dir)
 */
void session_shutdown(void);

#endif /* OXIDE_DESKTOP_SESSION_H */
