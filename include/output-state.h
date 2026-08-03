/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_OUTPUT_STATE_H
#define OXIDE_DESKTOP_OUTPUT_STATE_H

#include <stdbool.h>

struct output;

void output_state_init(struct output *output);

bool output_state_commit(struct output *output);

#endif // OXIDE_DESKTOP_OUTPUT_STATE_H
