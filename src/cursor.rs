use std::{io::Read, time::Duration};

use tracing::warn;
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../resources/cursor.rgba");

pub struct Cursor {
    icons: Vec<Image>,
    size: u32,
}

impl Cursor {
    pub fn load() -> Cursor {
        let name = std::env::var("XCURSOR_THEME")
            .ok()
            .unwrap_or_else(|| "default".into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(24);

        let theme = CursorTheme::load(&name);
        let icons = load_icon(&theme)
            .map_err(|err| warn!("Unable to load xcursor: {}, using fallback cursor", err))
            .unwrap_or_else(|_| {
                vec![Image {
                    size: 32,
                    width: 64,
                    height: 64,
                    xhot: 1,
                    yhot: 1,
                    delay: 1,
                    pixels_rgba: Vec::from(FALLBACK_CURSOR_DATA),
                    pixels_argb: vec![], //unused
                }]
            });

        Cursor { icons, size }
    }

    pub fn get_image(&self, scale: u32, time: Duration) -> Image {
        let size = self.size * scale;
        frame(time.as_millis() as u32, size, &self.icons)
    }
}

fn nearest_images(size: u32, images: &[Image]) -> impl Iterator<Item = &Image> {
    // Follow the nominal size of the cursor to choose the nearest.
    // Empty theme → no images; callers handle the empty case.
    let nearest_dims = images
        .iter()
        .min_by_key(|image| (size as i32 - image.size as i32).abs())
        .map(|image| (image.width, image.height));

    images.iter().filter(move |image| {
        nearest_dims.is_some_and(|(w, h)| image.width == w && image.height == h)
    })
}

fn frame(mut millis: u32, size: u32, images: &[Image]) -> Image {
    let mut nearest = nearest_images(size, images);
    let Some(first) = nearest.next() else {
        // Defensive fallback: 1x1 transparent pixel so we never panic on
        // malformed/empty themes; Cursor::load already falls back, but
        // animated themes could still end up empty here.
        return Image {
            size,
            width: 1,
            height: 1,
            xhot: 0,
            yhot: 0,
            delay: 1,
            pixels_rgba: vec![0, 0, 0, 0],
            pixels_argb: vec![],
        };
    };
    let total: u32 = std::iter::once(first)
        .chain(nearest)
        .fold(0, |acc, image| acc.saturating_add(image.delay));
    if total == 0 {
        return first.clone();
    }
    millis %= total;

    for img in std::iter::once(first).chain(nearest_images(size, images).skip(1)) {
        if millis < img.delay {
            return img.clone();
        }
        millis -= img.delay;
    }

    first.clone()
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Theme has no default cursor")]
    NoDefaultCursor,
    #[error("Error opening xcursor file: {0}")]
    File(#[from] std::io::Error),
    #[error("Failed to parse XCursor file")]
    Parse,
}

fn load_icon(theme: &CursorTheme) -> Result<Vec<Image>, Error> {
    let icon_path = theme.load_icon("default").ok_or(Error::NoDefaultCursor)?;
    let mut cursor_file = std::fs::File::open(icon_path)?;
    let mut cursor_data = Vec::new();
    cursor_file.read_to_end(&mut cursor_data)?;
    parse_xcursor(&cursor_data).ok_or(Error::Parse)
}
