// SPDX-License-Identifier: GPL-2.0-only
#define _POSIX_C_SOURCE 200809L
#include "animation.h"

#include <stdlib.h>
#include <time.h>

#include <wayland-server-core.h>
#include <wlr/types/wlr_compositor.h>
#include <wlr/types/wlr_scene.h>
#include <wlr/util/log.h>

#include "common/mem.h"
#include "config/rcxml.h"
#include "oxide-desktop.h"
#include "view.h"

/*
 * Window-open animation.
 *
 * The whole window - content, SSD titlebar, border - is lifted into its
 * own wrapper tree so the effect targets a single node: the wrapper
 * slides into place from the configured direction while every buffer in
 * the window fades in, both on an ease-out cubic curve.
 *
 * The wrapper is adoptive: any child that appears directly under the
 * view's scene tree mid-animation (e.g. decorations created slightly
 * after map) is absorbed on the next tick with its position compensated,
 * so nothing pops in detached from the unit.
 */

#define ANIM_DURATION_MS 220
#define ANIM_START_OFFSET 8 /* px away from final position at t=0 */
#define ANIM_TICK_MS 16

struct animation_child {
	struct wlr_scene_node *node;
	int x, y;
};

static enum animation_direction g_direction = ANIM_DIRECTION_DOWN;

void
animation_set_direction(enum animation_direction dir)
{
	g_direction = dir;
}

/* Start offset (relative to final position) for the configured direction */
static void
start_offset(int *ox, int *oy)
{
	switch (g_direction) {
	case ANIM_DIRECTION_UP:
		*ox = 0;
		*oy = ANIM_START_OFFSET; /* starts below, slides up */
		break;
	case ANIM_DIRECTION_LEFT:
		*ox = ANIM_START_OFFSET; /* starts right, slides left */
		*oy = 0;
		break;
	case ANIM_DIRECTION_RIGHT:
		*ox = -ANIM_START_OFFSET; /* starts left, slides right */
		*oy = 0;
		break;
	case ANIM_DIRECTION_DOWN:
	default:
		*ox = 0;
		*oy = -ANIM_START_OFFSET; /* starts above, slides down */
		break;
	}
}

