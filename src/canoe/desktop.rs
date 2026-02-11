//! Desktop background surface for pointer input and minimized window icons.

#![allow(dead_code)]

use memmap2::MmapMut;
use resvg::{tiny_skia, usvg};
use std::fs::File;
use std::os::fd::AsFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::QueueHandle;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

use super::render::Renderer;
use super::{OutputId, WindowId};

// Icon layout constants (logical pixels, scaled by output scale)
pub const ICON_SIZE: i32 = 32;
const ICON_LABEL_HEIGHT: i32 = 14;
const ICON_CELL_WIDTH: i32 = 64;
const ICON_CELL_HEIGHT: i32 = 50;
const ICON_MARGIN: i32 = 4;

/// Data for a minimized window icon.
pub struct DesktopIcon {
    pub window_id: WindowId,
    pub title: String,
    pub app_id: Option<String>,
    pub icon: Option<tiny_skia::Pixmap>,
}

/// Computed icon position on the desktop surface.
struct DesktopIconLayout {
    window_id: WindowId,
    x: i32,
    y: i32,
}

/// Desktop surface with minimized window icons.
pub struct DesktopSurface {
    pub surface: wl_surface::WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    pub buffer: Option<wl_buffer::WlBuffer>,
    pub pool: Option<wl_shm_pool::WlShmPool>,
    pub memfile: Option<File>,
    pub mmap: Option<MmapMut>,
    pub width: i32,
    pub height: i32,
    buf_width: i32,
    buf_height: i32,
    pub configured: bool,
    pub output_id: OutputId,
    pub selected_icon: Option<WindowId>,
    pub icon_cols: i32,
    icons: Vec<DesktopIconLayout>,
}

impl DesktopSurface {
    pub fn new(
        surface: wl_surface::WlSurface,
        layer_surface: ZwlrLayerSurfaceV1,
        output_id: OutputId,
    ) -> Self {
        Self {
            surface,
            layer_surface,
            buffer: None,
            pool: None,
            memfile: None,
            mmap: None,
            width: 0,
            height: 0,
            buf_width: 0,
            buf_height: 0,
            configured: false,
            output_id,
            selected_icon: None,
            icon_cols: 1,
            icons: Vec::new(),
        }
    }

    pub fn configure(&mut self, width: i32, height: i32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.configured = true;
    }

    pub fn reset_buffer(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        if let Some(pool) = self.pool.take() {
            pool.destroy();
        }
        self.memfile = None;
        self.mmap = None;
    }

    pub fn ensure_buffer<D>(&mut self, shm: &wl_shm::WlShm, qh: &QueueHandle<D>, scale: i32)
    where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ()>,
    {
        let scale = scale.max(1);
        if self.width <= 0 || self.height <= 0 {
            return;
        }

        let buf_w = self.width * scale;
        let buf_h = self.height * scale;

        if self.buffer.is_some() && self.buf_width == buf_w && self.buf_height == buf_h {
            return;
        }
        self.reset_buffer();

        let stride = buf_w * 4;
        let size = stride * buf_h;
        let memfd = match memfd::MemfdOptions::default()
            .close_on_exec(true)
            .create("canoe-desktop")
        {
            Ok(fd) => fd,
            Err(_) => {
                return;
            }
        };

        if memfd.as_file().set_len(size as u64).is_err() {
            return;
        }

        let mmap = match unsafe { memmap2::MmapMut::map_mut(memfd.as_file()) } {
            Ok(m) => m,
            Err(_) => {
                return;
            }
        };

        let pool = shm.create_pool(memfd.as_file().as_fd(), size, qh, ());
        let buffer = pool.create_buffer(0, buf_w, buf_h, stride, wl_shm::Format::Argb8888, qh, ());

        self.memfile = Some(memfd.into_file());
        self.mmap = Some(mmap);
        self.pool = Some(pool);
        self.buffer = Some(buffer);
        self.buf_width = buf_w;
        self.buf_height = buf_h;
        self.surface.set_buffer_scale(scale);
    }

    pub fn render(&mut self, rgba: u32) {
        if let Some(ref mut mmap) = self.mmap {
            let argb = rgba_to_argb(rgba);
            let color_bytes = argb.to_ne_bytes();
            for chunk in mmap.as_mut().chunks_exact_mut(4) {
                chunk.copy_from_slice(&color_bytes);
            }
        }
    }

