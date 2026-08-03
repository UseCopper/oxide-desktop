/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_MAGNIFIER_H
#define OXIDE_DESKTOP_MAGNIFIER_H

#include <stdbool.h>

struct server;
struct output;
struct wlr_buffer;
struct wlr_box;

enum magnify_dir {
	MAGNIFY_INCREASE,
	MAGNIFY_DECREASE
};

void magnifier_toggle(void);
void magnifier_set_scale(enum magnify_dir dir);
bool output_wants_magnification(struct output *output);
void magnifier_draw(struct output *output, struct wlr_buffer *output_buffer,
	struct wlr_box *damage);
bool magnifier_is_enabled(void);
void magnifier_reset(void);

#endif /* OXIDE_DESKTOP_MAGNIFIER_H */
