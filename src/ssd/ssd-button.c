// SPDX-License-Identifier: GPL-2.0-only

#include <assert.h>
#include <wlr/types/wlr_scene.h>
#include "config/rcxml.h"
#include "common/list.h"
#include "common/mem.h"
#include "common/scene-helpers.h"
#include "node.h"
#include "scaled-buffer/scaled-icon-buffer.h"
#include "scaled-buffer/scaled-img-buffer.h"
#include "img/img.h"
#include "ssd.h"
#include "ssd-internal.h"

/* Internal API */

struct ssd_button *
attach_ssd_button(struct wl_list *button_parts, enum lab_node_type type,
		struct wlr_scene_tree *parent,
		struct lab_img *imgs[LAB_BS_ALL + 1],
		int x, int y, struct view *view)
{
	struct wlr_scene_tree *root = lab_wlr_scene_tree_create(parent);
	wlr_scene_node_set_position(&root->node, x, y);

	assert(node_type_contains(LAB_NODE_BUTTON, type));
	struct ssd_button *button = znew(*button);
	button->node = &root->node;
	button->type = type;
	node_descriptor_create(&root->node, type, view, button);
	wl_list_append(button_parts, &button->link);

	/* Hitbox */
	float invisible[4] = { 0, 0, 0, 0 };
	lab_wlr_scene_rect_create(root, rc.theme->window_button_width,
		rc.theme->window_button_height, invisible);

	/* Icons */
	int button_width = rc.theme->window_button_width;
	int button_height = rc.theme->window_button_height;
	/*
	 * Ensure a small amount of horizontal padding within the button
	 * area (2px on each side with the default 26px button width).
	 * A new theme setting could be added to configure this. Using
	 * an existing setting (padding.width or window.button.spacing)
	 * was considered, but these settings have distinct purposes
	 * already and are zero by default.
	 */
	int icon_padding = button_width / 10;

	if (type == LAB_NODE_BUTTON_WINDOW_ICON) {
		struct scaled_icon_buffer *icon_buffer =
			scaled_icon_buffer_create(root, button_width
				- 2 * icon_padding, button_height);
		assert(icon_buffer);
		struct wlr_scene_node *icon_node = &icon_buffer->scene_buffer->node;
		scaled_icon_buffer_set_view(icon_buffer, view);
		wlr_scene_node_set_position(icon_node, icon_padding, 0);
		button->window_icon = icon_buffer;
	} else {
		for (uint8_t state_set = LAB_BS_DEFAULT;
				state_set <= LAB_BS_ALL; state_set++) {
			if (!imgs[state_set]) {
				continue;
			}
			int width = button_width;
			int height = button_height;
			int off_x = 0, off_y = 0;
			int art_w, art_h;
			/*
			 * Render hand-authored icons 1:1 whenever they already
			 * fit the hitbox, so xbm/png glyphs are never resampled
			 * and keep their baked-in padding. Artwork larger than
			 * the hitbox keeps the legacy scale-to-fit behavior.
			 */
			if (lab_img_get_size(imgs[state_set], &art_w, &art_h)
					&& art_w <= button_width
					&& art_h <= button_height) {
				width = art_w;
				height = art_h;
				off_x = (button_width - art_w) / 2;
				off_y = (button_height - art_h) / 2;
			}
			struct scaled_img_buffer *img_buffer =
				scaled_img_buffer_create(root, imgs[state_set],
					width, height);
			assert(img_buffer);
			struct wlr_scene_node *icon_node = &img_buffer->scene_buffer->node;
			wlr_scene_node_set_position(icon_node, off_x, off_y);
			wlr_scene_node_set_enabled(icon_node, false);
			button->img_buffers[state_set] = img_buffer;
		}
		/* Initially show non-hover, non-toggled, unrounded variant */
		wlr_scene_node_set_enabled(
			&button->img_buffers[LAB_BS_DEFAULT]->scene_buffer->node, true);
	}

	return button;
}

/* called from node descriptor destroy */
void ssd_button_free(struct ssd_button *button)
{
	wl_list_remove(&button->link);
	free(button);
}
