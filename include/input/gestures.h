/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_GESTURES_H
#define OXIDE_DESKTOP_GESTURES_H

struct seat;

void gestures_init(struct seat *seat);
void gestures_finish(struct seat *seat);

#endif /* OXIDE_DESKTOP_GESTURES_H */
