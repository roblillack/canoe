use resvg::tiny_skia;

use super::font;

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
            let metrics = match face.line_metrics() {
                Some(metrics) => metrics,
                None => return,
            };

            let line_height = (metrics.ascender - metrics.descender).max(1);
            let baseline_y = origin_y + (area_height - line_height) / 2 + metrics.ascender;

            let mut x_pos = origin_x + padding_x;
            let max_x = origin_x + area_width - padding_x;

            for ch in text.chars() {
                let glyph = match face.load_char(ch) {
                    Some(glyph) => glyph,
                    None => continue,
                };
                let advance = glyph.advance;
                if x_pos + advance > max_x {
                    break;
                }

                let width = glyph.width;
                let rows = glyph.rows;
                if width <= 0 || rows <= 0 {
                    x_pos += advance;
                    continue;
                }

                let pitch = glyph.pitch;
                let abs_pitch = pitch.abs();
                let glyph_x = x_pos + glyph.left;
                let glyph_y = baseline_y - glyph.top;

                for row in 0..rows {
                    let row_offset = if pitch < 0 {
                        (rows - 1 - row) * abs_pitch
                    } else {
                        row * abs_pitch
                    } as usize;
                    for col in 0..width {
                        let px = glyph_x + col;
                        let py = glyph_y + row;

                        if px >= origin_x
                            && px < origin_x + area_width
                            && py >= origin_y
                            && py < origin_y + area_height
                            && px >= 0
                            && px < buffer_width
                            && py >= 0
                            && py < buffer_height
                        {
                            let idx = row_offset + col as usize;
                            if idx >= glyph.buffer.len() {
                                continue;
                            }
                            let alpha = glyph.buffer[idx];
                            if alpha > 0 {
                                let offset = ((py * buffer_width + px) * 4) as usize;
                                if offset + 4 <= pixels.len() {
                                    blend_pixel(&mut pixels[offset..offset + 4], text_argb, alpha);
                                }
                            }
                        }
                    }
                }

                x_pos += advance;
            }
        });
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
