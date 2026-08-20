// SPDX-License-Identifier: GPL-2.0-only
#ifndef OXIDE_PANEL_LAUNCH_H
#define OXIDE_PANEL_LAUNCH_H

/*
 * Launch the out-of-process panel (clients/oxide-panel). The panel is a
 * normal Wayland client of this compositor, so it connects over the existing
 * WAYLAND_DISPLAY and can never deadlock the compositor.
 */
void oxide_launch_panel(void);

#endif /* OXIDE_PANEL_LAUNCH_H */
