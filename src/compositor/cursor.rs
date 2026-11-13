//! Cursor images from XCursor themes, for the cursors the compositor draws
//! itself: over canoe's own surfaces, during moves and resizes, and where no
//! client set one.

use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::Transform;
use xcursor::parser::{parse_xcursor, Image};

/// One frame of a cursor, at a given scale.
#[derive(Clone)]
pub struct CursorFrame {
    pub buffer: MemoryRenderBuffer,
    /// In pixels of the buffer.
    pub hotspot: (i32, i32),
    /// Buffer size in pixels.
    pub size: (i32, i32),
    pub delay: Duration,
}

/// A cursor's RGBA pixels, size and hotspot.
pub type CursorImage = (Vec<u8>, (u16, u16), (u16, u16));

pub struct CursorTheme {
    name: String,
    size: u32,
    /// Frames per (cursor name, buffer scale).
    cache: HashMap<(String, u32), Vec<CursorFrame>>,
}

impl CursorTheme {
    pub fn new(name: &str, size: u32) -> Self {
        Self {
            name: name.to_string(),
            size: size.max(1),
            cache: HashMap::new(),
        }
    }

    pub fn set_theme(&mut self, name: &str, size: u32) {
        if self.name != name || self.size != size {
            self.name = name.to_string();
            self.size = size.max(1);
            self.cache.clear();
        }
    }

    /// Logical cursor size.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// The theme's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The frame of cursor `name` to show `elapsed` into its animation, drawn
    /// for an output of integer scale `scale`.
    pub fn frame(&mut self, name: &str, scale: u32, elapsed: Duration) -> Option<CursorFrame> {
        let key = (name.to_string(), scale.max(1));
        if !self.cache.contains_key(&key) {
            let frames = self.load(name, scale.max(1));
            self.cache.insert(key.clone(), frames);
        }
        let frames = self.cache.get(&key)?;
        if frames.is_empty() {
            return None;
        }
        let total: Duration = frames.iter().map(|f| f.delay).sum();
        if total.is_zero() || frames.len() == 1 {
            return frames.first().cloned();
        }
        let mut t = Duration::from_nanos((elapsed.as_nanos() % total.as_nanos()) as u64);
        for frame in frames {
            if t < frame.delay {
                return Some(frame.clone());
            }
            t -= frame.delay;
        }
        frames.first().cloned()
    }

    /// The theme's default cursor, as RGBA pixels with its size and hotspot,
    /// for programs drawing at integer scale `scale` (X11 programs, which
    /// draw their own cursors).
    pub fn default_image(&self, scale: u32) -> Option<CursorImage> {
        let target = (self.size * scale.max(1)) as i32;
        let theme = xcursor::CursorTheme::load(&self.name);
        let image = ["default", "left_ptr"].iter().find_map(|name| {
            let data = std::fs::read(theme.load_icon(name)?).ok()?;
            let images = parse_xcursor(&data)?;
            images
                .into_iter()
                .min_by_key(|image| (image.size as i32 - target).abs())
        })?;
        Some((
            image.pixels_rgba,
            (image.width as u16, image.height as u16),
            (image.xhot as u16, image.yhot as u16),
        ))
    }

    fn load(&self, name: &str, scale: u32) -> Vec<CursorFrame> {
        let target = self.size * scale;
        let theme = xcursor::CursorTheme::load(&self.name);
        for candidate in std::iter::once(name).chain(fallback_names(name).iter().copied()) {
            let Some(path) = theme.load_icon(candidate) else {
                continue;
            };
            let mut data = Vec::new();
            let Ok(mut file) = std::fs::File::open(path) else {
                continue;
            };
            if file.read_to_end(&mut data).is_err() {
                continue;
            }
            let Some(images) = parse_xcursor(&data) else {
                continue;
            };
            let frames = pick_frames(&images, target);
            if !frames.is_empty() {
                return frames;
            }
        }
        // No theme at all: draw a plain arrow.
        vec![builtin_arrow(target as i32)]
    }
}

