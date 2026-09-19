//! Window pixel capture.
//!
//! Two things need a window's pixels off the GPU:
//!
//! * an app-triggered close, which keeps drawing the window from the last
//!   committed buffers after the client is gone (see [`capture_surface_tree`]);
//! * a maximize/unmaximize crossfade, which freezes the pre-transition frame so
//!   it can fade into the live content (see [`capture_window_snapshots`]).

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ExportMem, ImportAll, ImportMem, Offscreen, Renderer, Texture,
            damage::OutputDamageTracker,
            element::{AsRenderElements, memory::MemoryRenderBuffer, surface::WaylandSurfaceRenderElement},
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
    let mut target = renderer.create_buffer(Fourcc::Abgr8888, size).ok()?;
    let mut framebuffer = renderer.bind(&mut target).ok()?;

    let elements: Vec<WaylandSurfaceRenderElement<R>> = AsRenderElements::render_elements(
        &window.0,
        renderer,
        Point::from((0, 0)),
        Scale::from(1.0),
        1.0,
    );

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
    // Smithay's GL renderer flips y while drawing into a `Normal` target, so the
    // readback already comes out top-down: no extra transform is needed.
    Some(MemoryRenderBuffer::from_slice(
        data,
        Fourcc::Abgr8888,
        size,
        1,
        Transform::Normal,
        None,
    ))
}
