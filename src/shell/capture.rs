//! Window pixel capture.
//!
//! Three things need a window's pixels off the GPU:
//!
//! * an app-triggered close, which keeps drawing the window from the last
//!   committed buffers after the client is gone (see [`capture_surface_tree`]);
//! * a maximize/unmaximize crossfade, which freezes the pre-transition frame so
//!   it can fade into the live content (see [`capture_window_snapshots`]);
//! * the panel's window picker, which sends a scaled-down preview of each window
//!   over the IPC socket (see [`capture_thumbnail`]).

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ExportMem, ImportAll, ImportMem, Offscreen, Renderer, Texture,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, memory::MemoryRenderBuffer,
                surface::WaylandSurfaceRenderElement,
                utils::CropRenderElement,
            },
            gles::GlesTexture,
            utils::{RendererSurfaceStateUserData},
        },
    },
    desktop::Space,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::{
        compositor::{TraversalAction, with_states, with_surface_tree_downward},
        shell::xdg::SurfaceCachedState,
    },
};

use super::{
    WindowElement,
    ssd::{LastFrame, SurfaceFrame},
};

/// Capture the pre-transition pixels of every window that is waiting for a
/// snapshot, so its animation can crossfade from it. Must be called with the
/// output framebuffer unbound, before rendering the frame.
pub fn capture_window_snapshots<R>(space: &Space<WindowElement>, renderer: &mut R)
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let windows: Vec<WindowElement> = space.elements().cloned().collect();
    for window in windows {
        if window.is_ghosting() {
            continue;
        }

        let pending = window
            .decoration_state()
            .animation
            .as_ref()
            .map(|animation| animation.needs_snapshot())
            .unwrap_or(false);
        if pending {
            let content = window.resize_content_size();
            if content.w > 0 && content.h > 0 {
                let size = Size::<i32, Buffer>::from((content.w, content.h));
                if let Some(buffer) = capture_window_content(renderer, &window, size) {
                    window
                        .decoration_state()
                        .animation
                        .as_mut()
                        .unwrap()
                        .set_snapshot(buffer, content);
                } else {
                    window
                        .decoration_state()
                        .animation
                        .as_mut()
                        .unwrap()
                        .set_snapshot_unavailable();
                }
            } else {
                window
                    .decoration_state()
                    .animation
                    .as_mut()
                    .unwrap()
                    .set_snapshot_unavailable();
            }
        }
    }
}

/// Walk a window's surface tree and keep every mapped surface's buffer, with its
/// position relative to the window's top-left. Holding the buffers keeps the
/// pixels alive after the client is gone, so an app-triggered close can render
/// the whole window (content and subsurfaces).
pub fn capture_surface_tree(root: &WlSurface) -> Option<LastFrame> {
    let mut surfaces: Vec<SurfaceFrame> = Vec::new();

    with_surface_tree_downward(
        root,
        Point::<f64, Logical>::from((0.0, 0.0)),
        |_, states, location| {
            let mut location = *location;
            if let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() {
                if let Some(view) = data.lock().unwrap().view() {
                    location += view.offset.to_f64();
                    TraversalAction::DoChildren(location)
                } else {
                    TraversalAction::SkipChildren
                }
            } else {
                TraversalAction::SkipChildren
            }
        },
        |_, states, location| {
            let mut location = *location;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return;
            };
            let data = data.lock().unwrap();
            let Some(view) = data.view() else {
                return;
            };
            location += view.offset.to_f64();
            if let Some(buffer) = data.buffer() {
                surfaces.push(SurfaceFrame {
                    buffer: buffer.clone(),
                    location: Point::from((
                        location.x.round() as i32,
                        location.y.round() as i32,
                    )),
                    size: view.dst,
                    scale: data.buffer_scale(),
                    transform: data.buffer_transform(),
                    src: Some(view.src),
                });
            }
        },
        |_, _, _| true,
    );

    (!surfaces.is_empty()).then_some(LastFrame {
        geometry: with_states(root, |states| {
            states.cached_state.get::<SurfaceCachedState>().current().geometry
        })
        .unwrap_or_else(|| {
            Rectangle::from_size(surfaces.first().map(|s| s.size).unwrap_or_default())
        }),
        surfaces,
    })
}

