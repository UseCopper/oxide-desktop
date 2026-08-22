/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_ANIMATION_H
#define OXIDE_DESKTOP_ANIMATION_H

struct view;

/*
 * Direction the window travels while settling in. E.g. ANIM_DIRECTION_DOWN
 * means the window starts slightly above its final spot and slides down.
 */
enum animation_direction {
	ANIM_DIRECTION_UP,
	ANIM_DIRECTION_DOWN,
	ANIM_DIRECTION_LEFT,
	ANIM_DIRECTION_RIGHT,
};

/* Set the slide-in direction used for subsequently opened windows. */
void animation_set_direction(enum animation_direction dir);

/*
 * Window-open animation: the entire window (content + SSD + border) is
 * lifted into a private wrapper tree which slides into place from the
 * configured direction while every buffer fades in, on an ease-out cubic
 * curve. Children added to the window mid-animation are adopted too.
 */
void animation_start_open(struct view *view);

/* Stop any running open animation and restore the original tree. */
void animation_cancel_open(struct view *view);

struct wlr_scene_node;

/*
 * Fold a newly created child tree (e.g. decorations negotiated slightly
 * after map, as Qt apps do) into the running open animation immediately,
 * applying the current fade/slide state so it never pops in at full
 * brightness. No-op when no animation is running.
 */
void animation_adopt_node(struct view *view, struct wlr_scene_node *node);

#endif /* OXIDE_DESKTOP_ANIMATION_H */