    /// Render the desktop background with minimized window icons.
    #[allow(clippy::too_many_arguments)]
    pub fn render_with_icons(
        &mut self,
        bg_rgba: u32,
        desktop_icons: &[DesktopIcon],
        theme: &IconTheme,
        scale: i32,
        font_name: Option<&str>,
        font_size: f32,
    ) {
        // Fill background
        self.render(bg_rgba);

        if desktop_icons.is_empty() {
            self.icons.clear();
            return;
        }

        let scale = scale.max(1);
        let buf_w = self.buf_width;
        let buf_h = self.buf_height;
        if buf_w <= 0 || buf_h <= 0 {
            return;
        }

        // Compute icon grid layout (bottom-left, left-to-right, then bottom-to-top)
        // All coordinates in logical pixels first, then multiply by scale for rendering
        let logical_w = buf_w / scale;
        let logical_h = buf_h / scale;
        let cols = ((logical_w - ICON_MARGIN * 2) / ICON_CELL_WIDTH).max(1);
        self.icon_cols = cols;

        let mut layouts = Vec::with_capacity(desktop_icons.len());
        for (i, icon) in desktop_icons.iter().enumerate() {
            let col = i as i32 % cols;
            let row = i as i32 / cols;
            let x = ICON_MARGIN + col * ICON_CELL_WIDTH;
            let y = logical_h - ICON_MARGIN - ICON_CELL_HEIGHT - row * ICON_CELL_HEIGHT;
            layouts.push(DesktopIconLayout {
                window_id: icon.window_id,
                x,
                y,
            });
        }
        self.icons = layouts;

        // Now render each icon
        let Some(ref mut mmap) = self.mmap else {
            return;
        };
        let pixels = mmap.as_mut();
        let Some(mut renderer) = Renderer::new(pixels, buf_w, buf_h) else {
            return;
        };

        let selected = self.selected_icon;
        let label_font_size = font_size * 0.67;

        for (i, icon) in desktop_icons.iter().enumerate() {
            let layout = &self.icons[i];
            let is_selected = selected == Some(icon.window_id);

            let icon_bg = if is_selected {
                rgba_to_argb(theme.highlight_bg)
            } else {
                rgba_to_argb(theme.titlebar_bg)
            };
            let icon_text = if is_selected {
                rgba_to_argb(theme.highlight_text)
            } else {
                rgba_to_argb(theme.titlebar_text)
            };
            let label_bg = if is_selected {
                rgba_to_argb(theme.highlight_bg)
            } else {
                0 // transparent - no label background unless selected
            };
            let label_text = if is_selected {
                rgba_to_argb(theme.highlight_text)
            } else {
                rgba_to_argb(theme.text)
            };

            let ix = layout.x * scale;
            let iy = layout.y * scale;
            let icon_px = ICON_SIZE * scale;
            let cell_w = ICON_CELL_WIDTH * scale;

            // Center the 32x32 icon square within the cell width
            let icon_offset_x = (cell_w - icon_px) / 2;

            let has_icon_image = icon.icon.is_some();

            if !has_icon_image {
                // Draw icon background (32x32 area)
                renderer.fill_rect(ix + icon_offset_x, iy, icon_px, icon_px, icon_bg);

                // Draw 1px border around icon square
                let border_color = rgba_to_argb(theme.border);
                let b = scale.max(1);
                renderer.fill_rect(ix + icon_offset_x, iy, icon_px, b, border_color);
                renderer.fill_rect(
                    ix + icon_offset_x,
                    iy + icon_px - b,
                    icon_px,
                    b,
                    border_color,
                );
                renderer.fill_rect(ix + icon_offset_x, iy, b, icon_px, border_color);
                renderer.fill_rect(
                    ix + icon_offset_x + icon_px - b,
                    iy,
                    b,
                    icon_px,
                    border_color,
                );

                // Render first character centered in the icon
                let first_char: String = icon
                    .title
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_uppercase()
                    .collect();
                renderer.render_text(
                    &first_char,
                    ix + icon_offset_x,
                    iy,
                    icon_px,
                    icon_px,
                    scale,
                    icon_text,
                    font_size,
                    font_name,
                    0,
                );
            } else {
                renderer.blit_pixmap(icon.icon.as_ref().unwrap(), ix + icon_offset_x, iy);
            }

            // Draw label background if selected
            let label_y = iy + icon_px;
            let label_h = ICON_LABEL_HEIGHT * scale;
            if is_selected {
                renderer.fill_rect(ix, label_y, cell_w, label_h, label_bg);
            }

            // Render window title centered below icon
            let scaled_label_size = label_font_size * scale as f32;
            let text_w = super::font::measure_text(font_name, scaled_label_size, &icon.title)
                .unwrap_or(0.0) as i32;
            let pad = (cell_w - text_w).max(0) / 2;
            renderer.render_text(
                &icon.title,
                ix,
                label_y,
                cell_w,
                label_h,
                scale,
                label_text,
                label_font_size,
                font_name,
                pad,
            );
        }
    }

