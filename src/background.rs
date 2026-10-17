//! The wallpaper: one image, drawn under everything, covering each output.
//!
//! There is no desktop surface to put a picture on, so the picture is the
//! bottom of every output's element list: the compositor draws it last, in the
//! output's own coordinates, with the windows and layers over it. (Those lists
//! are front-to-back — the cursor is first and topmost — so the back is the
//! end.)
//!
//! Where the image comes from is a path, checked in the order a user would
//! expect: an explicit `OXIDE_WALLPAPER`, then the compositor's config
//! directory, then its data directory. Nothing found means nothing drawn, and
//! the clear colour stays the black it has always been.
//!
//! Decoding happens on its own thread, at at most the size of the output it
//! will be shown on. A wallpaper is a big image — a full decode plus two copies
//! of the pixels is easily a second or two — and doing that inside a render
//! pass stalls the whole desktop for it. Until it lands the desktop is black,
//! which is no worse than it was before there was a wallpaper at all.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

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
    utils::{Buffer, Logical, Physical, Point, Rectangle, Size, Transform},
};

/// How long the wallpaper takes to settle in: coming up from nothing while
/// growing to fill the output. Long enough to be a desktop arriving, short
/// enough not to be a wait.
const BACKGROUND_FADE: Duration = Duration::from_millis(250);
/// Where the wallpaper starts, as a fraction of the output it fills. Small
/// enough to read as growing, large enough that the picture is recognisable
/// from the first frame.
const BACKGROUND_START_SCALE: f64 = 0.85;
/// When the wallpaper was first drawn, which is when its arrival starts.
static APPEARED: OnceLock<Instant> = OnceLock::new();

/// The image, decoded at most once, off the compositor's thread.
static WALLPAPER: OnceLock<Mutex<Option<Wallpaper>>> = OnceLock::new();
/// Whether that thread has been started. Separate from the cell above so the
/// thread is spawned exactly once, by whichever frame gets there first.
static STARTED: Once = Once::new();

/// The decoded image and its size. Cheap to clone: the pixels are shared.
#[derive(Clone)]
struct Wallpaper {
    buffer: Arc<MemoryRenderBuffer>,
    size: Size<i32, Buffer>,
}

/// The wallpaper element for one output, cover-scaled to fill it.
///
/// `output` is the output's logical size — what the element is measured in —
/// `scale` its fractional scale, and `max_physical` the most pixels it will ever
/// be drawn at, which is what the image is decoded down to. The image is
/// cropped to the output's shape and stretched over it, so the picture fills the
/// screen without distortion.
///
/// For the first [`BACKGROUND_FADE`] after the image first appears it comes up
/// from nothing while growing from [`BACKGROUND_START_SCALE`] of the output to
/// all of it, centred — the desktop arriving rather than blinking on. Both run
/// on the same ease-out, and both are finished inside the animation's clock, so
/// a frame that arrives late is simply further along.
pub fn element<R>(
    renderer: &mut R,
    output: Size<i32, Logical>,
    scale: f64,
    max_physical: Size<i32, Physical>,
) -> Option<MemoryRenderBufferRenderElement<R>>
where
    R: Renderer + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let wallpaper = wallpaper(max_physical)?;
    let (src, full) = cover(wallpaper.size, output);

    // The appearance, timed from the first frame that has an image to draw. A
    // wallpaper that takes a second to decode should not have spent its fade
    // waiting for itself — and the frame that starts it is the beginning of the
    // animation, not its end.
    let appeared = *APPEARED.get_or_init(Instant::now);
    let elapsed = appeared.elapsed().as_secs_f64();
    let t = (elapsed / BACKGROUND_FADE.as_secs_f64()).clamp(0.0, 1.0);
    let ease = crate::shell::animation::ease_out_cubic(t);
    let grow = BACKGROUND_START_SCALE + (1.0 - BACKGROUND_START_SCALE) * ease;

    // The crop is the whole output's, drawn smaller and centred: the picture
    // grows into the screen rather than being re-cropped as it goes, so nothing
    // inside it shifts while it arrives.
    let size = Size::from((
        (full.w as f64 * grow).round() as i32,
        (full.h as f64 * grow).round() as i32,
    ));
    let drawn: Size<i32, Physical> = Size::from((
        (size.w as f64 * scale).round() as i32,
        (size.h as f64 * scale).round() as i32,
    ));
    let location = Point::from((
        (max_physical.w - drawn.w) as f64 / 2.0,
        (max_physical.h - drawn.h) as f64 / 2.0,
    ));

    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        location,
        &wallpaper.buffer,
        Some(ease as f32),
        Some(src),
        Some(size),
        Kind::Unspecified,
    )
    .ok()
}

/// The wallpaper, if it has finished decoding. Starts the decode the first time
/// it is asked.
fn wallpaper(cap: Size<i32, Physical>) -> Option<Wallpaper> {
    let cell = WALLPAPER.get_or_init(|| Mutex::new(None));
    STARTED.call_once(|| {
        // At least one pixel, so a not-yet-configured output cannot ask for a
        // zero-sized image.
        let cap = Size::from((cap.w.max(1) as u32, cap.h.max(1) as u32));
        std::thread::spawn(move || {
            let loaded = load(cap);
            match cell.lock() {
                Ok(mut slot) => *slot = loaded,
                Err(_) => tracing::warn!("wallpaper lock was poisoned"),
            }
        });
    });
    cell.lock().ok().and_then(|slot| slot.clone())
}

fn load(cap: Size<u32, Physical>) -> Option<Wallpaper> {
    let path = image_path()?;
    let image = match image::open(&path) {
        Ok(image) => image,
        Err(err) => {
            tracing::warn!(?path, ?err, "wallpaper could not be read");
            return None;
        }
    };
    // Down to the size it is drawn at: a wallpaper is usually much larger than
    // the screen, and the pixels that get cropped away were never going to be
    // seen. `thumbnail` keeps the aspect, so the cover crop below still works.
    let image = if image.width() > cap.w || image.height() > cap.h {
        image.thumbnail(cap.w, cap.h)
    } else {
        image
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
    Some(Wallpaper {
        buffer: Arc::new(buffer),
        size,
    })
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