/// Render a window's client content into an offscreen buffer and read it back
/// as a [`MemoryRenderBuffer`]. Returns `None` if the renderer cannot provide
/// a readable offscreen target.
fn capture_window_content<R>(
    renderer: &mut R,
    window: &WindowElement,
    size: Size<i32, Buffer>,
) -> Option<MemoryRenderBuffer>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let source = capture_source(window);
    let (data, size) = render_window_content(renderer, window, source, size)?;
    Some(MemoryRenderBuffer::from_slice(
        &data,
        Fourcc::Abgr8888,
        size,
        1,
        Transform::Normal,
        None,
    ))
}

/// The factor mapping a captured region onto the buffer it is rendered into.
///
/// `None` for an empty region, which cannot be scaled.
fn capture_scale(source: Size<i32, Physical>, output: Size<i32, Buffer>) -> Option<f64> {
    (source.w > 0 && source.h > 0 && output.w > 0).then(|| f64::from(output.w) / f64::from(source.w))
}

/// Where the render elements have to be placed so that `source` lands at the
/// buffer's origin, given the scale they are being drawn at.
fn capture_origin(source: Rectangle<i32, Physical>, factor: f64) -> Point<i32, Physical> {
    Point::from((
        -((f64::from(source.loc.x) * factor).round() as i32),
        -((f64::from(source.loc.y) * factor).round() as i32),
    ))
}

/// The region to cut out of the render elements, once they have been relocated by
/// [`capture_origin`].
///
/// Anchored at the origin, because that relocation is exactly what brings the
/// captured region there. `CropRenderElement` intersects the crop against
/// `element.geometry(scale)`, so a crop still carrying the region's original
/// offset no longer meets the element it is meant to cut and the intersection
/// comes back short — which renders as a crop rather than a centred view. The
/// crop and the relocation have to move together.
fn capture_crop(source: Rectangle<i32, Physical>, factor: f64) -> Rectangle<i32, Physical> {
    Rectangle::from_size(Size::from((
        (f64::from(source.size.w) * factor).round() as i32,
        (f64::from(source.size.h) * factor).round() as i32,
    )))
}

/// The region of a window to capture, in buffer pixels: what the client says it
/// is drawing, or the whole surface when it has said nothing.
///
/// A client-decorated surface is usually larger than its content — GTK and
/// Firefox reserve a margin for their own shadows — so capturing the surface
/// whole would scale that padding in as well, leaving the content shrunken and
/// off-centre behind a transparent border.
fn capture_source(window: &WindowElement) -> Rectangle<i32, Physical> {
    let Some(root) = window.wl_surface() else {
        return content_fallback(window, 1);
    };
    // The declared geometry is in logical coordinates, while render elements are
    // laid out in buffer pixels, so it has to be converted at the surface's
    // buffer scale. Treating it as already being in pixels would crop the wrong
    // region entirely on a scaled display.
    let (geometry, buffer_scale) = with_states(&root, |states| {
        let scale = states
            .data_map
            .get::<RendererSurfaceStateUserData>()
            .and_then(|data| data.lock().ok().map(|data| data.buffer_scale()))
            .unwrap_or(1);
        (states.cached_state.get::<SurfaceCachedState>().current().geometry, scale)
    });
    declared_content_rect(geometry, buffer_scale).unwrap_or_else(|| content_fallback(window, buffer_scale))
}

/// The client's declared content, converted from logical to buffer pixels.
fn declared_content_rect(
    geometry: Option<Rectangle<i32, Logical>>,
    buffer_scale: i32,
) -> Option<Rectangle<i32, Physical>> {
    let rect = geometry?;
    (!rect.is_empty() && buffer_scale > 0).then(|| rect.to_physical(Scale::from(buffer_scale)))
}

