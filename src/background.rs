//! The wallpaper: one image, drawn under everything, covering each output.
//!
//! There is no desktop surface to put a picture on, so the picture is the
//! bottom of every output's element list: the compositor draws it first, in the
//! output's own coordinates, and the windows go over it.
//!
//! Where the image comes from is a path, checked in the order a user would
//! expect: an explicit `OXIDE_WALLPAPER`, then the compositor's config
//! directory, then its data directory. Nothing found means nothing drawn, and
//! the clear colour stays the black it has always been.

use std::path::PathBuf;
use std::sync::OnceLock;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem, Renderer,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
        },
    },
    utils::{Buffer, Logical, Point, Rectangle, Size, Transform},
};

/// The image, decoded once and kept for the life of the process.
///
/// Loading is lazy because it needs no renderer: the bytes are decoded on the
/// CPU and handed to the GPU as a memory buffer on first draw. A wallpaper that
/// is missing or unreadable is `None`, which is also the answer for the rest of
/// the process — a file that was not there at startup is not going to be
/// re-checked every frame.
static WALLPAPER: OnceLock<Option<Wallpaper>> = OnceLock::new();

struct Wallpaper {
    buffer: MemoryRenderBuffer,
    size: Size<i32, Buffer>,
}

/// The wallpaper element for one output, cover-scaled to fill it.
///
/// `output` is the output's logical size — what the element is measured in. The
/// image is cropped to the output's shape and stretched over it, so the picture
/// fills the screen without distortion.
pub fn element<R>(
    renderer: &mut R,
    output: Size<i32, Logical>,
) -> Option<MemoryRenderBufferRenderElement<R>>
where
    R: Renderer + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let wallpaper = wallpaper()?;
    let (src, size) = cover(wallpaper.size, output);
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        Point::from((0.0, 0.0)),
        &wallpaper.buffer,
        None,
        Some(src),
        Some(size),
        Kind::Unspecified,
    )
    .ok()
}

/// The wallpaper, decoded on first use.
fn wallpaper() -> Option<&'static Wallpaper> {
    WALLPAPER.get_or_init(load).as_ref()
}

fn load() -> Option<Wallpaper> {
    let path = image_path()?;
    let image = match image::open(&path) {
        Ok(image) => image,
        Err(err) => {
            tracing::warn!(?path, ?err, "wallpaper could not be read");
            return None;
        }
    };
    let rgba = image.to_rgba8();
    let size = Size::from((rgba.width() as i32, rgba.height() as i32));
    if size.w <= 0 || size.h <= 0 {
        return None;
    }
    // `Abgr8888` is the format whose bytes are R, G, B, A in memory, which is
    // what `to_rgba8` hands over. (`Argb8888` is the same word the other way
    // round, and would swap the red and blue channels.)
    let buffer = MemoryRenderBuffer::from_slice(
        rgba.as_raw(),
        Fourcc::Abgr8888,
        size,
        1,
        Transform::Normal,
        None,
    );
    tracing::info!(?path, "wallpaper loaded");
    Some(Wallpaper { buffer, size })
}