static int64_t
now_us(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (int64_t)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

/* Set opacity on every buffer in the subtree (rects cannot fade). */
static void
set_subtree_opacity(struct wlr_scene_node *node, float opacity)
{
	switch (node->type) {
	case WLR_SCENE_NODE_BUFFER:
		wlr_scene_buffer_set_opacity(
			wlr_scene_buffer_from_node(node), opacity);
		break;
	case WLR_SCENE_NODE_TREE: {
		struct wlr_scene_node *child;
		wl_list_for_each(child,
				&wlr_scene_tree_from_node(node)->children,
				link) {
			set_subtree_opacity(child, opacity);
		}
		break;
	}
	default:
		break;
	}
}

/*
 * Adopt any node that appeared directly under the view's scene tree since
 * the animation started. Coordinates are stored relative to the scene tree
 * so restoring them at the end (when the wrapper sits at its final offset
 * of zero) places everything exactly where labwc left it.
 */
static void
adopt_strays(struct view *view, int wrap_x, int wrap_y)
{
	struct wlr_scene_node *node, *tmp;
	wl_list_for_each_safe(node, tmp, &view->scene_tree->children, link) {
		if (view->open_anim.wrapper &&
				node == &view->open_anim.wrapper->node) {
			continue;
		}
		struct animation_child *kid =
			wl_array_add(&view->open_anim.children, sizeof(*kid));
		if (!kid) {
			continue;
		}
		kid->node = node;
		kid->x = node->x;
		kid->y = node->y;
		wlr_scene_node_set_position(node,
			node->x - wrap_x, node->y - wrap_y);
		wlr_scene_node_reparent(node, view->open_anim.wrapper);
	}
}

/* Put every child back under the scene tree. Only valid once the wrapper
 * has settled at offset zero (i.e. from animation_cancel_open). */
static void
restore_children(struct view *view)
{
	struct animation_child *kid;
	wl_array_for_each(kid, &view->open_anim.children) {
		if (kid->node) {
			wlr_scene_node_set_position(kid->node, kid->x, kid->y);
			wlr_scene_node_reparent(kid->node, view->scene_tree);
		}
	}
	if (view->open_anim.wrapper) {
		wlr_scene_node_destroy(&view->open_anim.wrapper->node);
		view->open_anim.wrapper = NULL;
	}
	view->open_anim.children.size = 0;
}

static int64_t
anim_elapsed_us(struct view *view)
{
	return now_us() - view->open_anim.start_us;
}

static int
tick(void *data)
{
	struct view *view = data;
	if (!view->open_anim.running || !view->open_anim.wrapper) {
		return 0;
	}

	int64_t elapsed_us = anim_elapsed_us(view);
	float t = (float)elapsed_us / (ANIM_DURATION_MS * 1000);
	if (t > 1.0f) {
		t = 1.0f;
	}

	/* ease-out cubic */
	float e = 1.0f - (1.0f - t) * (1.0f - t) * (1.0f - t);

	int ox, oy;
	start_offset(&ox, &oy);

	/* Adopt late arrivals before moving, using the current offset so
	 * they don't jump; they then ride the wrapper like everything else */
	int cur_x = view->open_anim.wrapper->node.x;
	int cur_y = view->open_anim.wrapper->node.y;
	adopt_strays(view, cur_x, cur_y);

	wlr_scene_node_set_position(&view->open_anim.wrapper->node,
		(int)(ox * (1.0f - e) + 0.5f),
		(int)(oy * (1.0f - e) + 0.5f));

	set_subtree_opacity(&view->scene_tree->node, e);

	/* First tick: the window becomes visible with the fade already
	 * applied, so no unanimated frame can ever reach the screen. */
	wlr_scene_node_set_enabled(&view->scene_tree->node, true);

	if (t >= 1.0f) {
		animation_cancel_open(view);
		return 0;
	}

	/*
	 * Explicitly re-arm: some libwayland builds treat the timer as
	 * one-shot regardless of the callback's return value.
	 */
	wl_event_source_timer_update(view->open_anim.timer, ANIM_TICK_MS);
	return 0;
}

void
animation_start_open(struct view *view)
{
	if (!view || !view->surface || !view->scene_tree) {
		return;
	}
	if (view->open_anim.running) {
		animation_cancel_open(view);
	}

	/* Snapshot the current direct children of the view's scene tree */
	struct wl_list *head = &view->scene_tree->children;
	size_t count = 0;
	struct wlr_scene_node *node;
	wl_list_for_each(node, head, link) {
		count++;
	}
	if (!count) {
		return;
	}

	struct animation_child *kids = znew_n(struct animation_child, count);
	size_t i = 0;
	wl_list_for_each(node, head, link) {
		kids[i].node = node;
		kids[i].x = node->x;
		kids[i].y = node->y;
		i++;
	}

	view->open_anim.running = true;
	view->open_anim.start_us = now_us();

	/*
	 * Hide the whole window BEFORE anything else. Without this there is
	 * a one-frame flash: the map handlers above have already added and
	 * enabled every node at full opacity, so the next output frame can
	 * render the settled window before the animation's first tick.
	 * The first tick enables the tree again with the fade already
	 * applied, making an unanimated first frame impossible.
	 */
	wlr_scene_node_set_enabled(&view->scene_tree->node, false);

	/* Lift everything into a private wrapper tree we fully control.
	 * The wrapper starts at (0, 0): children keep their coordinates and
	 * the slide offset is applied to the wrapper itself. */
	struct wlr_scene_tree *wrapper =
		wlr_scene_tree_create(view->scene_tree);
	for (i = 0; i < count; i++) {
		wlr_scene_node_reparent(kids[i].node, wrapper);
	}

	view->open_anim.wrapper = wrapper;
	wl_array_init(&view->open_anim.children);
	wl_array_copy(&view->open_anim.children, &(struct wl_array){
		.data = kids, .size = count * sizeof(*kids),
	});
	free(kids);

	view->open_anim.timer = wl_event_loop_add_timer(server.wl_event_loop,
		tick, view);
	if (view->open_anim.timer) {
		wl_event_source_timer_update(view->open_anim.timer, 1);
	} else {
		animation_cancel_open(view);
	}
}

void
animation_cancel_open(struct view *view)
{
	if (!view || !view->open_anim.running) {
		return;
	}
	view->open_anim.running = false;

	wlr_log(WLR_DEBUG, "open animation ended after %ld ms",
		(long)(now_us() - view->open_anim.start_us) / 1000);

	if (view->open_anim.timer) {
		wl_event_source_remove(view->open_anim.timer);
		view->open_anim.timer = NULL;
	}

	/* Covers cancels that happen before the first tick ran */
	wlr_scene_node_set_enabled(&view->scene_tree->node, true);

	restore_children(view);
	set_subtree_opacity(&view->scene_tree->node, 1.0f);
}
