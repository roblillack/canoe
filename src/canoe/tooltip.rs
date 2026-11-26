//! Tooltips of desktop icons whose clients gave them one (xx-iconlet-v1).

use super::font;
use super::menu::MenuTheme;
use super::render::{ColorGlyphs, Renderer};
use super::scale::{FractionalSurface, Origin, Scale};
use super::surface::{Buffer, LayerSurface, Region, Surface};
use super::{OutputId, WindowId};

// Layout metrics, in logical pixels.
const PADDING_X: i32 = 6;
const PADDING_Y: i32 = 3;
/// Space between lines.
const LINE_GAP: i32 = 2;
/// Space between the tooltip and the icon.
const ICON_GAP: i32 = 4;
/// At most this many lines are shown.
const MAX_LINES: usize = 20;

const BORDER_COLOR: u32 = 0x000000FF;

/// A tooltip shown next to a desktop icon. It is laid out in logical
/// pixels and drawn at the surface's [`Scale`]; it takes no input.
pub struct IconTooltip {
    pub surface: Surface,
    pub layer_surface: LayerSurface,
    pub fractional: FractionalSurface,
    buffer: Option<Buffer>,
    /// Surface size (logical px)
    pub width: i32,
    pub height: i32,
    buffer_width: i32,
    buffer_height: i32,
    scale: Scale,
    pub configured: bool,
    pub output_id: OutputId,
    /// The window whose icon the tooltip belongs to.
    pub window_id: WindowId,
    /// Where the surface is on its output (logical px).
    pub origin_x: i32,
    pub origin_y: i32,
    pub text: String,
    theme: MenuTheme,
}

impl IconTooltip {
    /// A tooltip saying `text` for the icon of `window_id`, which is at
    /// `icon` (x, y, width, height) on an output of `output_size`, all in
    /// logical pixels. It goes above the icon, or below it if there is no
    /// room above, and stays on the output.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        surface: Surface,
        layer_surface: LayerSurface,
        fractional: FractionalSurface,
        output_id: OutputId,
        window_id: WindowId,
        text: String,
        icon: (i32, i32, i32, i32),
        output_size: (i32, i32),
        theme: MenuTheme,
        scale: Scale,
    ) -> Self {
        let (width, height) = measure(&text, &theme, &scale);
        let (origin_x, origin_y) = place((width, height), icon, output_size);
        Self {
            surface,
            layer_surface,
            fractional,
            buffer: None,
            width,
            height,
            buffer_width: 0,
            buffer_height: 0,
            scale,
            configured: false,
            output_id,
            window_id,
            origin_x,
            origin_y,
            text,
            theme,
        }
    }

    pub fn reset_buffer(&mut self) {
        self.buffer = None;
    }

    pub fn ensure_buffer(&mut self, scale: Scale) {
        if self.width <= 0 || self.height <= 0 {
            return;
        }
        let (buffer_width, buffer_height) = self.fractional.apply(
            &self.surface,
            self.width,
            self.height,
            &scale,
            Origin::new(self.origin_x, self.origin_y),
        );
        if buffer_width <= 0 || buffer_height <= 0 {
            return;
        }
        if self.buffer.is_some()
            && (self.buffer_width, self.buffer_height) == (buffer_width, buffer_height)
            && self.scale == scale
        {
            return;
        }
        self.buffer = Buffer::new(buffer_width, buffer_height);
        self.buffer_width = buffer_width;
        self.buffer_height = buffer_height;
        self.scale = scale;
    }

    pub fn render(&mut self) {
        let Some(buffer) = self.buffer.clone() else {
            return;
        };
        let (w, h) = (self.buffer_width, self.buffer_height);
        let scale = self.scale;
        let theme = &self.theme;
        let line_h = scale.px(line_height(theme));
        let lines = lines(&self.text);
        buffer.draw(|pixels| {
            let Some(mut renderer) = Renderer::new(pixels, w, h) else {
                return;
            };
            let line = scale.line();
            renderer.fill_rect(0, 0, w, h, rgba_to_argb(BORDER_COLOR));
            renderer.fill_rect(
                line,
                line,
                w - line * 2,
                h - line * 2,
                rgba_to_argb(theme.bg),
            );
            let x = line + scale.px(PADDING_X);
            let mut y = line + scale.px(PADDING_Y);
            for text in lines {
                renderer.render_text(
                    text,
                    x,
                    y,
                    w - line - x,
                    line_h,
                    scale.factor(),
                    rgba_to_argb(theme.text),
                    ColorGlyphs::Natural,
                    theme.font_size,
                    theme.font_name.as_deref(),
                    0,
                );
                y += line_h + scale.px(LINE_GAP);
            }
        });
    }

    pub fn commit(&self) {
        // The tooltip never takes the pointer from the icon under it.
        self.surface.set_input_region(Some(&Region::new()));
        if let Some(ref buffer) = self.buffer {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface
                .damage_buffer(0, 0, self.buffer_width, self.buffer_height);
            self.surface.commit();
        }
    }
}

