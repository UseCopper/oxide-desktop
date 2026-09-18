//! Rasterize short UI strings (window titles) into premultiplied RGBA buffers
//! for the SSD titlebar.
//!
//! A single [`FontSystem`]/[`SwashCache`] pair is kept per thread: building it
//! loads the system fonts, which is far too expensive to repeat per frame. The
//! renderer is only invoked when a title or its available width actually
//! changes (see [`crate::shell::ssd::HeaderBar::redraw`]).

use std::cell::RefCell;

use cosmic_text::{
    Attrs, Buffer as TextBuffer, Color, Ellipsize, EllipsizeHeightLimit, FontSystem, Metrics, Shaping,
    SwashCache, Wrap,
};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::element::memory::MemoryRenderBuffer,
    },
    utils::{Logical, Size, Transform},
};

/// Point size of the title text. The header bar is fixed at 30 logical pixels,
/// so this leaves comfortable padding.
pub const TITLE_FONT_SIZE: f32 = 13.0;

struct TextRenderer {
    font_system: FontSystem,
    cache: SwashCache,
}

thread_local! {
    static RENDERER: RefCell<Option<TextRenderer>> = const { RefCell::new(None) };
}

/// Rasterize `text` into a premultiplied RGBA buffer of at most `max_width`
/// logical pixels wide and `height` tall, truncating with an ellipsis when it
/// does not fit. Returns the buffer and its logical size, or `None` when there
/// is nothing to draw.
pub fn rasterize(
    text: &str,
    max_width: i32,
    height: i32,
    color: [f32; 4],
) -> Option<(MemoryRenderBuffer, Size<i32, Logical>)> {
    if text.is_empty() || max_width <= 0 || height <= 0 {
        return None;
    }
    RENDERER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let renderer = slot.get_or_insert_with(|| TextRenderer {
            font_system: FontSystem::new(),
            cache: SwashCache::new(),
        });
        renderer.rasterize(text, max_width, height, color)
    })
}

impl TextRenderer {
    fn rasterize(
        &mut self,
        text: &str,
        max_width: i32,
        height: i32,
        color: [f32; 4],
    ) -> Option<(MemoryRenderBuffer, Size<i32, Logical>)> {
        let metrics = Metrics::new(TITLE_FONT_SIZE, height as f32);
        let mut buffer = TextBuffer::new(&mut self.font_system, metrics);
        let mut buffer = buffer.borrow_with(&mut self.font_system);
        buffer.set_wrap(Wrap::None);
        buffer.set_ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)));
        buffer.set_size(Some(max_width as f32), Some(height as f32));
        buffer.set_text(text, &Attrs::new(), Shaping::Advanced, None);

        // Size the output to the shaped line so we don't allocate a full-width
        // buffer for a short title.
        let line_w = buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0f32, f32::max);
        let width = (line_w.ceil() as i32).clamp(1, max_width);
        let size: Size<i32, Logical> = Size::from((width, height));
        let mut pixels = vec![0u8; (width * height * 4) as usize];

        let base = [
            (color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
            (color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
            (color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        ];
        let text_color = Color::rgba(base[0], base[1], base[2], 255);

        buffer.draw(&mut self.cache, text_color, |x, y, w, h, pixel| {
            if w != 1 || h != 1 || x < 0 || y < 0 || x >= width || y >= height {
                return;
            }
            let coverage = pixel.a() as u32;
            if coverage == 0 {
                return;
            }
            // The renderer blends with premultiplied alpha, so store the base
            // color scaled by the glyph's coverage.
            let premul = [
                (base[0] as u32 * coverage / 255) as u8,
                (base[1] as u32 * coverage / 255) as u8,
                (base[2] as u32 * coverage / 255) as u8,
            ];
            let i = ((y * width + x) * 4) as usize;
            // Source-over, in case glyphs overlap.
            let inv = 255 - coverage;
            for (channel, value) in premul.into_iter().enumerate() {
                pixels[i + channel] = value + (pixels[i + channel] as u32 * inv / 255) as u8;
            }
            pixels[i + 3] = (coverage + (pixels[i + 3] as u32 * inv / 255)) as u8;
        });

        Some((
            MemoryRenderBuffer::from_slice(
                &pixels,
                Fourcc::Abgr8888,
                (width, height),
                1,
                Transform::Normal,
                None,
            ),
            size,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_title_has_nothing_to_draw() {
        assert!(rasterize("", 200, 30, [1.0; 4]).is_none());
    }

    #[test]
    fn rasterizes_a_title() {
        let (_buffer, size) = rasterize("Hello", 200, 30, [1.0; 4]).unwrap();
        assert!(size.w > 0 && size.h == 30);
    }

    #[test]
    fn long_title_is_truncated_to_the_available_width() {
        let (_, size) =
            rasterize("a very long window title that will not fit", 40, 30, [1.0; 4]).unwrap();
        assert!(size.w <= 40);
    }
}
