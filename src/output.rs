//! What mode an output runs at.
//!
//! Both kinds of output arrive with a list of modes to choose from and no instruction about
//! which one to use: a real monitor's EDID modes, and the video modes a nested window is
//! offered. The rule is the same for both, so it lives once, here — the most pixels first,
//! because that is what a display is for, and the highest refresh among the modes of that
//! size second, because a monitor that offers 1440p at 60 and at 144 should run at 144.
//!
//! What neither list is: ordered. A mode list's first entry is often the smallest one, and
//! the flag that marks a monitor's own preference — `PREFERRED` in an EDID — is the panel's
//! idea of its best rather than the best of what it offers, so it is frequently neither the
//! largest nor the fastest.

/// The best of a set of modes, as an index into `modes`.
///
/// Each mode is its pixel count and its refresh rate: `(width * height, millihertz)`. The
/// refresh's unit does not matter as long as every mode is in the same one, and millihertz
/// is what the nested backends report.
///
/// Ties keep the earliest mode, so the same list always answers the same thing. A list with
/// nothing in it answers nothing, rather than an index into it.
pub fn best_mode(modes: &[(u64, u32)]) -> Option<usize> {
    let mut best: Option<(usize, (u64, u32))> = None;
    for (index, mode) in modes.iter().enumerate() {
        if best.is_none_or(|(_, best_mode)| *mode > best_mode) {
            best = Some((index, *mode));
        }
    }
    best.map(|(index, _)| index)
}

/// How many pixels a mode has: the first half of what [`best_mode`] compares.
pub fn pixels(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height)
}

/// A refresh rate in millihertz from one in the whole hertz a monitor's modes are listed in.
pub fn millihertz(refresh_hz: u32) -> u32 {
    refresh_hz.saturating_mul(1_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_biggest_and_fastest_mode_wins() {
        // 1080p@60, 1440p@60, 1440p@144, 720p@60: the biggest, and the fastest of those, so
        // a panel's 144Hz is used rather than the rate its EDID prefers.
        let modes = [
            (pixels(1920, 1080), millihertz(60)),
            (pixels(2560, 1440), millihertz(60)),
            (pixels(2560, 1440), millihertz(144)),
            (pixels(1280, 720), millihertz(60)),
        ];
        assert_eq!(best_mode(&modes), Some(2));

        // Resolution before refresh: a 4K panel at 60 beats a 1080p one at 240.
        let modes = [
            (pixels(4096, 2160), millihertz(60)),
            (pixels(1920, 1080), millihertz(240)),
        ];
        assert_eq!(best_mode(&modes), Some(0));

        // The list is in no order: its first entry is often the smallest.
        let modes = [
            (pixels(640, 480), millihertz(60)),
            (pixels(1920, 1080), millihertz(60)),
        ];
        assert_eq!(best_mode(&modes), Some(1));

        // Ties keep the first, so one list always answers the same thing.
        let modes = [
            (pixels(1920, 1080), millihertz(60)),
            (pixels(1920, 1080), millihertz(60)),
        ];
        assert_eq!(best_mode(&modes), Some(0));

        // A display that offers nothing offers nothing to pick.
        assert_eq!(best_mode(&[]), None);
    }
}
