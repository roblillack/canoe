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
        "dnd-copy" => &["copy", "left_ptr"],
        "dnd-move" => &["grabbing", "closedhand", "left_ptr"],
        "dnd-ask" => &["context-menu", "left_ptr"],
        "dnd-no-drop" => &[
            "no-drop",
            "not-allowed",
            "forbidden",
            "crossed_circle",
            "left_ptr",
        ],
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

/// A black arrow with a white outline, for systems without cursor themes.
fn builtin_arrow(size: i32) -> CursorFrame {
    let size = size.max(8);
    let mut buffer =
        MemoryRenderBuffer::new(Fourcc::Argb8888, (size, size), 1, Transform::Normal, None);
    let _ = buffer.render().draw(|dst: &mut [u8]| {
        for y in 0..size {
            for x in 0..size {
                // A triangle: x <= y/1.5, cut off at 3/4 of the height.
                let inside = (x as f64) <= (y as f64) / 1.5 && y < size * 3 / 4;
                let edge = inside
                    && ((x as f64) > (y as f64) / 1.5 - 1.5 || x == 0 || y == size * 3 / 4 - 1);
                let i = ((y * size + x) * 4) as usize;
                let (v, a) = if edge {
                    (255, 255)
                } else if inside {
                    (0, 255)
                } else {
                    (0, 0)
                };
                dst[i..i + 4].copy_from_slice(&[v, v, v, a]);
            }
        }
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
