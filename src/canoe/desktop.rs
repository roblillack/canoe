//! Desktop background surface for pointer input and minimized window icons.

#![allow(dead_code)]

use memmap2::MmapMut;
use resvg::{tiny_skia, usvg};
use std::fs::File;
use std::os::fd::AsFd;
use std::rc::Rc;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::QueueHandle;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

use super::render::Renderer;
use super::{shmfile, OutputId, WindowId};

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
    pub icon: Option<Rc<tiny_skia::Pixmap>>,
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
    pub dirty: bool,
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
            dirty: true,
        }
    }

    pub fn configure(&mut self, width: i32, height: i32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.configured = true;
        self.dirty = true;
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
        self.dirty = true;
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
        let memfile = match shmfile::create("canoe-desktop", size as i64) {
            Ok(f) => f,
            Err(_) => {
                return;
            }
        };

        let mmap = match unsafe { memmap2::MmapMut::map_mut(&memfile) } {
            Ok(m) => m,
            Err(_) => {
                return;
            }
        };

        let pool = shm.create_pool(memfile.as_fd(), size, qh, ());
        let buffer = pool.create_buffer(0, buf_w, buf_h, stride, wl_shm::Format::Argb8888, qh, ());

        self.memfile = Some(memfile);
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
    ///
    /// `font_name` / `font_size` are the UI font used for the first-letter
    /// fallback drawn inside the icon square when no image is available.
    /// `label_font_name` / `label_font_size` control the label rendered
    /// beneath each icon and are typically resolved from
    /// [`crate::config::DesktopIconsConfig`].
    #[allow(clippy::too_many_arguments)]
    pub fn render_with_icons(
        &mut self,
        bg_rgba: u32,
        desktop_icons: &[DesktopIcon],
        theme: &IconTheme,
        scale: i32,
        font_name: Option<&str>,
        font_size: f32,
        label_font_name: Option<&str>,
        label_font_size: f32,
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

        // Render order: non-selected first, selected last so its (possibly
        // wider) label overlays neighbouring labels.
        let mut order: Vec<usize> = (0..desktop_icons.len()).collect();
        if let Some(sel) = selected {
            if let Some(pos) = order
                .iter()
                .position(|&i| desktop_icons[i].window_id == sel)
            {
                let idx = order.remove(pos);
                order.push(idx);
            }
        }

        for &i in &order {
            let icon = &desktop_icons[i];
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

            // Render window title centered below icon. Non-selected labels
            // are truncated with an ellipsis to fit within the cell so they
            // don't overlap neighbours; the selected label is allowed to
            // overflow up to 5x the icon width before being truncated.
            let label_y = iy + icon_px;
            let label_h = ICON_LABEL_HEIGHT * scale;
            let scaled_label_size = label_font_size * scale as f32;
            let max_label_w = if is_selected { icon_px * 5 } else { cell_w };
            let display_title = truncate_with_ellipsis(
                &icon.title,
                max_label_w,
                label_font_name,
                scaled_label_size,
            );
            let text_w =
                super::font::measure_text(label_font_name, scaled_label_size, &display_title)
                    .unwrap_or(0.0) as i32;
            let icon_center_x = ix + cell_w / 2;
            let label_x = icon_center_x - text_w / 2;

            // Draw label background if selected, sized to the text
            if is_selected {
                let margin = 2 * scale;
                let bg_x = label_x - margin;
                let bg_w = text_w + margin * 2;
                renderer.fill_rect(bg_x, label_y, bg_w, label_h, label_bg);
            }

            renderer.render_text(
                &display_title,
                label_x,
                label_y,
                text_w.max(1),
                label_h,
                scale,
                label_text,
                label_font_size,
                label_font_name,
                0,
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

/// Build a regular-weight fontconfig query from a base UI font name.
///
/// Strips bold-related properties so that icon labels render in the regular
/// weight even when the main UI font is configured as bold. Returns `"sans"`
/// when `base` is `None` or empty.
pub fn regular_weight_font_query(base: Option<&str>) -> String {
    let base = base.unwrap_or("sans");
    let parts: Vec<&str> = base
        .split(':')
        .filter(|part| {
            let lower = part.to_ascii_lowercase();
            !lower.starts_with("style=") && !lower.starts_with("weight=") && lower != "bold"
        })
        .collect();
    let family = if parts.is_empty() { "sans" } else { parts[0] };
    let mut query = String::from(family);
    for &part in &parts[1..] {
        query.push(':');
        query.push_str(part);
    }
    query.push_str(":weight=regular");
    query
}

/// Theme colors for desktop icons (extracted from UiConfig).
pub struct IconTheme {
    pub text: u32,
    pub highlight_bg: u32,
    pub highlight_text: u32,
    pub titlebar_bg: u32,
    pub titlebar_text: u32,
    pub border: u32,
}

/// Return `text` shortened with a trailing `…` so its rendered width fits
/// within `max_width`. Returns the original string if it already fits.
fn truncate_with_ellipsis(
    text: &str,
    max_width: i32,
    font: Option<&str>,
    font_size: f32,
) -> String {
    if max_width <= 0 || text.is_empty() {
        return String::new();
    }
    let measure = |s: &str| super::font::measure_text(font, font_size, s).unwrap_or(0.0) as i32;
    if measure(text) <= max_width {
        return text.to_string();
    }
    const ELLIPSIS: &str = "\u{2026}";
    let ell_w = measure(ELLIPSIS);
    if ell_w >= max_width {
        return ELLIPSIS.to_string();
    }
    let target = max_width - ell_w;
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect();
        if measure(&candidate) <= target {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out: String = chars[..lo].iter().collect();
    out.push_str(ELLIPSIS);
    out
}

fn rgba_to_argb(rgba: u32) -> u32 {
    let r = (rgba >> 24) & 0xff;
    let g = (rgba >> 16) & 0xff;
    let b = (rgba >> 8) & 0xff;
    let a = rgba & 0xff;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Load an icon for the given app_id.
///
/// Priority: user override (`~/.config/canoe/icons/`) > XDG desktop icon > None (first-char).
pub fn load_icon_for_app(app_id: &str, size_px: i32) -> Option<tiny_skia::Pixmap> {
    let size = size_px.max(1) as u32;

    // 1. User override from ~/.config/canoe/icons/
    if let Ok(home) = std::env::var("HOME") {
        let dir = std::path::PathBuf::from(home)
            .join(".config")
            .join("canoe")
            .join("icons");
        if let Some(pixmap) = load_icon_file(&dir, app_id, size) {
            return Some(pixmap);
        }
    }

    // 2. XDG desktop file icon lookup
    if let Some(pixmap) = load_xdg_icon(app_id, size) {
        return Some(pixmap);
    }

    None
}

/// Try loading `<name>.svg` or `<name>.png` from the given directory.
fn load_icon_file(dir: &std::path::Path, name: &str, size: u32) -> Option<tiny_skia::Pixmap> {
    let svg_path = dir.join(format!("{}.svg", name));
    if let Some(pixmap) = rasterize_svg_file(&svg_path, size) {
        return Some(pixmap);
    }
    let png_path = dir.join(format!("{}.png", name));
    load_png_file(&png_path, size)
}

fn rasterize_svg_file(path: &std::path::Path, size: u32) -> Option<tiny_skia::Pixmap> {
    let svg_data = std::fs::read_to_string(path).ok()?;
    rasterize_svg(&svg_data, size)
}

fn rasterize_svg(svg_data: &str, size: u32) -> Option<tiny_skia::Pixmap> {
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_str(svg_data, &opt).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let tree_size = tree.size();
    let scale_x = size as f32 / tree_size.width();
    let scale_y = size as f32 / tree_size.height();
    let scale = scale_x.min(scale_y);
    let scaled_w = tree_size.width() * scale;
    let scaled_h = tree_size.height() * scale;
    let tx = (size as f32 - scaled_w) * 0.5;
    let ty = (size as f32 - scaled_h) * 0.5;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap)
}

fn load_png_file(path: &std::path::Path, size: u32) -> Option<tiny_skia::Pixmap> {
    let data = std::fs::read(path).ok()?;
    let src = tiny_skia::Pixmap::decode_png(&data).ok()?;
    scale_pixmap(&src, size)
}

/// Look up an icon via XDG desktop files and the hicolor icon theme.
fn load_xdg_icon(app_id: &str, size: u32) -> Option<tiny_skia::Pixmap> {
    let icon_name = read_desktop_icon_name(app_id)?;

    // If the Icon value is an absolute path, load it directly.
    let icon_path = std::path::Path::new(&icon_name);
    if icon_path.is_absolute() {
        return load_icon_by_path(icon_path, size);
    }

    // Search hicolor icon theme across XDG data directories.
    find_hicolor_icon(&icon_name, size)
}

/// Find `<app-id>.desktop` in XDG data dirs and return the `Icon=` value.
fn read_desktop_icon_name(app_id: &str) -> Option<String> {
    let filename = format!("{}.desktop", app_id);
    for dir in xdg_data_dirs() {
        let path = dir.join("applications").join(&filename);
        if let Some(name) = parse_desktop_icon(&path) {
            return Some(name);
        }
    }
    None
}

/// Parse a .desktop file and return the Icon= value from [Desktop Entry].
fn parse_desktop_icon(path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if in_entry {
            if let Some(value) = line.strip_prefix("Icon=") {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Load an icon from an absolute file path (SVG or PNG).
fn load_icon_by_path(path: &std::path::Path, size: u32) -> Option<tiny_skia::Pixmap> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("svg") => rasterize_svg_file(path, size),
        Some("png") => load_png_file(path, size),
        _ => None,
    }
}

/// Search the hicolor icon theme for the given icon name, returning the
/// largest available rasterized to `size`.
fn find_hicolor_icon(icon_name: &str, size: u32) -> Option<tiny_skia::Pixmap> {
    // Prefer scalable SVG, then the largest fixed-size icon.
    // Standard hicolor sizes, largest first.
    const SIZES: &[u32] = &[512, 256, 128, 96, 72, 64, 48, 36, 32, 24, 22, 16];

    for dir in xdg_data_dirs() {
        let theme_dir = dir.join("icons").join("hicolor");

        // Try scalable first.
        let svg = theme_dir
            .join("scalable")
            .join("apps")
            .join(format!("{}.svg", icon_name));
        if let Some(pixmap) = rasterize_svg_file(&svg, size) {
            return Some(pixmap);
        }

        // Try fixed sizes, largest first.
        for &s in SIZES {
            let subdir = format!("{}x{}", s, s);
            let png = theme_dir
                .join(&subdir)
                .join("apps")
                .join(format!("{}.png", icon_name));
            if let Some(pixmap) = load_png_file(&png, size) {
                return Some(pixmap);
            }
            let svg = theme_dir
                .join(&subdir)
                .join("apps")
                .join(format!("{}.svg", icon_name));
            if let Some(pixmap) = rasterize_svg_file(&svg, size) {
                return Some(pixmap);
            }
        }
    }

    // Last resort: check pixmaps directories.
    for dir in xdg_data_dirs() {
        let pixmaps = dir.join("pixmaps");
        let png = pixmaps.join(format!("{}.png", icon_name));
        if let Some(pixmap) = load_png_file(&png, size) {
            return Some(pixmap);
        }
        let svg = pixmaps.join(format!("{}.svg", icon_name));
        if let Some(pixmap) = rasterize_svg_file(&svg, size) {
            return Some(pixmap);
        }
    }

    None
}

/// Return XDG data directories: $XDG_DATA_HOME then $XDG_DATA_DIRS.
fn xdg_data_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();

    // $XDG_DATA_HOME (default: $HOME/.local/share)
    if let Ok(v) = std::env::var("XDG_DATA_HOME") {
        if !v.is_empty() {
            dirs.push(std::path::PathBuf::from(v));
        }
    } else if let Ok(home) = std::env::var("HOME") {
        dirs.push(std::path::PathBuf::from(home).join(".local").join("share"));
    }

    // $XDG_DATA_DIRS (default: /usr/local/share:/usr/share)
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    if data_dirs.is_empty() {
        dirs.push(std::path::PathBuf::from("/usr/local/share"));
        dirs.push(std::path::PathBuf::from("/usr/share"));
    } else {
        for p in data_dirs.split(':') {
            if !p.is_empty() {
                dirs.push(std::path::PathBuf::from(p));
            }
        }
    }

    dirs
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