/// The window's content size when the client declared no geometry of its own.
fn content_fallback(window: &WindowElement, buffer_scale: i32) -> Rectangle<i32, Physical> {
    let content = window.resize_content_size();
    let scale = Scale::from(buffer_scale.max(1));
    Rectangle::from_size(content.to_physical(scale))
}

/// Render a window's client content at exactly `size` and read it back as
/// `Abgr8888` rows (R, G, B, A per pixel, top-down).
///
/// `Abgr8888` is GL's `RGBA8`, and Smithay's GL renderer flips y while drawing
/// into a `Normal` target, so the readback needs no swizzle or flip to become
/// R, G, B, A top-down — which is also what `GdkMemoryFormat::R8g8b8a8` wants.
fn render_window_content<R>(
    renderer: &mut R,
    window: &WindowElement,
    source: Rectangle<i32, Physical>,
    size: Size<i32, Buffer>,
) -> Option<(Vec<u8>, Size<i32, Buffer>)>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + Send + 'static,
{
    let mut target = renderer.create_buffer(Fourcc::Abgr8888, size).ok()?;
    let mut framebuffer = renderer.bind(&mut target).ok()?;

    // The scale the elements are rendered at. This is the load-bearing part: the
    // damage tracker draws each element at *its own* scale and does not fit it
    // to the framebuffer, so rendering a window into a smaller buffer without
    // reducing the scale yields a crop of its corner at full resolution rather
    // than a smaller view of it.
    let Some(factor) = capture_scale(source.size, size) else {
        return None;
    };
    let scale = Scale::from(factor);
    // Pull the captured region to the origin. The elements are laid out from the
    // window's own origin, so a region that does not start there — which is every
    // client-decorated window, since its content is inset by the shadow margin —
    // would otherwise be drawn at that offset inside a buffer sized only for the
    // region, putting it off to one side and clipping the far edge.
    let origin = capture_origin(source, factor);
    let raw: Vec<WaylandSurfaceRenderElement<R>> =
        AsRenderElements::render_elements(&window.0, renderer, origin, scale, 1.0);

    // Cropped to the region being captured, expressed in the same scaled space
    // the elements were created in — `CropRenderElement` intersects the crop
    // against `element.geometry(scale)`, so the two have to agree.
    let crop = capture_crop(source, factor);
    let elements: Vec<CropRenderElement<WaylandSurfaceRenderElement<R>>> = raw
        .into_iter()
        .filter_map(|element| CropRenderElement::from_element(element, scale, crop))
        .collect();
    if elements.is_empty() {
        return None;
    }

    let physical_size = Size::<i32, Physical>::from((size.w, size.h));
    let mut tracker = OutputDamageTracker::new(physical_size, 1.0, Transform::Normal);
    tracker
        .render_output(renderer, &mut framebuffer, 0, &elements, [0.0, 0.0, 0.0, 0.0])
        .ok()?;

    let region = Rectangle::<i32, Buffer>::from_size(size);
    let mapping = renderer
        .copy_framebuffer(&framebuffer, region, Fourcc::Abgr8888)
        .ok()?;
    let data = renderer.map_texture(&mapping).ok()?;
    Some((data.to_vec(), size))
}

/// How much bigger than the thumbnail the intermediate readback is allowed to be,
/// in each dimension.
///
/// The renderer reduces with a single bilinear sample, which reconstructs
/// faithfully to roughly 2.5:1 and undersamples badly past that. Stopping the GPU
/// step at about this ratio and finishing with the area average below therefore
/// costs nothing visible, while the transfer and the averaging both shrink by the
/// square of the factor — for a 1080p window behind a 200px thumbnail that is
/// about 8 MB per readback reduced to 1.4 MB.
const READBACK_OVERSAMPLE: i32 = 2;

/// `OXIDE_THUMB_OVERSAMPLE` overrides how far the GPU reduces before the area
/// average takes over. Lower is cheaper and slightly softer, higher is the
/// reverse; see [`READBACK_OVERSAMPLE`].
fn readback_oversample() -> i32 {
    std::env::var("OXIDE_THUMB_OVERSAMPLE")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value >= 1)
        .unwrap_or(READBACK_OVERSAMPLE)
}