/// Older names cursor themes use for the CSS cursor names.
fn fallback_names(name: &str) -> &'static [&'static str] {
    match name {
        "default" => &["left_ptr", "arrow"],
        "pointer" => &["hand2", "hand1", "left_ptr"],
        "crosshair" => &["cross", "tcross", "left_ptr"],
        "n-resize" => &["top_side", "left_ptr"],
        "s-resize" => &["bottom_side", "left_ptr"],
        "e-resize" => &["right_side", "left_ptr"],
        "w-resize" => &["left_side", "left_ptr"],
        "ne-resize" => &["top_right_corner", "left_ptr"],
        "nw-resize" => &["top_left_corner", "left_ptr"],
        "se-resize" => &["bottom_right_corner", "left_ptr"],
        "sw-resize" => &["bottom_left_corner", "left_ptr"],
        "ew-resize" => &["sb_h_double_arrow", "left_ptr"],
        "ns-resize" => &["sb_v_double_arrow", "left_ptr"],
        "all-scroll" => &["fleur", "left_ptr"],
        "text" => &["xterm", "left_ptr"],
        _ => &["left_ptr", "default"],
    }
}

/// The frames of the size closest to `target`.
fn pick_frames(images: &[Image], target: u32) -> Vec<CursorFrame> {
    let Some(nearest) = images
        .iter()
        .min_by_key(|image| (image.size as i32 - target as i32).abs())
    else {
        return Vec::new();
    };
    images
        .iter()
        .filter(|image| image.width == nearest.width && image.height == nearest.height)
        .map(|image| {
            let mut buffer = MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (image.width as i32, image.height as i32),
                1,
                Transform::Normal,
                None,
            );
            // xcursor's pixels_rgba are premultiplied RGBA; the buffer wants
            // ARGB8888 in little endian, i.e. BGRA bytes.
            let pixels = &image.pixels_rgba;
            let _ = buffer.render().draw(|dst: &mut [u8]| {
                for (d, &[r, g, b, a]) in dst
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(pixels.as_chunks::<4>().0)
                {
                    *d = [b, g, r, a];
                }
                Result::<_, std::convert::Infallible>::Ok(vec![
                    smithay::utils::Rectangle::from_size(
                        (image.width as i32, image.height as i32).into(),
                    ),
                ])
            });
            CursorFrame {
                buffer,
                hotspot: (image.xhot as i32, image.yhot as i32),
                size: (image.width as i32, image.height as i32),
                delay: Duration::from_millis(image.delay as u64),
            }
        })
        .collect()
}

/// The classic left-pointing arrow, as a closed polygon whose tip is the
/// hotspot, in a grid [`ARROW_GRID`] units tall. Drawn when no cursor theme
/// at all can be loaded, so that even then the pointer looks like a pointer.
const ARROW: &[(f64, f64)] = &[
    (0.0, 0.0),
    (0.0, 17.1),
    (4.2, 13.2),
    (6.5, 20.1),
    (10.1, 18.4),
    (7.3, 12.4),
    (12.3, 11.9),
];

/// The height of the grid [`ARROW`] is given in, a little taller than the
/// arrow itself so it keeps a margin to the bottom of the image.
const ARROW_GRID: f64 = 20.8;

/// Width of the arrow's outline, in grid units.
const ARROW_OUTLINE: f64 = 0.9;

/// A black arrow with a white outline, for systems without cursor themes.
fn builtin_arrow(size: i32) -> CursorFrame {
    let size = size.max(8);
    let pixels = arrow_pixels(size);
    let mut buffer =
        MemoryRenderBuffer::new(Fourcc::Argb8888, (size, size), 1, Transform::Normal, None);
    let _ = buffer.render().draw(|dst: &mut [u8]| {
        // The buffer is tightly packed, like the one `pick_frames` fills.
        let n = dst.len().min(pixels.len());
        dst[..n].copy_from_slice(&pixels[..n]);
        Result::<_, std::convert::Infallible>::Ok(vec![smithay::utils::Rectangle::from_size(
            (size, size).into(),
        )])
    });
    CursorFrame {
        buffer,
        hotspot: (0, 0),
        size: (size, size),
        delay: Duration::ZERO,
    }
}

