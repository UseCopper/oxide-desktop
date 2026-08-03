/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_IDLE_H
#define OXIDE_DESKTOP_IDLE_H

struct wl_display;
struct wlr_seat;

void idle_manager_create(struct wl_display *display);
void idle_manager_notify_activity(struct wlr_seat *wlr_seat);

#endif /* OXIDE_DESKTOP_IDLE_H */