impl Drop for IconTooltip {
    fn drop(&mut self) {
        self.buffer = None;
        self.fractional.destroy();
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}

/// The lines of a tooltip's text that are shown.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(|l| l.trim_end_matches('\r'))
        .take(MAX_LINES)
}

fn line_height(theme: &MenuTheme) -> i32 {
    theme.font_size.ceil() as i32 + 2
}

/// The size of a tooltip saying `text`, in logical pixels. The text is
/// measured at the size it will be drawn at.
fn measure(text: &str, theme: &MenuTheme, scale: &Scale) -> (i32, i32) {
    let font_px = scale.px_f(theme.font_size);
    let widest = lines(text)
        .filter_map(|line| font::measure_text(theme.font_name.as_deref(), font_px, line))
        .fold(0.0, f32::max);
    let count = lines(text).count().max(1) as i32;
    let text_w = (widest / scale.factor()).ceil() as i32;
    (
        text_w + PADDING_X * 2 + 2,
        count * line_height(theme) + (count - 1) * LINE_GAP + PADDING_Y * 2 + 2,
    )
}

/// Where a tooltip of `size` goes for an icon at `icon` on an output of
/// `output_size`: centered above the icon, or below it without room above,
/// and moved onto the output.
fn place(
    (width, height): (i32, i32),
    (icon_x, icon_y, icon_w, icon_h): (i32, i32, i32, i32),
    (output_w, output_h): (i32, i32),
) -> (i32, i32) {
    let x = icon_x + (icon_w - width) / 2;
    let above = icon_y - ICON_GAP - height;
    let y = if above >= 0 {
        above
    } else {
        icon_y + icon_h + ICON_GAP
    };
    let clamp = |v: i32, size: i32, room: i32| v.min(room - size).max(0);
    (clamp(x, width, output_w), clamp(y, height, output_h))
}

fn rgba_to_argb(rgba: u32) -> u32 {
    let r = (rgba >> 24) & 0xff;
    let g = (rgba >> 16) & 0xff;
    let b = (rgba >> 8) & 0xff;
    let a = rgba & 0xff;
    (a << 24) | (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goes_above_the_icon_centered() {
        assert_eq!(place((100, 30), (50, 500, 48, 48), (1000, 800)), (24, 466));
    }

    #[test]
    fn goes_below_without_room_above() {
        assert_eq!(place((100, 30), (50, 10, 48, 48), (1000, 800)), (24, 62));
    }

    #[test]
    fn stays_on_the_output() {
        assert_eq!(place((100, 30), (0, 500, 48, 48), (1000, 800)), (0, 466));
        assert_eq!(
            place((100, 30), (960, 500, 48, 48), (1000, 800)),
            (900, 466)
        );
    }

    #[test]
    fn shows_at_most_max_lines() {
        let text = "a\n".repeat(MAX_LINES + 5);
        assert_eq!(lines(&text).count(), MAX_LINES);
        assert_eq!(lines("one\r\ntwo").collect::<Vec<_>>(), ["one", "two"]);
    }
}