/// [`ARROW`] rasterized into `size`×`size` premultiplied ARGB8888 pixels,
/// antialiased by sampling [`ARROW_SUBSAMPLES`] times per pixel and axis.
fn arrow_pixels(size: i32) -> Vec<u8> {
    const ARROW_SUBSAMPLES: i32 = 4;
    let scale = size as f64 / ARROW_GRID;
    let arrow: Vec<(f64, f64)> = ARROW.iter().map(|&(x, y)| (x * scale, y * scale)).collect();
    let outline = ARROW_OUTLINE * scale;
    let samples = (ARROW_SUBSAMPLES * ARROW_SUBSAMPLES) as u32;
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            // How much of this pixel the arrow's fill and its outline cover.
            let (mut fill, mut edge) = (0u32, 0u32);
            for sy in 0..ARROW_SUBSAMPLES {
                for sx in 0..ARROW_SUBSAMPLES {
                    let point = (
                        x as f64 + (sx as f64 + 0.5) / ARROW_SUBSAMPLES as f64,
                        y as f64 + (sy as f64 + 0.5) / ARROW_SUBSAMPLES as f64,
                    );
                    if !inside_polygon(&arrow, point) {
                        continue;
                    }
                    // The outline is drawn inside the shape, so the arrow
                    // keeps the polygon's silhouette.
                    if distance_to_polygon(&arrow, point) < outline {
                        edge += 1;
                    } else {
                        fill += 1;
                    }
                }
            }
            // Premultiplied: the white outline is scaled by its own coverage
            // and the black fill only adds opacity. Gray, so the channel order
            // does not matter.
            let value = (255 * edge / samples) as u8;
            let alpha = (255 * (fill + edge) / samples) as u8;
            let i = ((y * size + x) * 4) as usize;
            pixels[i..i + 4].copy_from_slice(&[value, value, value, alpha]);
        }
    }
    pixels
}

/// Whether `point` is inside the closed polygon `points` (even-odd rule).
fn inside_polygon(points: &[(f64, f64)], point: (f64, f64)) -> bool {
    let (px, py) = point;
    let mut inside = false;
    let mut j = points.len() - 1;
    for i in 0..points.len() {
        let (xi, yi) = points[i];
        let (xj, yj) = points[j];
        if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The distance from `point` to the closest edge of the closed polygon.
fn distance_to_polygon(points: &[(f64, f64)], point: (f64, f64)) -> f64 {
    let mut j = points.len() - 1;
    let mut best = f64::MAX;
    for i in 0..points.len() {
        best = best.min(distance_to_segment(points[j], points[i], point));
        j = i;
    }
    best
}

/// The distance from `point` to the segment from `a` to `b`.
fn distance_to_segment(a: (f64, f64), b: (f64, f64), point: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 {
        0.0
    } else {
        (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / length).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((point.0 - cx).powi(2) + (point.1 - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pixels of the built-in arrow as (value, alpha) pairs: the value is
    /// premultiplied, so an opaque outline pixel is (255, 255), an opaque fill
    /// pixel (0, 255) and an uncovered one (0, 0).
    fn arrow(size: i32) -> Vec<(u8, u8)> {
        arrow_pixels(size)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&[value, _, _, alpha]| (value, alpha))
            .collect()
    }

    #[test]
    fn test_builtin_arrow_is_an_arrow() {
        let size = 32;
        let pixels = arrow(size);
        let at = |x: i32, y: i32| pixels[(y * size + x) as usize];

        // The tip is the hotspot, drawn in the outline's white ...
        let (value, alpha) = at(0, 0);
        assert!(alpha > 0 && value == alpha, "no tip at the hotspot");
        // ... with black fill behind it ...
        assert_eq!(at(4, 12), (0, 255));
        // ... and nothing in the corners the arrow does not reach.
        assert_eq!(at(size - 1, 0), (0, 0));
        assert_eq!(at(size - 1, size - 1), (0, 0));
        assert_eq!(at(0, size - 1), (0, 0));

        // The outline encloses the fill: no filled pixel borders on an
        // uncovered one.
        for y in 0..size {
            for x in 0..size {
                if at(x, y) != (0, 255) {
                    continue;
                }
                assert!(x > 0 && y > 0 && x < size - 1 && y < size - 1);
                for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                    assert!(at(x + dx, y + dy).1 > 0, "fill leaks at {x},{y}");
                }
            }
        }

        // Antialiased: the slanted edges are partly covered.
        assert!(pixels.iter().any(|&(_, alpha)| alpha > 0 && alpha < 255));
    }

    #[test]
    fn test_builtin_arrow_scales() {
        for size in [8, 16, 24, 32, 48, 96] {
            let pixels = arrow(size);
            assert_eq!(pixels.len(), (size * size) as usize);
            let area = (size * size) as u32;
            let covered: u32 = pixels.iter().map(|&(_, alpha)| alpha as u32).sum::<u32>() / 255;
            let outline: u32 = pixels.iter().map(|&(value, _)| value as u32).sum::<u32>() / 255;
            // The arrow covers about a quarter of the image at every size ...
            assert!(
                covered * 5 > area && covered * 3 < area,
                "size {size}: {covered} covered of {area}"
            );
            // ... and is mostly fill, with the outline around it.
            assert!(outline * 2 < covered, "size {size}: {outline} of {covered}");
        }
    }
}
