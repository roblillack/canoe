//! Desktop icon surface (bottom layer) + shared surface machinery also used by the background fill
#![allow(dead_code)]

use resvg::{tiny_skia, usvg};
use std::rc::Rc;

use super::render::{ColorGlyphs, Renderer};
use super::scale::{FractionalSurface, Origin, Scale};
use super::surface::{Buffer, LayerSurface, Region, Surface};
use super::{OutputId, WindowId};

// Icon layout constants (logical pixels)
pub const ICON_SIZE: i32 = 48;
const ICON_LABEL_HEIGHT: i32 = 19;
const ICON_CELL_WIDTH: i32 = 96;
const ICON_CELL_HEIGHT: i32 = 72;
const ICON_MARGIN: i32 = 5;
/// How far the selected label's background reaches past the text.
const LABEL_MARGIN: i32 = 3;

/// Data for a minimized window icon.
pub struct DesktopIcon {
    pub window_id: WindowId,
    pub title: String,
    pub app_id: Option<String>,
    pub icon: Option<Rc<tiny_skia::Pixmap>>,
}

/// Computed icon cell on the desktop surface, in buffer pixels.
struct DesktopIconLayout {
    window_id: WindowId,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Desktop surface with minimized window icons.
pub struct DesktopSurface {
    pub surface: Surface,
    pub layer_surface: LayerSurface,
    pub fractional: FractionalSurface,
    pub buffer: Option<Buffer>,
    pub width: i32,
    pub height: i32,
    buf_width: i32,
    buf_height: i32,
    /// The scale the buffer is drawn at.
    pub scale: Scale,
    pub configured: bool,
    pub output_id: OutputId,
    pub selected_icon: Option<WindowId>,
    pub icon_cols: i32,
    icons: Vec<DesktopIconLayout>,
    pub dirty: bool,
}

impl DesktopSurface {
    pub fn new(
        surface: Surface,
        layer_surface: LayerSurface,
        fractional: FractionalSurface,
        output_id: OutputId,
    ) -> Self {
        Self {
            surface,
            layer_surface,
            fractional,
            buffer: None,
            width: 0,
            height: 0,
            buf_width: 0,
            buf_height: 0,
            scale: Scale::default(),
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
        self.buffer = None;
        self.dirty = true;
    }

    pub fn ensure_buffer(&mut self, scale: Scale) {
        if self.width <= 0 || self.height <= 0 {
            return;
        }

        let (buf_w, buf_h) = self.fractional.apply(
            &self.surface,
            self.width,
            self.height,
            &scale,
            // The desktop covers its output from the corner.
            Origin::default(),
        );
        self.scale = scale;
        self.allocate_buffer(buf_w, buf_h);
    }

    /// Ensure a buffer for a surface drawn in a single color: one pixel
    /// stretched over the surface when it has a viewport, else a full-size
    /// buffer at scale 1.
    pub fn ensure_solid_buffer(&mut self) {
        if self.width <= 0 || self.height <= 0 {
            return;
        }

        let scale = Scale::default();
        let (buf_w, buf_h) = self.fractional.apply(
            &self.surface,
            self.width,
            self.height,
            &scale,
            Origin::default(),
        );
        self.scale = scale;
        if self.fractional.has_viewport() {
            self.allocate_buffer(1, 1);
        } else {
            self.allocate_buffer(buf_w, buf_h);
        }
    }

    /// (Re)allocate the buffer unless one of this size already exists.
    fn allocate_buffer(&mut self, buf_w: i32, buf_h: i32) {
        if self.buffer.is_some() && self.buf_width == buf_w && self.buf_height == buf_h {
            return;
        }
        self.reset_buffer();

        let Some(buffer) = Buffer::new(buf_w, buf_h) else {
            return;
        };
        self.buffer = Some(buffer);
        self.buf_width = buf_w;
        self.buf_height = buf_h;
    }

    /// Fill the whole buffer with `rgba` (0xRRGGBBAA, straight alpha).
    pub fn render(&mut self, rgba: u32) {
        if let Some(ref buffer) = self.buffer {
            let argb = rgba_to_premultiplied_argb(rgba);
            let color_bytes = argb.to_ne_bytes();
            for pixel in buffer.pixels_mut().as_chunks_mut::<4>().0 {
                *pixel = color_bytes;
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

        let scale = self.scale;
        let buf_w = self.buf_width;
        let buf_h = self.buf_height;
        if buf_w <= 0 || buf_h <= 0 {
            return;
        }

        // Compute icon grid layout (bottom-left, left-to-right, then
        // bottom-to-top), in logical pixels taken to buffer pixels.
        let cell_w = scale.px(ICON_CELL_WIDTH);
        let cell_h = scale.px(ICON_CELL_HEIGHT);
        let cols = ((buf_w - scale.px(ICON_MARGIN * 2)) / cell_w.max(1)).max(1);
        self.icon_cols = cols;

        let mut layouts = Vec::with_capacity(desktop_icons.len());
        for (i, icon) in desktop_icons.iter().enumerate() {
            let col = i as i32 % cols;
            let row = i as i32 / cols;
            let x = scale.px(ICON_MARGIN + col * ICON_CELL_WIDTH);
            let y = buf_h - scale.px(ICON_MARGIN + ICON_CELL_HEIGHT + row * ICON_CELL_HEIGHT);
            layouts.push(DesktopIconLayout {
                window_id: icon.window_id,
                x,
                y,
                width: cell_w,
                height: cell_h,
            });
        }
        self.icons = layouts;

        // Now render each icon
        let Some(buffer) = self.buffer.clone() else {
            return;
        };
        let mut pixels = buffer.pixels_mut();
        let pixels = pixels.as_mut_slice();
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
            let color_glyphs = if is_selected {
                ColorGlyphs::Natural
            } else {
                ColorGlyphs::Tinted
            };

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

            let ix = layout.x;
            let iy = layout.y;
            let icon_px = scale.px(ICON_SIZE);

            // Center the 32x32 icon square within the cell width
            let icon_offset_x = (cell_w - icon_px) / 2;

            let has_icon_image = icon.icon.is_some();

            if !has_icon_image {
                // Draw icon background (32x32 area)
                renderer.fill_rect(ix + icon_offset_x, iy, icon_px, icon_px, icon_bg);

                // Draw 1px border around icon square
                let border_color = rgba_to_argb(theme.border);
                let b = scale.line();
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
                    scale.factor(),
                    icon_text,
                    color_glyphs,
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
            let label_h = scale.px(ICON_LABEL_HEIGHT);
            let scaled_label_size = scale.px_f(label_font_size);
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
                let margin = scale.px(LABEL_MARGIN);
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
                scale.factor(),
                label_text,
                color_glyphs,
                label_font_size,
                label_font_name,
                0,
            );
        }
    }

    /// Hit test: return the window id of the icon at the given surface-local
    /// coordinates. Coordinates are in logical (surface-local) pixels;
    /// layouts are stored in buffer pixels.
    pub fn icon_at(&self, x: i32, y: i32) -> Option<WindowId> {
        let (x, y) = (self.scale.px(x), self.scale.px(y));
        for layout in &self.icons {
            if x >= layout.x
                && x < layout.x + layout.width
                && y >= layout.y
                && y < layout.y + layout.height
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

    /// The minimized-icon cells always accept input. With `desktop_area`,
    /// so does that surface-local rectangle (right-click opens the window
    /// menu, left-click unfocuses); everything else falls through to clients
    /// below (e.g. shaderbg). No icons and no area = fully click-through.
    ///
    /// Never pass `None` to set_input_region here: `None` means an
    /// *infinite* region; an empty region means "no input".
    pub fn update_input_region(&self, desktop_area: Option<(i32, i32, i32, i32)>) {
        if self.width <= 0 || self.height <= 0 {
            return;
        }

        let mut region = Region::new();
        if let Some((x, y, width, height)) = desktop_area {
            region.add(x, y, width, height);
        }
        // Layouts are in buffer pixels; the input region is in surface-local
        // logical pixels. Round outwards so the region covers every device
        // pixel of the cell.
        let device = self.scale.device();
        for layout in &self.icons {
            let x0 = (layout.x as f64 / device).floor() as i32;
            let y0 = (layout.y as f64 / device).floor() as i32;
            let x1 = ((layout.x + layout.width) as f64 / device).ceil() as i32;
            let y1 = ((layout.y + layout.height) as f64 / device).ceil() as i32;
            region.add(x0, y0, x1 - x0, y1 - y0);
        }
        self.surface.set_input_region(Some(&region));
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

/// The part of a desktop surface of `surface_size` on an output at
/// `output_origin` that takes clicks on the empty desktop: the output's usable
/// area (global coordinates), made surface-local.
///
/// Bars and panels on the bottom layer sit in the exclusive zones left out of
/// the usable area. River stacks surfaces of a layer in creation order, so a
/// bar created before the desktop surface (e.g. when Canoe restarts while the
/// bar keeps running) is below it and would otherwise get none of its clicks.
pub fn desktop_input_area(
    usable_area: (i32, i32, i32, i32),
    output_origin: (i32, i32),
    surface_size: (i32, i32),
) -> Option<(i32, i32, i32, i32)> {
    let (x, y, width, height) = usable_area;
    let x0 = (x - output_origin.0).max(0);
    let y0 = (y - output_origin.1).max(0);
    let x1 = (x - output_origin.0 + width).min(surface_size.0);
    let y1 = (y - output_origin.1 + height).min(surface_size.1);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
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
    // Cut only between clusters, so emoji sequences and combining marks
    // are never split. `ends[n]` is the byte length of the first n clusters.
    let ends: Vec<usize> = std::iter::once(0)
        .chain(super::font::clusters(text).into_iter().map(|c| c.end))
        .collect();
    let mut lo = 0usize;
    let mut hi = ends.len() - 1;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if measure(&text[..ends[mid]]) <= target {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out = text[..ends[lo]].to_string();
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

/// Convert straight-alpha 0xRRGGBBAA to the premultiplied ARGB that
/// `wl_shm` buffers hold.
fn rgba_to_premultiplied_argb(rgba: u32) -> u32 {
    let a = rgba & 0xff;
    let premul = |c: u32| (c * a + 127) / 255;
    let r = premul((rgba >> 24) & 0xff);
    let g = premul((rgba >> 16) & 0xff);
    let b = premul((rgba >> 8) & 0xff);
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
        self.buffer = None;
        self.fractional.destroy();
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::{desktop_input_area, rgba_to_premultiplied_argb};

    #[test]
    fn desktop_input_area_leaves_out_exclusive_zones() {
        // A 30px bar at the top of an output at (1920, 0).
        assert_eq!(
            desktop_input_area((1920, 30, 1920, 1050), (1920, 0), (1920, 1080)),
            Some((0, 30, 1920, 1050))
        );
        // No exclusive zones: the whole surface.
        assert_eq!(
            desktop_input_area((0, 0, 1920, 1080), (0, 0), (1920, 1080)),
            Some((0, 0, 1920, 1080))
        );
        // Clamped to the surface, e.g. while the output's size changes.
        assert_eq!(
            desktop_input_area((0, 0, 2560, 1440), (0, 0), (1920, 1080)),
            Some((0, 0, 1920, 1080))
        );
        // Nothing left.
        assert_eq!(desktop_input_area((0, 0, 0, 0), (0, 0), (1920, 1080)), None);
    }

    #[test]
    fn premultiplies_fill_color() {
        assert_eq!(rgba_to_premultiplied_argb(0x008080FF), 0xFF008080);
        assert_eq!(rgba_to_premultiplied_argb(0xFFFFFF80), 0x80808080);
        assert_eq!(rgba_to_premultiplied_argb(0x12345600), 0x00000000);
    }
}
