/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_MATCH_H
#define OXIDE_DESKTOP_MATCH_H

#include <stdbool.h>

/**
 * match_glob() - Pattern match using shell wildcard rules (see glob(7))
 * @pattern: Pattern to match against.
 * @string: String to search.
 * Note: Comparison case-insensitive.
 */
bool match_glob(const char *pattern, const char *string);

#endif /* OXIDE_DESKTOP_MATCH_H */
