use resvg::tiny_skia;

use super::font;

/// How color glyphs (emoji) are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorGlyphs {
    /// In their own colors, for the active titlebar and selected items.
    Natural,
    /// Desaturated and blended with the text color, so they recede along
    /// with the text of inactive titlebars and unselected items.
    Tinted,
}

/// How far tinted color glyphs are pulled from their grayscale version
/// toward the text color.
const TINT_STRENGTH: f32 = 0.5;

pub struct Renderer<'a> {
    pixmap: tiny_skia::PixmapMut<'a>,
}

impl<'a> Renderer<'a> {
    pub fn new(pixels: &'a mut [u8], width: i32, height: i32) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let pixmap = tiny_skia::PixmapMut::from_bytes(pixels, width as u32, height as u32)?;
        Some(Self { pixmap })
    }

    pub fn width(&self) -> i32 {
        self.pixmap.width() as i32
    }

    pub fn height(&self) -> i32 {
        self.pixmap.height() as i32
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        self.pixmap.data_mut()
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, width: i32, height: i32, color_argb: u32) {
        if width <= 0 || height <= 0 {
            return;
        }

        let buffer_width = self.width();
        let buffer_height = self.height();
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + width).min(buffer_width);
        let y1 = (y + height).min(buffer_height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }

        let color_bytes = color_argb.to_ne_bytes();
        let pixels = self.data_mut();
        for row in y0..y1 {
            for col in x0..x1 {
                let offset = ((row * buffer_width + col) * 4) as usize;
                if offset + 4 <= pixels.len() {
                    pixels[offset..offset + 4].copy_from_slice(&color_bytes);
                }
            }
        }
    }

    pub fn blit_pixmap(&mut self, icon: &tiny_skia::Pixmap, x: i32, y: i32) {
        let buffer_width = self.width();
        let buffer_height = self.height();
        if buffer_width <= 0 || buffer_height <= 0 {
            return;
        }

        let icon_w = icon.width() as i32;
        let icon_h = icon.height() as i32;
        if icon_w <= 0 || icon_h <= 0 {
            return;
        }

        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + icon_w).min(buffer_width);
        let y1 = (y + icon_h).min(buffer_height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }

        let icon_data = icon.data();
        let pixels = self.data_mut();
        for row in y0..y1 {
            let src_y = row - y;
            for col in x0..x1 {
                let src_x = col - x;
                let src_offset = ((src_y * icon_w + src_x) * 4) as usize;
                if src_offset + 4 <= icon_data.len() {
                    let src_r = icon_data[src_offset];
                    let src_g = icon_data[src_offset + 1];
                    let src_b = icon_data[src_offset + 2];
                    let src_a = icon_data[src_offset + 3];
                    if src_a == 0 {
                        continue;
                    }
                    let dst_offset = ((row * buffer_width + col) * 4) as usize;
                    if dst_offset + 4 <= pixels.len() {
                        blend_pixel_premul(
                            &mut pixels[dst_offset..dst_offset + 4],
                            src_r,
                            src_g,
                            src_b,
                            src_a,
                        );
                    }
                }
            }
        }
    }

    pub fn blit_argb(&mut self, src: &[u8], src_width: i32, src_height: i32, x: i32, y: i32) {
        if src_width <= 0 || src_height <= 0 {
            return;
        }

        let dst_width = self.width();
        let dst_height = self.height();
        if dst_width <= 0 || dst_height <= 0 {
            return;
        }

        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + src_width).min(dst_width);
        let y1 = (y + src_height).min(dst_height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }

        let dst = self.data_mut();
        for row in y0..y1 {
            let src_y = row - y;
            for col in x0..x1 {
                let src_x = col - x;
                let src_offset = ((src_y * src_width + src_x) * 4) as usize;
                let dst_offset = ((row * dst_width + col) * 4) as usize;
                if src_offset + 4 <= src.len() && dst_offset + 4 <= dst.len() {
                    dst[dst_offset..dst_offset + 4]
                        .copy_from_slice(&src[src_offset..src_offset + 4]);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_text(
        &mut self,
        text: &str,
        origin_x: i32,
        origin_y: i32,
        area_width: i32,
        area_height: i32,
        scale: f32,
        text_argb: u32,
        color_glyphs: ColorGlyphs,
        font_size: f32,
        font_name: Option<&str>,
        padding_x: i32,
    ) {
        if text.is_empty() || area_width <= 0 || area_height <= 0 {
            return;
        }

        let origin_y = origin_y + 1;
        let size_px = (font_size * scale).round().max(1.0) as u32;
        let buffer_width = self.width();
        let buffer_height = self.height();
        let padding_x = padding_x.max(0);

        let pixels = self.data_mut();
        let _ = font::with_face(font_name, size_px, |face| {
            let Some(metrics) = face.line_metrics() else {
                return;
            };

            let line_height = (metrics.ascender - metrics.descender).max(1);
            let baseline_y = origin_y + (area_height - line_height) / 2 + metrics.ascender;
            let start_x = origin_x + padding_x;
            let available = area_width - 2 * padding_x;
            let clip = Clip {
                x0: origin_x.max(0),
                y0: origin_y.max(0),
                x1: (origin_x + area_width).min(buffer_width),
                y1: (origin_y + area_height).min(buffer_height),
            };

            let layout = face.layout(text);
            for placed in &layout.glyphs {
                // Same rounding as `font::measure_text`, so text measured to
                // fit is never cut short.
                if (placed.x + placed.advance).round() as i32 > available {
                    break;
                }
                let Some(glyph) = face.glyph_bitmap(placed) else {
                    continue;
                };
                let glyph_x = start_x + (placed.x + placed.x_offset).round() as i32 + glyph.left;
                let glyph_y = baseline_y - placed.y_offset.round() as i32 - glyph.top;
                let paint = Paint {
                    text_argb,
                    color_glyphs,
                };
                draw_glyph(pixels, buffer_width, &clip, &glyph, glyph_x, glyph_y, paint);
            }
        });
    }
}

/// Visible area of a text draw, in buffer pixels (end-exclusive).
struct Clip {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

/// How to color the glyphs of a text draw.
#[derive(Clone, Copy)]
struct Paint {
    text_argb: u32,
    color_glyphs: ColorGlyphs,
}

impl Paint {
    /// The premultiplied (r, g, b, a) to composite for a color glyph pixel
    /// given as premultiplied (b, g, r, a). Color glyphs take on the text
    /// color's alpha, so translucent text fades emoji along with it.
    fn color_pixel(self, b: u8, g: u8, r: u8, a: u8) -> (u8, u8, u8, u8) {
        let text_alpha = (self.text_argb >> 24) as f32 / 255.0;
        let q = |v: f32| (v * text_alpha).round().clamp(0.0, 255.0) as u8;
        match self.color_glyphs {
            ColorGlyphs::Natural => (q(r as f32), q(g as f32), q(b as f32), q(a as f32)),
            ColorGlyphs::Tinted => {
                // The luma of a premultiplied color is the premultiplied luma.
                let gray = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
                let coverage = a as f32 / 255.0;
                let mix = |shift: u32| {
                    let text = ((self.text_argb >> shift) & 0xff) as f32 * coverage;
                    q(gray * (1.0 - TINT_STRENGTH) + text * TINT_STRENGTH)
                };
                (mix(16), mix(8), mix(0), q(a as f32))
            }
        }
    }
}

/// Composite one glyph bitmap with its top-left corner at (`x`, `y`).
/// Coverage glyphs are drawn in the text color, color glyphs as described
/// by [`Paint::color_pixel`].
fn draw_glyph(
    pixels: &mut [u8],
    buffer_width: i32,
    clip: &Clip,
    glyph: &font::GlyphBitmap<'_>,
    x: i32,
    y: i32,
    paint: Paint,
) {
    let abs_pitch = glyph.pitch.unsigned_abs() as usize;
    let bpp = glyph.pixel_mode.bytes_per_pixel();
    let rows = (clip.y0 - y).max(0)..(clip.y1 - y).min(glyph.rows);
    let cols = (clip.x0 - x).max(0)..(clip.x1 - x).min(glyph.width);

    for row in rows {
        let src_row = if glyph.pitch < 0 {
            glyph.rows - 1 - row
        } else {
            row
        } as usize;
        let py = y + row;
        for col in cols.clone() {
            let idx = src_row * abs_pitch + col as usize * bpp;
            let Some(src) = glyph.buffer.get(idx..idx + bpp) else {
                continue;
            };
            let offset = ((py * buffer_width + x + col) * 4) as usize;
            let Some(dst) = pixels.get_mut(offset..offset + 4) else {
                continue;
            };
            match glyph.pixel_mode {
                font::GlyphPixelMode::Gray => {
                    if src[0] > 0 {
                        blend_pixel(dst, paint.text_argb, src[0]);
                    }
                }
                font::GlyphPixelMode::Bgra => {
                    if src[3] == 0 {
                        continue;
                    }
                    let (r, g, b, a) = paint.color_pixel(src[0], src[1], src[2], src[3]);
                    blend_pixel_premul(dst, r, g, b, a);
                }
            }
        }
    }
}

/// Blend a straight-alpha ARGB color, at `alpha` coverage, over a
/// premultiplied ARGB destination pixel.
fn blend_pixel(bg: &mut [u8], fg_argb: u32, alpha: u8) {
    let fg_a = ((fg_argb >> 24) & 0xff) as u16;
    let fg_r = ((fg_argb >> 16) & 0xff) as u16;
    let fg_g = ((fg_argb >> 8) & 0xff) as u16;
    let fg_b = (fg_argb & 0xff) as u16;

    let a = (alpha as u16 * fg_a) / 255;
    blend_pixel_premul(
        bg,
        ((fg_r * a) / 255) as u8,
        ((fg_g * a) / 255) as u8,
        ((fg_b * a) / 255) as u8,
        a as u8,
    );
}

/// Composite a premultiplied RGBA source over a premultiplied ARGB
/// destination ("over" operator). The destination's alpha is kept, so
/// drawing onto a transparent buffer leaves partially covered pixels
/// partially transparent instead of darkening them to opaque.
fn blend_pixel_premul(bg: &mut [u8], src_r: u8, src_g: u8, src_b: u8, src_a: u8) {
    let inv_a = 255u16.saturating_sub(src_a as u16);

    // Read background (native endian ARGB)
    let bg_val = u32::from_ne_bytes([bg[0], bg[1], bg[2], bg[3]]);
    let bg_a = ((bg_val >> 24) & 0xff) as u16;
    let bg_r = ((bg_val >> 16) & 0xff) as u16;
    let bg_g = ((bg_val >> 8) & 0xff) as u16;
    let bg_b = (bg_val & 0xff) as u16;

    let over = |src: u8, dst: u16| (src as u16 + (dst * inv_a) / 255).min(255) as u32;
    let out_argb = (over(src_a, bg_a) << 24)
        | (over(src_r, bg_r) << 16)
        | (over(src_g, bg_g) << 8)
        | over(src_b, bg_b);
    bg.copy_from_slice(&out_argb.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(argb: u32) -> [u8; 4] {
        argb.to_ne_bytes()
    }

    fn val(p: [u8; 4]) -> u32 {
        u32::from_ne_bytes(p)
    }

    #[test]
    fn blend_over_opaque_stays_opaque() {
        let mut p = px(0xFF000000);
        blend_pixel(&mut p, 0xFFFFFFFF, 128);
        assert_eq!(val(p), 0xFF808080);
    }

    #[test]
    fn blend_over_transparent_keeps_partial_coverage_transparent() {
        let mut p = px(0x00000000);
        blend_pixel(&mut p, 0xFFFFFFFF, 128);
        // Premultiplied half-covered white, not opaque grey.
        assert_eq!(val(p), 0x80808080);

        let mut p = px(0x00000000);
        blend_pixel(&mut p, 0xFFFFFFFF, 255);
        assert_eq!(val(p), 0xFFFFFFFF);
    }

    #[test]
    fn premul_blend_over_transparent_copies_source() {
        let mut p = px(0x00000000);
        blend_pixel_premul(&mut p, 0x20, 0x40, 0x60, 0x80);
        assert_eq!(val(p), 0x80204060);
    }
}

#[cfg(test)]
mod text_tests {
    use super::*;

    fn bgra_glyph(buffer: &[u8]) -> font::GlyphBitmap<'_> {
        font::GlyphBitmap {
            width: 1,
            rows: 1,
            pitch: 4,
            pixel_mode: font::GlyphPixelMode::Bgra,
            buffer,
            left: 0,
            top: 0,
        }
    }

    fn draw(glyph: &font::GlyphBitmap<'_>, text_argb: u32) -> u32 {
        draw_with(glyph, text_argb, ColorGlyphs::Natural)
    }

    fn draw_with(glyph: &font::GlyphBitmap<'_>, text_argb: u32, color_glyphs: ColorGlyphs) -> u32 {
        let mut px = [0u8; 4];
        let clip = Clip {
            x0: 0,
            y0: 0,
            x1: 1,
            y1: 1,
        };
        let paint = Paint {
            text_argb,
            color_glyphs,
        };
        draw_glyph(&mut px, 1, &clip, glyph, 0, 0, paint);
        u32::from_ne_bytes(px)
    }

    #[test]
    fn color_glyphs_ignore_text_color_but_take_its_alpha() {
        // Premultiplied BGRA: opaque orange.
        let orange = [0x00, 0x80, 0xFF, 0xFF];
        assert_eq!(draw(&bgra_glyph(&orange), 0xFF000000), 0xFFFF8000);
        assert_eq!(draw(&bgra_glyph(&orange), 0xFFFFFFFF), 0xFFFF8000);
        assert_eq!(draw(&bgra_glyph(&orange), 0x80000000), 0x80804000);
        assert_eq!(draw(&bgra_glyph(&orange), 0x00000000), 0x00000000);
    }

    #[test]
    fn tinted_color_glyphs_are_gray_blended_with_the_text_color() {
        let tinted = |px: &[u8], text| draw_with(&bgra_glyph(px), text, ColorGlyphs::Tinted);
        // Opaque orange has a luma of ~146, halfway to the text color.
        let orange = [0x00, 0x80, 0xFF, 0xFF];
        assert_eq!(tinted(&orange, 0xFF000000), 0xFF494949);
        assert_eq!(tinted(&orange, 0xFFFFFFFF), 0xFFC8C8C8);
        assert_eq!(tinted(&orange, 0xFFFF0000), 0xFFC84949);
        // The text alpha still applies.
        assert_eq!(tinted(&orange, 0x80000000), 0x80252525);
        // Partially covered pixels stay premultiplied.
        let half_orange = [0x00, 0x40, 0x80, 0x80];
        assert_eq!(tinted(&half_orange, 0xFF000000), 0x80242424);
        assert_eq!(tinted(&half_orange, 0xFFFFFFFF), 0x80646464);
    }

    /// Text measured with `font::measure_text` must render completely when
    /// given exactly that much room.
    fn assert_fits_measured_width(text: &str) {
        let size = 15.0;
        let Some(measured) = font::measure_text(None, size, text) else {
            eprintln!("no font available, skipping");
            return;
        };
        let measured = measured as i32;
        let height = 30;
        let render = |area_width: i32| {
            let width = measured + 60;
            let mut px = vec![0u8; (width * height * 4) as usize];
            let mut r = Renderer::new(&mut px, width, height).unwrap();
            r.render_text(
                text,
                0,
                0,
                area_width,
                height,
                1.0,
                0xFF000000,
                ColorGlyphs::Natural,
                size,
                None,
                0,
            );
            px
        };
        let exact = render(measured);
        let roomy = render(measured + 60);
        let width = (measured + 60) as usize;
        for y in 0..height as usize {
            let row = y * width * 4;
            let span = row..row + measured as usize * 4;
            assert_eq!(
                exact[span.clone()],
                roomy[span],
                "{text:?} cut short at measured width {measured}"
            );
        }
    }

    #[test]
    fn measured_width_fits_rendered_text() {
        assert_fits_measured_width("Plain ASCII title");
        assert_fits_measured_width("Party 🎉 time 😀 ok");
        assert_fits_measured_width("Family 👨\u{200D}👩\u{200D}👧 and 🇩🇪");
        assert_fits_measured_width("CJK 漢字 + 🍣 mix");
        assert_fits_measured_width("AVATAR Wave");
    }
}