/// Where the wallpaper is looked for, in order, ending at nothing.
fn image_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("OXIDE_WALLPAPER") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
        tracing::warn!(?path, "OXIDE_WALLPAPER is not a file");
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let mut candidates = Vec::new();
    for dir in [Some(config), data].into_iter().flatten() {
        for name in ["wallpaper", "background"] {
            for extension in ["png", "jpg", "jpeg"] {
                candidates.push(dir.join("oxide").join(format!("{name}.{extension}")));
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// How much of the image a covered output shows, and the size to draw it at.
///
/// Cover rather than fit: the shorter side of the image decides the scale, so
/// the crop is the part of the image that overflows — centred, like a photo
/// cropped to a frame — and the whole output is filled. Returns the source
/// rectangle in the image's own pixels and the size to draw it at (the output's
/// own size, since that is what it covers). Separated from the widget work so
/// the arithmetic can be tested without a display.
fn cover(
    image: Size<i32, Buffer>,
    output: Size<i32, Logical>,
) -> (Rectangle<f64, Logical>, Size<i32, Logical>) {
    let whole = Rectangle::<f64, Logical>::new(
        (0.0, 0.0).into(),
        (image.w as f64, image.h as f64).into(),
    );
    if image.w <= 0 || image.h <= 0 || output.w <= 0 || output.h <= 0 {
        // Nothing sensible to scale: hand back the whole image and the output,
        // and let the renderer decide what that looks like.
        return (whole, output);
    }
    let scale = (output.w as f64 / image.w as f64).max(output.h as f64 / image.h as f64);
    let crop_w = (output.w as f64 / scale).min(image.w as f64);
    let crop_h = (output.h as f64 / scale).min(image.h as f64);
    let src = Rectangle::new(
        (
            (image.w as f64 - crop_w) / 2.0,
            (image.h as f64 - crop_h) / 2.0,
        )
            .into(),
        (crop_w, crop_h).into(),
    );
    (src, output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wallpaper_covers_the_output_without_distorting_it() {
        // A square image on a wide output: the image is scaled by width, so the
        // top and bottom overflow and the crop is the middle band.
        let image = Size::from((1000, 1000));
        let output = Size::from((800, 400));
        let (src, size) = cover(image, output);
        assert_eq!(size, output, "the crop is drawn at the output's size");
        assert_eq!(src.size, Size::from((1000.0, 500.0)));
        assert_eq!(src.loc, (0.0, 250.0).into(), "the crop is centred");

        // The crop's shape is the output's, which is what makes the stretch
        // invisible: a source with the image's shape would distort.
        let shape = src.size.w / src.size.h;
        let want = output.w as f64 / output.h as f64;
        assert!((shape - want).abs() < 1e-9, "{shape} is not {want}");

        // A tall image on a square output: the same, the other way round.
        let image = Size::from((600, 1200));
        let output = Size::from((900, 900));
        let (src, size) = cover(image, output);
        assert_eq!(size, output);
        // Scaled by width (900/600 = 1.5), so the crop is 600x600 of the height.
        assert_eq!(src.size, Size::from((600.0, 600.0)));
        assert_eq!(src.loc, (0.0, 300.0).into());

        // An image smaller than the output is scaled up to fill it, and cropped
        // to the output's shape like any other: 320x200 on 16:9 loses 10
        // pixels off the top and bottom.
        let image = Size::from((320, 200));
        let output = Size::from((1920, 1080));
        let (src, size) = cover(image, output);
        assert_eq!(size, output);
        assert_eq!(src.size, Size::from((320.0, 180.0)));
        assert_eq!(src.loc, (0.0, 10.0).into());

        // An image whose shape is the output's needs no crop at all, whatever
        // the sizes are.
        let image = Size::from((1920, 1080));
        let output = Size::from((960, 540));
        let (src, size) = cover(image, output);
        assert_eq!(size, output);
        assert_eq!(src.size, Size::from((1920.0, 1080.0)));
        assert_eq!(src.loc, (0.0, 0.0).into());
    }

    #[test]
    fn a_degenerate_wallpaper_is_handed_back_whole() {
        // A zero-sized image or output cannot be scaled, and dividing by it
        // would be a NaN that tints the whole screen.
        let (src, size) = cover(Size::from((0, 0)), Size::from((100, 100)));
        assert_eq!(src.size, Size::from((0.0, 0.0)));
        assert_eq!(size, Size::from((100, 100)));

        let (src, size) = cover(Size::from((100, 50)), Size::from((0, 0)));
        assert_eq!(src.size, Size::from((100.0, 50.0)));
        assert_eq!(src.loc, (0.0, 0.0).into());
        assert_eq!(size, Size::from((0, 0)));
    }

    #[test]
    fn the_wallpaper_is_looked_for_where_a_user_would_put_it() {
        // The explicit path wins, and is what a test can be sure of.
        let dir = std::env::temp_dir().join(format!("oxide-wallpaper-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("mine.png");
        std::fs::write(&file, b"not an image, but a file").expect("temp file");
        // SAFETY: single-threaded test, and the variable is read once.
        unsafe { std::env::set_var("OXIDE_WALLPAPER", &file) };
        assert_eq!(image_path(), Some(file.clone()));
        // A path that is not there falls through to the directories rather than
        // being trusted.
        unsafe { std::env::set_var("OXIDE_WALLPAPER", dir.join("absent.png")) };
        let found = image_path();
        assert!(
            found.is_none() || found.as_deref() != Some(file.as_path()),
            "a missing OXIDE_WALLPAPER was used anyway"
        );
        unsafe { std::env::remove_var("OXIDE_WALLPAPER") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
