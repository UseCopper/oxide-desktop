#!/bin/sh

: ${BUILD_DIR="build"}

[ -d "${BUILD_DIR}" ] || meson setup "${BUILD_DIR}"
meson configure -Db_sanitize=address,undefined "${BUILD_DIR}"
ninja -C "${BUILD_DIR}"
LSAN_OPTIONS=suppressions=scripts/asan_leak_suppressions "${BUILD_DIR}/oxide-desktop" -d 2>log.txt

