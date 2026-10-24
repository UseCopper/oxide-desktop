use std::{
    cell::Cell,
    sync::{Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

#[cfg(feature = "egl")]
use smithay::backend::renderer::ImportEgl;
#[cfg(feature = "debug")]
use smithay::{
    backend::{allocator::Fourcc, renderer::ImportMem},
    reexports::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle},
};

use smithay::{
    backend::{
        SwapBuffersError,
        allocator::dmabuf::Dmabuf,
        egl::EGLDevice,
        renderer::{
            ImportDma, ImportMemWl, Renderer,
            damage::{Error as OutputDamageTrackerError, OutputDamageTracker},
            element::AsRenderElements,
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    input::{
        keyboard::LedState,
        pointer::{CursorImageAttributes, CursorImageStatus},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
        wayland_server::{Display, protocol::wl_surface},
        winit::{
            dpi::{PhysicalSize, Size},
            event_loop::pump_events::PumpStatus,
            monitor::VideoMode,
            window::Window as WinitWindow,
        },
    },
    utils::{IsAlive, Scale, Transform},
    wayland::{
        compositor,
        dmabuf::{
            DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
        },
        presentation::Refresh,
    },
};
use tracing::{error, info, warn};

use crate::state::{AnvilState, Backend, take_presentation_feedback, update_primary_scanout_output};
use crate::{drawing::*, render::*};

pub const OUTPUT_NAME: &str = "winit";

pub struct WinitData {
    backend: WinitGraphicsBackend<GlesRenderer>,
    damage_tracker: OutputDamageTracker,
    dmabuf_state: (DmabufState, DmabufGlobal, Option<DmabufFeedback>),
    #[cfg(feature = "debug")]
    pub fps: fps_ticker::Fps,
}

impl DmabufHandler for AnvilState<WinitData> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.backend_data.dmabuf_state.0
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        if self
            .backend_data
            .backend
            .renderer()
            .import_dmabuf(&dmabuf, None)
            .is_ok()
        {
            let _ = notifier.successful::<AnvilState<WinitData>>();
        } else {
            notifier.failed();
        }
    }
}

impl Backend for WinitData {
    fn seat_name(&self) -> String {
        String::from("winit")
    }
    fn reset_buffers(&mut self, _output: &Output) {}
    fn early_import(&mut self, _surface: &wl_surface::WlSurface) {}
    fn update_led_state(&mut self, _led_state: LedState) {}

    fn panel_thumbnail(
        &mut self,
        window: &crate::shell::WindowElement,
        max: smithay::utils::Size<i32, smithay::utils::Buffer>,
    ) -> Option<(smithay::utils::Size<i32, smithay::utils::Buffer>, Vec<u8>)> {
        crate::shell::capture_thumbnail(self.backend.renderer(), window, max)
    }
}

/// The refresh rate for an output's mode, out of a display's best mode: the number the
/// compositor paces frames at, or 60Hz for a display that will not say.
fn display_refresh(display: Option<(PhysicalSize<u32>, u32)>) -> i32 {
    display.map(|(_, refresh)| refresh as i32).unwrap_or(60_000)
}

thread_local! {
    /// Whether the window has been asked, once, to be the size of its display.
    ///
    /// Once, so that the host answering with a resize does not ask again — and so that a
    /// window the user has resized since is left where they put it.
    static DISPLAY_SIZED: Cell<bool> = const { Cell::new(false) };
}

/// Ask the window to be the size of the best mode its display offers, if that has not been
/// asked yet and the display is known.
///
/// The display is not known until the host has placed the window, which is after the first
/// frame: the compositor's own window is created at the size its attributes give it and the
/// answer to this arrives as a resize, which is what sets the output's mode.
fn size_to_display(window: &dyn WinitWindow, best: Option<&(PhysicalSize<u32>, u32)>) {
    if DISPLAY_SIZED.with(Cell::get) {
        return;
    }
    let Some((want, _)) = best else {
        // This host does not say: a virtual display, a remote session. The window stays
        // the size it was created at, and 60Hz stays on the output.
        return;
    };
    if want.width == 0 || want.height == 0 {
        return;
    }
    let asked: Size = (*want).into();
    let _ = window.request_surface_size(asked);
    DISPLAY_SIZED.with(|sized| sized.set(true));
}

/// The mode the display a nested window is on is best served by: its size, and its refresh
/// rate in millihertz.
///
/// The same rule a real monitor's modes go through — the most pixels, then the highest
/// refresh — because a nested session's display is a monitor with a mode list like any
/// other, and the list is in no order.
///
/// Nothing comes back for a display that will not say, which is a virtual one or a remote
/// session: the window keeps the size its attributes asked for and the output keeps 60Hz,
/// as both were for everything.
fn display_mode(window: &dyn WinitWindow) -> Option<(PhysicalSize<u32>, u32)> {
    let display = window.current_monitor()?;
    // What the display offers, or — on a host that will only say what it is running at, as
    // Wayland does — the one mode it is in. Either is a mode list; the same rule picks from
    // both, and a list of one picks that one.
    let mut modes: Vec<VideoMode> = display.video_modes().collect();
    if modes.is_empty() {
        modes = display.current_video_mode().into_iter().collect();
    }
    let keys: Vec<(u64, u32)> = modes
        .iter()
        .map(|mode| {
            let size = mode.size();
            (
                crate::output::pixels(size.width, size.height),
                refresh_of(mode),
            )
        })
        .collect();
    let mode = modes.get(crate::output::best_mode(&keys)?)?;
    Some((mode.size(), refresh_of(mode)))
}

/// A video mode's refresh rate in millihertz, with the 60Hz fallback for one that will not
/// say. This is the rate the compositor paces its frames at, so a made-up 1Hz would be
/// worse than the number it replaces.
fn refresh_of(mode: &VideoMode) -> u32 {
    match mode.refresh_rate_millihertz().map(|rate| rate.get()) {
        // Between 1Hz and 1kHz: anything outside that is not a display.
        Some(rate) if (1_000..=1_000_000).contains(&rate) => rate,
        _ => 60_000,
    }
}

pub fn run_winit() {
    let mut event_loop = EventLoop::try_new().unwrap();
    let display = Display::new().unwrap();
    let mut display_handle = display.handle();

    #[cfg_attr(not(feature = "egl"), allow(unused_mut))]
    let (mut backend, mut winit) = {
        // Use vsync so `eglSwapBuffers` blocks on the display's refresh instead
        // of the compositor spinning and swapping as fast as it can.
        use smithay::backend::egl::context::GlAttributes;
        use smithay::reexports::winit::{dpi::LogicalSize, window::WindowAttributes};

        let attributes = WindowAttributes::default()
            .with_surface_size(LogicalSize::new(1280.0, 800.0))
            .with_title("Smithay")
            .with_visible(true);
        let gl_attributes = GlAttributes {
            version: (3, 0),
            profile: None,
            debug: cfg!(debug_assertions),
            vsync: true,
        };

        match winit::init_from_attributes_with_gl_attr::<GlesRenderer>(attributes, gl_attributes) {
            Ok(ret) => ret,
            Err(err) => {
                error!("Failed to initialize Winit backend: {}", err);
                return;
            }
        }
    };
    // The window is the whole of the compositor's output in a nested session, so it asks to
    // be the size of the best mode its display offers: the resolution the desktop lays
    // itself out at is the display's own, and a window smaller than the screen is a smaller
    // desktop than the screen has. On most hosts the display is not known this early, and
    // the resize handler below asks instead.
    let best = display_mode(backend.window());
    size_to_display(backend.window(), best.as_ref());

    let size = backend.window_size();

    let mode = Mode {
        size,
        refresh: display_refresh(best),
    };
    info!(
        width = mode.size.w,
        height = mode.size.h,
        refresh = mode.refresh,
        "output mode"
    );
    let output = Output::new(
        OUTPUT_NAME.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<AnvilState<WinitData>>(&display.handle());
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);

    #[cfg(feature = "debug")]
    #[allow(deprecated)]
    let fps_image =
        image::io::Reader::with_format(std::io::Cursor::new(FPS_NUMBERS_PNG), image::ImageFormat::Png)
            .decode()
            .unwrap();
    #[cfg(feature = "debug")]
    let fps_texture = backend
        .renderer()
        .import_memory(
            &fps_image.to_rgba8(),
            Fourcc::Abgr8888,
            (fps_image.width() as i32, fps_image.height() as i32).into(),
            false,
        )
        .expect("Unable to upload FPS texture");
    #[cfg(feature = "debug")]
    let mut fps_element = FpsElement::new(fps_texture);

    let render_node = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node());

    let dmabuf_default_feedback = match render_node {
        Ok(Some(node)) => {
            let dmabuf_formats = backend.renderer().dmabuf_formats();
            let dmabuf_default_feedback = DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats)
                .build()
                .unwrap();
            Some(dmabuf_default_feedback)
        }
        Ok(None) => {
            warn!("failed to query render node, dmabuf will use v3");
            None
        }
        Err(err) => {
            warn!(?err, "failed to egl device for display, dmabuf will use v3");
            None
        }
    };

    // if we failed to build dmabuf feedback we fall back to dmabuf v3
    // Note: egl on Mesa requires either v4 or wl_drm (initialized with bind_wl_display)
    let dmabuf_state = if let Some(default_feedback) = dmabuf_default_feedback {
        let mut dmabuf_state = DmabufState::new();
        let dmabuf_global = dmabuf_state.create_global_with_default_feedback::<AnvilState<WinitData>>(
            &display.handle(),
            &default_feedback,
        );
        (dmabuf_state, dmabuf_global, Some(default_feedback))
    } else {
        let dmabuf_formats = backend.renderer().dmabuf_formats();
        let mut dmabuf_state = DmabufState::new();
        let dmabuf_global =
            dmabuf_state.create_global::<AnvilState<WinitData>>(&display.handle(), dmabuf_formats);
        (dmabuf_state, dmabuf_global, None)
    };

    #[cfg(feature = "egl")]
    if backend.renderer().bind_wl_display(&display.handle()).is_ok() {
        info!("EGL hardware-acceleration enabled");
    };

    let data = {
        let damage_tracker = OutputDamageTracker::from_output(&output);

        WinitData {
            backend,
            damage_tracker,
            dmabuf_state,
            #[cfg(feature = "debug")]
            fps: fps_ticker::Fps::default(),
        }
    };
    let mut state = AnvilState::init(display, event_loop.handle(), data, true);
    state
        .shm_state
        .update_formats(state.backend_data.backend.renderer().shm_formats());
    state.space.map_output(&output, (0, 0));

    #[cfg(feature = "xwayland")]
    state.start_xwayland();

    info!("Initialization completed, starting the main loop.");

    let mut pointer_element = PointerElement::with_cursor(crate::cursor::Cursor::load());

    // Render only when the host says the surface is ready (`RedrawRequested`,
    // driven by the compositor's frame callbacks), so presentation is paced by
    // the display refresh rather than a fixed timer or a busy loop.
    let mut redraw = true;

    while state.running.load(Ordering::SeqCst) {
        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                // We only have one output
                let output = state.space.outputs().next().unwrap().clone();
                crate::shell::capture_relative_geometries(&state.space, &output);
                state.space.map_output(&output, (0, 0));
                let window = state.backend_data.backend.window();
                let best = display_mode(window);
                size_to_display(window, best.as_ref());
                let mode = Mode {
                    size,
                    refresh: display_refresh(best),
                };
                output.change_current_state(Some(mode), None, None, None);
                output.set_preferred(mode);
                crate::shell::fixup_positions(&mut state.space, state.pointer.current_location());
                crate::shell::apply_relative_geometries(&mut state.space, &output);
            }
            WinitEvent::Input(event) => state.process_input_event_windowed(event, OUTPUT_NAME),
            WinitEvent::CloseRequested => {
                state.running.store(false, Ordering::SeqCst);
            }
            WinitEvent::Redraw => redraw = true,
            _ => (),
        });

        if let PumpStatus::Exit(_) = status {
            state.running.store(false, Ordering::SeqCst);
            break;
        }

        // Dispatch pending Wayland client requests (configure acks and buffer
        // commits) BEFORE rendering. Otherwise a commit dispatched after the
        // render is only shown on the next loop iteration, adding a whole frame
        // of latency to every resize step.
        if event_loop
            .dispatch(Some(Duration::from_millis(1)), &mut state)
            .is_err()
        {
            state.running.store(false, Ordering::SeqCst);
            break;
        }
        state.reap_closing_windows();
        state.space.refresh();
        state.popups.cleanup();
        display_handle.flush_clients().unwrap();

        state.tick_animations();

        // drawing logic
        if redraw {
            redraw = false;
            // Ask the host for the next frame. It is delivered once the surface
            // is ready, which paces us to the display's refresh.
            state.backend_data.backend.window().request_redraw();
            let now = state.clock.now();
            let frame_target = now
                + output
                    .current_mode()
                    .map(|mode| Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
                    .unwrap_or_default();
            state.pre_repaint(&output, frame_target);

            crate::shell::capture_window_snapshots(&state.space, state.backend_data.backend.renderer());

            let dim_alpha = state.dim_alpha(Instant::now());
            let blackout_alpha = state.blackout_alpha(Instant::now());

            let backend = &mut state.backend_data.backend;

            // draw the cursor as relevant
            // reset the cursor if the surface is no longer alive
            let mut reset = false;
            if let CursorImageStatus::Surface(ref surface) = state.cursor_status {
                reset = !surface.alive();
            }
            if reset {
                state.cursor_status = CursorImageStatus::default_named();
            }
            // The cursor is drawn by the compositor (including named shapes),
            // so keep the host cursor hidden.
            let cursor_visible = false;

            pointer_element.set_status(state.cursor_status.clone());

            #[cfg(feature = "debug")]
            let fps = state.backend_data.fps.avg().round() as u32;
            #[cfg(feature = "debug")]
            fps_element.update_fps(fps);

            let show_window_preview = state.show_window_preview;
            let space = &mut state.space;
            let damage_tracker = &mut state.backend_data.damage_tracker;

            let dnd_icon = state.dnd_icon.as_ref();

            let scale = Scale::from(output.current_scale().fractional_scale());
            let cursor_hotspot = if let CursorImageStatus::Surface(ref surface) = state.cursor_status {
                compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<Mutex<CursorImageAttributes>>()
                        .and_then(|attrs| attrs.lock().ok().map(|a| a.hotspot))
                        .unwrap_or_default()
                })
            } else {
                (0, 0).into()
            };
            let cursor_pos = state.pointer.current_location();

            #[cfg(feature = "debug")]
            let mut renderdoc = state.renderdoc.as_mut();

            let age = 0;
            #[cfg(feature = "debug")]
            let window_handle = backend
                .window()
                .window_handle()
                .map(|handle| {
                    if let RawWindowHandle::Wayland(handle) = handle.as_raw() {
                        handle.surface.as_ptr()
                    } else {
                        std::ptr::null_mut()
                    }
                })
                .unwrap_or_else(|_| std::ptr::null_mut());
            let render_res = backend.bind().and_then(|(renderer, mut fb)| {
                #[cfg(feature = "debug")]
                if let Some(renderdoc) = renderdoc.as_mut() {
                    renderdoc.start_frame_capture(renderer.egl_context().get_context_handle(), window_handle);
                }

                let mut elements = Vec::<CustomRenderElements<GlesRenderer>>::new();

                elements.extend(
                    pointer_element.render_elements(
                        renderer,
                        (cursor_pos - cursor_hotspot.to_f64())
                            .to_physical(scale)
                            .to_i32_round(),
                        scale,
                        1.0,
                    ),
                );

                // draw the dnd icon if any
                if let Some(icon) = dnd_icon {
                    let dnd_icon_pos = (cursor_pos + icon.offset.to_f64())
                        .to_physical(scale)
                        .to_i32_round();
                    if icon.surface.alive() {
                        elements.extend(AsRenderElements::<GlesRenderer>::render_elements(
                            &smithay::desktop::space::SurfaceTree::from_surface(&icon.surface),
                            renderer,
                            dnd_icon_pos,
                            scale,
                            1.0,
                        ));
                    }
                }

                #[cfg(feature = "debug")]
                elements.push(CustomRenderElements::Fps(fps_element.clone()));

                render_output(
                    &output,
                    space,
                    elements,
                    renderer,
                    &mut fb,
                    damage_tracker,
                    age,
                    show_window_preview,
                    dim_alpha,
                    blackout_alpha,
                )
                .map_err(|err| match err {
                    OutputDamageTrackerError::Rendering(err) => err.into(),
                    _ => unreachable!(),
                })
            });

            match render_res {
                Ok(render_output_result) => {
                    let has_rendered = render_output_result.damage.is_some();
                    if let Some(damage) = render_output_result.damage {
                        if let Err(err) = backend.submit(Some(damage)) {
                            warn!("Failed to submit buffer: {}", err);
                        }
                    }

                    #[cfg(feature = "debug")]
                    if let Some(renderdoc) = renderdoc.as_mut() {
                        renderdoc.end_frame_capture(
                            backend.renderer().egl_context().get_context_handle(),
                            backend
                                .window()
                                .window_handle()
                                .map(|handle| {
                                    if let RawWindowHandle::Wayland(handle) = handle.as_raw() {
                                        handle.surface.as_ptr()
                                    } else {
                                        std::ptr::null_mut()
                                    }
                                })
                                .unwrap_or_else(|_| std::ptr::null_mut()),
                        );
                    }

                    backend.window().set_cursor_visible(cursor_visible);

                    let states = render_output_result.states;

                    update_primary_scanout_output(
                        &state.space,
                        &output,
                        &state.dnd_icon,
                        &state.cursor_status,
                        &states,
                    );

                    if has_rendered {
                        let mut output_presentation_feedback =
                            take_presentation_feedback(&output, &state.space, &states);
                        output_presentation_feedback.presented(
                            frame_target,
                            output
                                .current_mode()
                                .map(|mode| {
                                    Refresh::fixed(Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
                                })
                                .unwrap_or(Refresh::Unknown),
                            0,
                            wp_presentation_feedback::Kind::Vsync,
                        )
                    }

                    // Send frame events so that client start drawing their next frame
                    state.post_repaint(&output, frame_target, None, &states);
                }
                Err(SwapBuffersError::ContextLost(err)) => {
                    #[cfg(feature = "debug")]
                    if let Some(renderdoc) = renderdoc.as_mut() {
                        renderdoc.discard_frame_capture(
                            backend.renderer().egl_context().get_context_handle(),
                            backend
                                .window()
                                .window_handle()
                                .map(|handle| {
                                    if let RawWindowHandle::Wayland(handle) = handle.as_raw() {
                                        handle.surface.as_ptr()
                                    } else {
                                        std::ptr::null_mut()
                                    }
                                })
                                .unwrap_or_else(|_| std::ptr::null_mut()),
                        );
                    }

                    error!("Critical Rendering Error: {}", err);
                    state.running.store(false, Ordering::SeqCst);
                }
                Err(err) => warn!("Rendering error: {}", err),
            }
        }

        // Unlike the DRM backend, nothing else prunes the renderer's texture
        // caches on winit, so unreferenced dmabuf imports would accumulate
        // forever. Do it once per frame.
        if let Err(err) = state.backend_data.backend.renderer().cleanup_texture_cache() {
            warn!("Failed to clean up renderer texture cache: {}", err);
        }

        #[cfg(feature = "debug")]
        state.backend_data.fps.tick();
    }
}