    /// Hit test: return the window id of the icon at the given surface-local coordinates.
    /// Coordinates are in logical (surface-local) pixels; layouts are stored in logical pixels.
    pub fn icon_at(&self, x: i32, y: i32, _scale: i32) -> Option<WindowId> {
        for layout in &self.icons {
            if x >= layout.x
                && x < layout.x + ICON_CELL_WIDTH
                && y >= layout.y
                && y < layout.y + ICON_CELL_HEIGHT
            {
                return Some(layout.window_id);
            }
        }
        None
    }

    /// Find the index of the currently selected icon.
    pub fn selected_icon_index(&self) -> Option<usize> {
        let selected = self.selected_icon?;
        self.icons.iter().position(|l| l.window_id == selected)
    }

    /// Get the window id at a given index in the icons list.
    pub fn icon_window_at_index(&self, idx: usize) -> Option<WindowId> {
        self.icons.get(idx).map(|l| l.window_id)
    }

    /// Get the number of icons.
    pub fn icon_count(&self) -> usize {
        self.icons.len()
    }

    pub fn update_input_region<D>(
        &self,
        compositor: &wl_compositor::WlCompositor,
        qh: &QueueHandle<D>,
    ) where
        D: 'static + wayland_client::Dispatch<wl_region::WlRegion, ()>,
    {
        if self.width <= 0 || self.height <= 0 {
            return;
        }

        let region = compositor.create_region(qh, ());
        region.add(0, 0, self.width, self.height);
        self.surface.set_input_region(Some(&region));
        region.destroy();
    }

    pub fn commit(&self) {
        if let Some(ref buffer) = self.buffer {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface
                .damage_buffer(0, 0, self.buf_width, self.buf_height);
            self.surface.commit();
        }
    }
}

/// Theme colors for desktop icons (extracted from UiConfig).
pub struct IconTheme {
    pub bg: u32,
    pub text: u32,
    pub highlight_bg: u32,
    pub highlight_text: u32,
    pub titlebar_bg: u32,
    pub titlebar_text: u32,
    pub border: u32,
}

fn rgba_to_argb(rgba: u32) -> u32 {
    let r = (rgba >> 24) & 0xff;
    let g = (rgba >> 16) & 0xff;
    let b = (rgba >> 8) & 0xff;
    let a = rgba & 0xff;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Load an icon for the given app_id from `~/.config/canoe/icons/`.
/// Tries `<app_id>.svg` first, then `<app_id>.png`. Returns `None` if neither exists.
pub fn load_icon_for_app(app_id: &str, size_px: i32) -> Option<tiny_skia::Pixmap> {
    let home = std::env::var("HOME").ok()?;
    let dir = std::path::PathBuf::from(home)
        .join(".config")
        .join("canoe")
        .join("icons");
    let size = size_px.max(1) as u32;

    // Try SVG first
    let svg_path = dir.join(format!("{}.svg", app_id));
    if let Ok(svg_data) = std::fs::read_to_string(&svg_path) {
        let opt = usvg::Options::default();
        if let Ok(tree) = usvg::Tree::from_str(&svg_data, &opt) {
            if let Some(mut pixmap) = tiny_skia::Pixmap::new(size, size) {
                let tree_size = tree.size();
                let scale_x = size as f32 / tree_size.width();
                let scale_y = size as f32 / tree_size.height();
                let scale = scale_x.min(scale_y);
                let scaled_w = tree_size.width() * scale;
                let scaled_h = tree_size.height() * scale;
                let tx = (size as f32 - scaled_w) * 0.5;
                let ty = (size as f32 - scaled_h) * 0.5;
                let transform =
                    tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);
                let mut pixmap_mut = pixmap.as_mut();
                resvg::render(&tree, transform, &mut pixmap_mut);
                return Some(pixmap);
            }
        }
    }

    // Try PNG
    let png_path = dir.join(format!("{}.png", app_id));
    if let Ok(png_data) = std::fs::read(&png_path) {
        if let Ok(src) = tiny_skia::Pixmap::decode_png(&png_data) {
            return scale_pixmap(&src, size);
        }
    }

    None
}

/// Scale a pixmap to the target size, preserving aspect ratio and centering.
fn scale_pixmap(src: &tiny_skia::Pixmap, size: u32) -> Option<tiny_skia::Pixmap> {
    if src.width() == size && src.height() == size {
        return Some(src.clone());
    }
    let mut dst = tiny_skia::Pixmap::new(size, size)?;
    let scale_x = size as f32 / src.width() as f32;
    let scale_y = size as f32 / src.height() as f32;
    let scale = scale_x.min(scale_y);
    let scaled_w = src.width() as f32 * scale;
    let scaled_h = src.height() as f32 * scale;
    let tx = (size as f32 - scaled_w) * 0.5;
    let ty = (size as f32 - scaled_h) * 0.5;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);
    dst.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        transform,
        None,
    );
    Some(dst)
}

impl Drop for DesktopSurface {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        if let Some(pool) = self.pool.take() {
            pool.destroy();
        }
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}