/// The size to read a window's content back at on the way to a `target` thumbnail:
/// the content, bounded so the GPU's one downscale step stays near 1:`READBACK_OVERSAMPLE`.
pub fn readback_size(content: Size<i32, Buffer>, target: Size<i32, Buffer>) -> Option<Size<i32, Buffer>> {
    let oversample = readback_oversample();
    let ceiling = Size::from((
        target.w.saturating_mul(oversample),
        target.h.saturating_mul(oversample),
    ));
    // `fit_size` keeps the aspect ratio and never upscales, so a window already
    // smaller than the ceiling is read back whole, as before.
    fit_size(content, ceiling)
}

/// The size a window's content scales to in order to fit inside `max` without
/// distortion. `None` for degenerate input.
///
/// Never upscales, which is what the other callers want: scaling a window up to
/// fill a box only makes it blurrier.
pub fn fit_size(content: Size<i32, Buffer>, max: Size<i32, Buffer>) -> Option<Size<i32, Buffer>> {
    fit_size_allowing(content, max, false)
}

/// As [`fit_size`], but a `upscale` content may grow to fill the box.
///
/// Used for the panel's previews, where a window smaller than the thumbnail
/// should still arrive filling it: rendering it small and letting the panel
/// stretch the result costs the same and looks worse.
pub fn fit_size_allowing(
    content: Size<i32, Buffer>,
    max: Size<i32, Buffer>,
    upscale: bool,
) -> Option<Size<i32, Buffer>> {
    if content.w <= 0 || content.h <= 0 || max.w <= 0 || max.h <= 0 {
        return None;
    }
    let mut scale = (f64::from(max.w) / f64::from(content.w))
        .min(f64::from(max.h) / f64::from(content.h));
    if !upscale {
        scale = scale.min(1.0);
    }
    Some(Size::from((
        ((f64::from(content.w) * scale).round() as i32).max(1),
        ((f64::from(content.h) * scale).round() as i32).max(1),
    )))
}

/// Area-average (`box filter`) an `Abgr8888` image down to `dst`.
///
/// Two separable passes, each averaging the exact source span that maps onto one
/// destination pixel. This is the same reconstruction a mip chain performs, and
/// it is why the result does not alias: every source pixel contributes to the
/// output in proportion to the area it covers.
///
/// A single bilinear sample would not do. Shrinking a 1920px surface to 200px
/// that way samples roughly one texel in ten and turns text into speckle, which
/// is the usual way a scaled-down preview ends up looking like noise.
///
/// `src` must be `src_size.w * src_size.h * 4` bytes. `src` and `dst` may not
/// overlap.
fn box_filter(src: &[u8], src_size: Size<i32, Buffer>, dst: Size<i32, Buffer>) -> Option<Vec<u8>> {
    let (sw, sh) = (src_size.w as usize, src_size.h as usize);
    let (dw, dh) = (dst.w as usize, dst.h as usize);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return None;
    }
    if src.len() < sw * sh * 4 {
        return None;
    }

    // Horizontal: src (sw x sh) -> tmp (dw x sh).
    let mut tmp = vec![0u8; dw * sh * 4];
    for y in 0..sh {
        let row = y * sw * 4;
        let out = y * dw * 4;
        for x in 0..dw {
            // Integer bounds keep the partition exact: the spans tile the row
            // with no overlap and no gap, so no pixel is double-counted.
            let x0 = x * sw / dw;
            let x1 = (((x + 1) * sw) / dw).max(x0 + 1);
            let count = (x1 - x0) as u32;
            let mut sum = [0u32; 4];
            for sx in x0..x1 {
                let pixel = &src[row + sx * 4..row + sx * 4 + 4];
                for channel in 0..4 {
                    sum[channel] += u32::from(pixel[channel]);
                }
            }
            for channel in 0..4 {
                tmp[out + x * 4 + channel] = (sum[channel] / count) as u8;
            }
        }
    }

    // Vertical: tmp (dw x sh) -> dst (dw x dh).
    let mut out = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        let y0 = y * sh / dh;
        let y1 = (((y + 1) * sh) / dh).max(y0 + 1);
        let count = (y1 - y0) as u32;
        for x in 0..dw {
            let mut sum = [0u32; 4];
            for sy in y0..y1 {
                let pixel = &tmp[(sy * dw + x) * 4..(sy * dw + x) * 4 + 4];
                for channel in 0..4 {
                    sum[channel] += u32::from(pixel[channel]);
                }
            }
            let at = (y * dw + x) * 4;
            for channel in 0..4 {
                out[at + channel] = (sum[channel] / count) as u8;
            }
        }
    }
    Some(out)
}

