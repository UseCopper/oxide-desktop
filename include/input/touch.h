/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_TOUCH_H
#define OXIDE_DESKTOP_TOUCH_H

struct seat;

void touch_init(struct seat *seat);
void touch_finish(struct seat *seat);

#endif /* OXIDE_DESKTOP_TOUCH_H */
