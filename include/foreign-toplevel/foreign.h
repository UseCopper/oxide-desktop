/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_FOREIGN_TOPLEVEL_H
#define OXIDE_DESKTOP_FOREIGN_TOPLEVEL_H

struct view;
struct foreign_toplevel;

struct foreign_toplevel *foreign_toplevel_create(struct view *view);
void foreign_toplevel_set_parent(struct foreign_toplevel *toplevel,
	struct foreign_toplevel *parent);
void foreign_toplevel_destroy(struct foreign_toplevel *toplevel);

#endif /* OXIDE_DESKTOP_FOREIGN_TOPLEVEL_H */