/// A window preview for the panel's window picker: its client content scaled down
/// to fit inside `max`, as tightly packed `Abgr8888` rows (R, G, B, A per pixel,
/// top-down) for the panel to wrap in a texture.
///
/// Reduced by an area average on the CPU rather than sampled down on the GPU:
/// the renderer's downscale filter is plain bilinear with no mip chain, which
/// aliases badly at these ratios. The GPU does one bounded step (see
/// [`readback_size`]) and the average finishes the job, so the cost tracks the
/// thumbnail rather than the window.
///
/// Returns the size actually rendered at — the fit, not `max`, which depends on
/// the window's aspect ratio — with the pixels.
pub fn capture_thumbnail<R>(
    renderer: &mut R,
    window: &WindowElement,
    max: Size<i32, Buffer>,
) -> Option<(Size<i32, Buffer>, Vec<u8>)>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + Send + 'static,
{
    // Fit against the region actually captured, not the window's nominal size:
    // for a client-decorated window those differ, and fitting against the wrong
    // one is what stretches the thumbnail off-centre.
    let source = capture_source(window);
    if source.size.w <= 0 || source.size.h <= 0 {
        return None;
    }
    let content = Size::<i32, Buffer>::from((source.size.w, source.size.h));
    let target = fit_size_allowing(content, max, true)?;
    // Read back no larger than the thumbnail needs, so the cost does not scale
    // with how big the window happens to be. A window smaller than the thumbnail
    // is rendered straight up to it instead: there is nothing to save by
    // rendering it small and reducing afterwards.
    let readback = if content.w <= target.w && content.h <= target.h {
        target
    } else {
        readback_size(content, target)?
    };

    let (pixels, size) = render_window_content(renderer, window, source, readback)?;
    if target == size {
        // Already small enough: the readback is the image.
        return Some((size, pixels));
    }
    Some((target, box_filter(&pixels, size, target)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(w: i32, h: i32) -> Size<i32, Buffer> {
        Size::from((w, h))
    }

    #[test]
    fn a_small_window_is_scaled_up_to_fill_the_thumbnail() {
        let box_size = size(230, 118);
        // Below the box on both axes: should grow to fill it, not arrive small
        // and leave the panel to stretch it.
        assert_eq!(fit_size_allowing(size(100, 80), box_size, true), Some(size(148, 118)));
        // 4:3, so it scales on height: 48 -> 118 makes the width 157.
        assert_eq!(fit_size_allowing(size(64, 48), box_size, true), Some(size(157, 118)));

        // Above the box: still reduced, and never distorted.
        assert_eq!(
            fit_size_allowing(size(1920, 1080), box_size, true),
            Some(size(210, 118)),
        );

        // The other callers want the opposite, and must not be affected.
        assert_eq!(fit_size(size(100, 80), box_size), Some(size(100, 80)));
    }

    #[test]
    fn fits_inside_the_box_without_distorting() {
        // 16:9 into a 200x200 box: the width binds, height follows.
        assert_eq!(fit_size(size(1920, 1080), size(200, 200)), Some(size(200, 113)));
        // Tall and narrow: the height binds instead.
        assert_eq!(fit_size(size(600, 1200), size(200, 200)), Some(size(100, 200)));
        // Already smaller than the box: never scaled up.
        assert_eq!(fit_size(size(80, 60), size(200, 200)), Some(size(80, 60)));
        assert_eq!(fit_size(size(0, 100), size(200, 200)), None);
    }

    /// `count` pixels in a row, each with a distinct red value and opaque alpha.
    fn ramp(count: u8) -> Vec<u8> {
        let mut src = Vec::new();
        for i in 0..count {
            src.extend_from_slice(&[i * 10, 0, 0, 255]);
        }
        src
    }

    #[test]
    fn declared_content_respects_the_buffer_scale() {
        // Firefox reports its content in logical pixels, excluding its shadow. The
        // render elements are in buffer pixels, so a fractional or HiDPI scale has
        // to be applied or the capture covers the wrong region.
        let logical = Rectangle::<i32, Logical>::new(Point::from((10, 28)), Size::from((800, 600)));
        assert_eq!(
            declared_content_rect(Some(logical), 1),
            Some(Rectangle::new(Point::from((10, 28)), Size::from((800, 600))))
        );
        assert_eq!(
            declared_content_rect(Some(logical), 2),
            Some(Rectangle::new(Point::from((20, 56)), Size::from((1600, 1200)))),
            "the offset scales too, not just the size",
        );
        // Nothing declared, or nothing usable.
        assert_eq!(declared_content_rect(None, 1), None);
        assert_eq!(
            declared_content_rect(Some(Rectangle::<i32, Logical>::from_size(Size::from((0, 0)))), 1),
            None
        );
    }

    #[test]
    fn capture_scale_shrinks_rather_than_crops() {
        // The bug this pins: rendering a 1920px source into a 400px buffer without
        // reducing the scale draws a 400px *crop* of the source at full size.
        let source = Size::<i32, Physical>::from((1920, 1080));
        let output = size(400, 225);
        let factor = capture_scale(source, output).unwrap();
        assert!(factor < 1.0, "must actually scale down");
        assert!((factor - 400.0 / 1920.0).abs() < 1e-9);

        // Equal sizes mean no scaling at all.
        let same = size(1920, 1080);
        assert_eq!(capture_scale(source, same), Some(1.0));
        assert_eq!(capture_scale(Size::from((0, 1080)), output), None);
    }

    #[test]
    fn a_captured_region_is_pulled_to_the_origin() {
        // A client-decorated window's content starts at a non-zero offset, inside
        // a surface that is larger by the shadow margin. The buffer is only as
        // large as the content, so without relocating the region to the origin the
        // content is drawn off to one side and its far edge is clipped — which
        // reads as "not centred" rather than as a cropping bug.
        let source = Rectangle::<i32, Physical>::new(Point::from((10, 28)), Size::from((800, 600)));
        let output = size(320, 240);
        let factor = capture_scale(source.size, output).unwrap();
        let origin = capture_origin(source, factor);
        assert_eq!(origin, Point::from((-4, -11)), "the offset is pulled back exactly");

        // The whole point: the crop has to land *inside* the element and cover the
        // buffer exactly. A crop still carrying the region's original offset
        // misses the element, the intersection comes back short, and the preview
        // renders as a crop instead of a centred view.
        let surface = Rectangle::<i32, Physical>::from_size(Size::from((820, 628)));
        let element = Rectangle::new(
            origin,
            Size::from((
                (f64::from(surface.size.w) * factor).round() as i32,
                (f64::from(surface.size.h) * factor).round() as i32,
            )),
        );
        let crop = capture_crop(source, factor);
        assert_eq!(crop.loc, Point::from((0, 0)), "anchored at the relocated origin");
        assert_eq!(element.intersection(crop), Some(crop), "crop must lie within the element");
        assert!((crop.size.w - output.w).abs() <= 1 && (crop.size.h - output.h).abs() <= 1);
    }

    #[test]
    fn a_region_at_the_origin_is_left_alone() {
        let source = Rectangle::<i32, Physical>::from_size(Size::from((1920, 1080)));
        assert_eq!(capture_origin(source, 0.4), Point::from((0, 0)));
    }

    #[test]
    fn the_scaled_crop_fills_the_output_buffer() {
        // Whatever the region and the buffer, the crop the elements are cut to
        // must cover the buffer exactly — otherwise the capture is offset into
        // the window rather than filling the thumbnail.
        for (source, output) in [
            (
                Rectangle::<i32, Physical>::from_size(Size::from((1920, 1080))),
                size(800, 450),
            ),
            (
                Rectangle::<i32, Physical>::from_size(Size::from((1366, 768))),
                size(200, 113),
            ),
            (
                Rectangle::<i32, Physical>::new(Point::from((10, 28)), Size::from((800, 600))),
                size(100, 75),
            ),
        ] {
            let factor = capture_scale(source.size, output).unwrap();
            let crop = capture_crop(source, factor);
            assert_eq!(crop.loc, Point::from((0, 0)));
            assert!(
                (crop.size.w - output.w).abs() <= 1 && (crop.size.h - output.h).abs() <= 1,
                "crop {crop:?} does not fill {output:?}",
            );
        }
    }

    #[test]
    fn readback_is_bounded_but_keeps_the_aspect_ratio() {
        let oversample = readback_oversample();
        // 1080p behind a 200x113 thumbnail: bounded to a small multiple of the
        // thumbnail rather than the full 1920x1080.
        let content = size(1920, 1080);
        let target = size(200, 113);
        let readback = readback_size(content, target).unwrap();
        assert!(readback.w <= target.w * oversample);
        assert!(readback.h <= target.h * oversample);
        assert!(readback.w < content.w, "should have shrunk the transfer");
        // Same aspect ratio as the content, or the thumbnail would be stretched.
        let content_ratio = f64::from(content.w) / f64::from(content.h);
        let readback_ratio = f64::from(readback.w) / f64::from(readback.h);
        assert!((content_ratio - readback_ratio).abs() < 0.01);

        // A window comfortably inside the ceiling is read back whole, so a small
        // window is never needlessly re-rendered.
        let small = size(64, 48);
        assert_eq!(readback_size(small, target), Some(small));
    }

    #[test]
    fn box_filter_averages_the_span_it_covers() {
        // 4 -> 2, so each output is the mean of exactly its own pair. These
        // exact values are what pins the tiling down: an overlapping or
        // half-covered span would give something else.
        let out = box_filter(&ramp(4), size(4, 1), size(2, 1)).unwrap();
        let red = |i: usize| out[i * 4];
        assert_eq!((red(0), red(1)), (5, 25));
        assert_eq!(out[3], 255, "alpha is averaged, not dropped");
    }

    #[test]
    fn box_filter_tiles_the_source_exactly_once() {
        // A 3:1 reduction, which does not divide evenly: the spans must still
        // cover 0..9 with no gap and no overlap.
        let out = box_filter(&ramp(9), size(9, 1), size(3, 1)).unwrap();
        let red = |i: usize| out[i * 4];
        assert_eq!((red(0), red(1), red(2)), (10, 40, 70));
    }

    #[test]
    fn box_filter_preserves_the_mean() {
        // Averaging loses total energy but must not shift the average, which is
        // what makes the result look like the source rather than darker.
        let src = ramp(9);
        let out = box_filter(&src, size(9, 1), size(3, 1)).unwrap();
        let src_mean: f64 = (0..9u8).map(|i| f64::from(i) * 10.0).sum::<f64>() / 9.0;
        let out_mean: f64 = (0..3).map(|i| f64::from(out[i * 4])).sum::<f64>() / 3.0;
        assert!((src_mean - out_mean).abs() < 1.0, "{src_mean} vs {out_mean}");
    }

    #[test]
    fn box_filter_handles_both_axes_and_odd_ratios() {
        let src: Vec<u8> = (0..(7 * 5 * 4)).map(|i| i as u8).collect();
        let out = box_filter(&src, size(7, 5), size(3, 2)).unwrap();
        assert_eq!(out.len(), 3 * 2 * 4);
        // A no-op downscale of equal size should be close to a copy.
        let same = box_filter(&src, size(7, 5), size(7, 5)).unwrap();
        assert_eq!(same, src);
    }
}
