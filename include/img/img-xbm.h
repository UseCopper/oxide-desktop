/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef OXIDE_DESKTOP_IMG_XBM_H
#define OXIDE_DESKTOP_IMG_XBM_H

struct lab_data_buffer;

/**
 * img_xbm_load_from_bitmap() - create button from monochrome bitmap
 * @bitmap: bitmap data array in hexadecimal xbm format
 * @rgba: color
 *
 * Example bitmap: char button[6] = { 0x3f, 0x3f, 0x21, 0x21, 0x21, 0x3f };
 */
struct lab_data_buffer *img_xbm_load_from_bitmap(const char *bitmap, float *rgba);

/* img_xbm_load - Convert xbm file to buffer with cairo surface */
struct lab_data_buffer *img_xbm_load(const char *filename, float *rgba);

#endif /* OXIDE_DESKTOP_IMG_XBM_H */
