//! Window menu rendering and interaction.

use crate::config::UiConfig;
use memmap2::MmapMut;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::os::fd::AsFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::QueueHandle;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

use super::{font, render::Renderer, shadow::draw_shadow_soft, shmfile, OutputId, WindowId};

/// Menu entry data.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub window_id: WindowId,
    pub title: String,
    pub hidden: bool,
    pub active: bool,
}

/// Window menu surface and state.
pub struct WindowMenu {
    pub surface: wl_surface::WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    pub buffer: Option<wl_buffer::WlBuffer>,
    pub pool: Option<wl_shm_pool::WlShmPool>,
    pub memfile: Option<File>,
    pub mmap: Option<MmapMut>,
    pub width: i32,
    pub height: i32,
    pub buffer_width: i32,
    pub buffer_height: i32,
    pub scale: i32,
    pub configured: bool,
    pub items: Vec<MenuItem>,
    pub header_title: Option<String>,
    pub hovered: Option<usize>,
    pub output_id: OutputId,
    pub origin_x: i32,
    pub origin_y: i32,
    pub theme: MenuTheme,
    cache: Option<MenuCache>,
}

const MENU_BORDER: i32 = 1;
const ITEM_PADDING_X: i32 = 8;
const ITEM_PADDING_Y: i32 = 4;
const ICON_SIZE: i32 = 10;
const ICON_GAP: i32 = 6;
const ACTIVE_DIAMOND_SIZE: i32 = 8;

/// Fallback flat drop-shadow size used when soft shadows are disabled.
const L_SHADOW_SIZE: i32 = 3;
const L_SHADOW_COLOR: u32 = 0x404040FF;

const BORDER_COLOR: u32 = 0x000000FF;

#[derive(Debug, Clone)]
pub struct MenuTheme {
    pub font_name: Option<String>,
    pub font_size: f32,
    pub bg: u32,
    pub text: u32,
    pub highlight_bg: u32,
    pub highlight_text: u32,
    pub titlebar_bg: u32,
    pub titlebar_text: u32,
    /// When true, render a soft drop shadow with [`shadow_size`] / [`shadow_color`].
    /// When false, fall back to the flat L-shape drop shadow.
    pub shadows_enabled: bool,
    pub shadow_size: i32,
    pub shadow_color: u32,
}

#[derive(Clone, Debug, PartialEq)]
struct MenuCacheKey {
    width: i32,
    height: i32,
    scale: i32,
    font_size: f32,
    bg: u32,
    text: u32,
    highlight_bg: u32,
    highlight_text: u32,
    titlebar_bg: u32,
    titlebar_text: u32,
    items_hash: u64,
    header_hash: u64,
}

#[derive(Clone, Debug)]
struct MenuCache {
    key: MenuCacheKey,
    pixels: Vec<u8>,
}

impl MenuTheme {
    pub fn from_ui(ui: &UiConfig) -> Self {
        Self {
            font_name: ui.font_name.clone(),
            font_size: ui.font_size,
            bg: ui.menu_bg,
            text: ui.menu_text,
            highlight_bg: ui.menu_highlight_bg,
            highlight_text: ui.menu_highlight_text,
            titlebar_bg: ui.titlebar_bg_inactive,
            titlebar_text: ui.titlebar_text_inactive,
            shadows_enabled: ui.shadows_enabled,
            shadow_size: ui.shadows_active_size.max(0),
            shadow_color: ui.shadows_color,
        }
    }

    /// Pixels of shadow that extend to the left of the menu content.
    pub fn shadow_left(&self) -> i32 {
        if self.shadows_enabled {
            self.shadow_size
        } else {
            0
        }
    }

    /// Pixels of shadow that extend above the menu content. Soft mode uses
    /// half the nominal size so the shadow looks like it's cast by a light
    /// source above the menu (matching the window drop shadow).
    pub fn shadow_top(&self) -> i32 {
        if self.shadows_enabled {
            self.shadow_size / 2
        } else {
            0
        }
    }

    /// Pixels of shadow that extend to the right of the menu content.
    pub fn shadow_right(&self) -> i32 {
        if self.shadows_enabled {
            self.shadow_size
        } else {
            L_SHADOW_SIZE
        }
    }

    /// Pixels of shadow that extend below the menu content. Soft mode uses
    /// 1.5x the nominal size to account for the downward offset of the
    /// rendered shape; matches the window drop shadow.
    pub fn shadow_bottom(&self) -> i32 {
        if self.shadows_enabled {
            self.shadow_size + self.shadow_size / 2
        } else {
            L_SHADOW_SIZE
        }
    }
}

impl WindowMenu {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        surface: wl_surface::WlSurface,
        layer_surface: ZwlrLayerSurfaceV1,
        output_id: OutputId,
        items: Vec<MenuItem>,
        header_title: Option<String>,
        origin_x: i32,
        origin_y: i32,
        theme: MenuTheme,
    ) -> Self {
        let (width, height) = measure_menu(&items, &theme, header_title.as_deref());
        Self {
            surface,
            layer_surface,
            buffer: None,
            pool: None,
            memfile: None,
            mmap: None,
            width,
            height,
            buffer_width: width,
            buffer_height: height,
            scale: 1,
            configured: false,
            items,
            header_title,
            hovered: None,
            output_id,
            origin_x,
            origin_y,
            theme,
            cache: None,
        }
    }

    pub fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        let content_w = self.menu_width();
        let content_h = self.menu_height();
        let content_x = self.theme.shadow_left() + MENU_BORDER;
        let content_y = self.theme.shadow_top() + MENU_BORDER;
        let content_w = content_w - MENU_BORDER * 2;
        let content_h = content_h - MENU_BORDER * 2;
        if x < content_x
            || y < content_y
            || x >= content_x + content_w
            || y >= content_y + content_h
        {
            return None;
        }

        let header_h = self.header_height();
        let item_h = item_height(&self.theme);
        if y < content_y + header_h {
            return None;
        }
        let idx = ((y - content_y - header_h) / item_h) as usize;
        if idx < self.items.len() {
            Some(idx)
        } else {
            None
        }
    }

    pub fn update_hover(&mut self, x: i32, y: i32) -> bool {
        let next = self.item_at(x, y);
        if next != self.hovered {
            self.hovered = next;
            return true;
        }
        false
    }

    pub fn select_next(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let next = match self.hovered {
            Some(idx) => (idx + 1) % self.items.len(),
            None => 0,
        };
        if self.hovered != Some(next) {
            self.hovered = Some(next);
            return true;
        }
        false
    }

    pub fn select_window(&mut self, window_id: Option<WindowId>) -> bool {
        let next = window_id.and_then(|id| self.items.iter().position(|item| item.window_id == id));
        if self.hovered != next {
            self.hovered = next;
            return true;
        }
        false
    }

    pub fn ensure_buffer<D>(&mut self, shm: &wl_shm::WlShm, qh: &QueueHandle<D>, scale: i32)
    where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ()>,
    {
        if self.width <= 0 || self.height <= 0 {
            return;
        }

        let scale = scale.max(1);
        let buffer_width = self.width * scale;
        let buffer_height = self.height * scale;
        if buffer_width <= 0 || buffer_height <= 0 {
            return;
        }

        if self.buffer.is_some()
            && self.buffer_width == buffer_width
            && self.buffer_height == buffer_height
            && self.scale == scale
        {
            return;
        }

        self.surface.set_buffer_scale(scale);

        let stride = buffer_width * 4;
        let size = stride * buffer_height;
        let memfile = match shmfile::create("canoe-menu", size as i64) {
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
        let buffer = pool.create_buffer(
            0,
            buffer_width,
            buffer_height,
            stride,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );

        self.memfile = Some(memfile);
        self.mmap = Some(mmap);
        self.pool = Some(pool);
        self.buffer = Some(buffer);
        self.buffer_width = buffer_width;
        self.buffer_height = buffer_height;
        self.scale = scale;
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

    pub fn render(&mut self) {
        let menu_w = self.menu_width();
        let menu_h = self.menu_height();
        if menu_w <= 0 || menu_h <= 0 {
            return;
        }

        if !self.ensure_cache() {
            return;
        }

        let theme = self.theme.clone();
        let header_title = self.header_title.clone();
        let hovered_idx = self.hovered;
        let hovered_item = hovered_idx.and_then(|idx| self.items.get(idx).cloned());
        let item_h = item_height(&theme);
        let header_h = if header_title.is_some() { item_h } else { 0 };
        let scale = self.scale.max(1);
        let content_x_px = theme.shadow_left() * scale;
        let content_y_px = theme.shadow_top() * scale;
        let row_ctx = MenuRowContext {
            menu_w,
            item_h,
            scale,
            content_x_px,
            content_y_px,
            theme: &theme,
        };

        let Some(ref mut mmap) = self.mmap else {
            return;
        };

        let pixels = mmap.as_mut();
        let cache_pixels = match self.cache.as_ref() {
            Some(cache) => cache.pixels.as_slice(),
            None => return,
        };
        if cache_pixels.len() != pixels.len() {
            return;
        }
        pixels.copy_from_slice(cache_pixels);

        let mut renderer = match Renderer::new(pixels, self.buffer_width, self.buffer_height) {
            Some(renderer) => renderer,
            None => return,
        };

        if let (Some(idx), Some(item)) = (hovered_idx, hovered_item.as_ref()) {
            let row_y = MENU_BORDER + header_h + (idx as i32 * item_h);
            draw_menu_row(&mut renderer, item, row_y, true, &row_ctx);
        }
    }

    pub fn update_input_region<D>(
        &self,
        compositor: &wl_compositor::WlCompositor,
        qh: &QueueHandle<D>,
    ) where
        D: 'static + wayland_client::Dispatch<wl_region::WlRegion, ()>,
    {
        let menu_w = self.menu_width();
        let menu_h = self.menu_height();
        if menu_w <= 0 || menu_h <= 0 {
            return;
        }

        let region = compositor.create_region(qh, ());
        region.add(
            self.theme.shadow_left(),
            self.theme.shadow_top(),
            menu_w,
            menu_h,
        );
        self.surface.set_input_region(Some(&region));
        region.destroy();
    }

    pub fn commit(&self) {
        if let Some(ref buffer) = self.buffer {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface
                .damage_buffer(0, 0, self.buffer_width, self.buffer_height);
            self.surface.commit();
        }
    }

    fn menu_width(&self) -> i32 {
        (self.width - self.theme.shadow_left() - self.theme.shadow_right()).max(0)
    }

    fn menu_height(&self) -> i32 {
        (self.height - self.theme.shadow_top() - self.theme.shadow_bottom()).max(0)
    }

    /// Surface-local offset (x, y) of the point that should sit under the
    /// pointer when the menu is opened: horizontally one half-row-height in
    /// from the menu's left edge, vertically at the vertical center of the
    /// title (first) row.
    pub fn pointer_anchor(&self) -> (i32, i32) {
        let half_row = item_height(&self.theme) / 2;
        (
            self.theme.shadow_left() + half_row,
            self.theme.shadow_top() + MENU_BORDER + half_row,
        )
    }

    fn header_height(&self) -> i32 {
        if self.header_title.is_some() {
            item_height(&self.theme)
        } else {
            0
        }
    }
}

impl Drop for WindowMenu {
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

fn item_height(theme: &MenuTheme) -> i32 {
    let font_h = theme.font_size.ceil() as i32;
    (font_h + ITEM_PADDING_Y * 2).max(1)
}

fn measure_menu(items: &[MenuItem], theme: &MenuTheme, header_title: Option<&str>) -> (i32, i32) {
    let mut max_width = 0.0f32;
    let mut has_font = false;
    for item in items {
        if let Some(width) =
            font::measure_text(theme.font_name.as_deref(), theme.font_size, &item.title)
        {
            has_font = true;
            if width > max_width {
                max_width = width;
            }
        }
    }
    if let Some(title) = header_title {
        if let Some(width) = font::measure_text(theme.font_name.as_deref(), theme.font_size, title)
        {
            has_font = true;
            if width > max_width {
                max_width = width;
            }
        }
    }
    let row_count = items.len() as i32 + if header_title.is_some() { 1 } else { 0 };
    let pad_w = theme.shadow_left() + theme.shadow_right();
    let pad_h = theme.shadow_top() + theme.shadow_bottom();
    if !has_font {
        let menu_w = 120;
        let menu_h = (row_count * item_height(theme)).max(1) + MENU_BORDER * 2;
        return (menu_w + pad_w, menu_h + pad_h);
    }

    let content_w = ITEM_PADDING_X * 2 + ICON_SIZE + ICON_GAP + max_width.ceil() as i32;
    let content_h = item_height(theme) * row_count;
    let menu_w = content_w + MENU_BORDER * 2;
    let menu_h = content_h + MENU_BORDER * 2;
    (menu_w + pad_w, menu_h + pad_h)
}

fn items_hash(items: &[MenuItem]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for item in items {
        item.window_id.hash(&mut hasher);
        item.title.hash(&mut hasher);
        item.hidden.hash(&mut hasher);
        item.active.hash(&mut hasher);
    }
    hasher.finish()
}

fn header_hash(header_title: Option<&str>) -> u64 {
    let Some(title) = header_title else {
        return 0;
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut hasher);
    hasher.finish()
}

impl WindowMenu {
    fn cache_key(&self) -> MenuCacheKey {
        MenuCacheKey {
            width: self.buffer_width,
            height: self.buffer_height,
            scale: self.scale,
            font_size: self.theme.font_size,
            bg: self.theme.bg,
            text: self.theme.text,
            highlight_bg: self.theme.highlight_bg,
            highlight_text: self.theme.highlight_text,
            titlebar_bg: self.theme.titlebar_bg,
            titlebar_text: self.theme.titlebar_text,
            items_hash: items_hash(&self.items),
            header_hash: header_hash(self.header_title.as_deref()),
        }
    }

    fn ensure_cache(&mut self) -> bool {
        let key = self.cache_key();
        let needs_rebuild = match self.cache {
            Some(ref cache) => cache.key != key,
            None => true,
        };

        if !needs_rebuild {
            return true;
        }

        let mut pixels = vec![0u8; (self.buffer_width * self.buffer_height * 4) as usize];
        clear_buffer(&mut pixels);
        let mut renderer = match Renderer::new(&mut pixels, self.buffer_width, self.buffer_height) {
            Some(renderer) => renderer,
            None => return false,
        };

        let menu_w = self.menu_width();
        let menu_h = self.menu_height();
        let scale = self.scale.max(1);
        let menu_w_px = menu_w * scale;
        let menu_h_px = menu_h * scale;
        let content_x_px = self.theme.shadow_left() * scale;
        let content_y_px = self.theme.shadow_top() * scale;

        draw_menu_shadow(
            &mut renderer,
            &self.theme,
            menu_w,
            menu_h,
            content_x_px,
            content_y_px,
            scale,
        );

        renderer.fill_rect(
            content_x_px,
            content_y_px,
            menu_w_px,
            menu_h_px,
            rgba_to_argb(self.theme.bg),
        );

        draw_border_rect(
            &mut renderer,
            content_x_px,
            content_y_px,
            menu_w_px,
            menu_h_px,
            rgba_to_argb(BORDER_COLOR),
        );

        let item_h = item_height(&self.theme);
        let header_h = self.header_height();
        let row_ctx = MenuRowContext {
            menu_w,
            item_h,
            scale,
            content_x_px,
            content_y_px,
            theme: &self.theme,
        };
        if let Some(title) = self.header_title.as_deref() {
            draw_menu_header(&mut renderer, title, MENU_BORDER, &row_ctx);
        }
        for (idx, item) in self.items.iter().enumerate() {
            let row_y = MENU_BORDER + header_h + (idx as i32 * item_h);
            draw_menu_row(&mut renderer, item, row_y, false, &row_ctx);
        }

        self.cache = Some(MenuCache { key, pixels });
        true
    }
}

struct MenuRowContext<'a> {
    menu_w: i32,
    item_h: i32,
    scale: i32,
    /// Buffer-space x offset of the menu content area (left edge), in pixels.
    content_x_px: i32,
    /// Buffer-space y offset of the menu content area (top edge), in pixels.
    content_y_px: i32,
    theme: &'a MenuTheme,
}

fn draw_menu_header(renderer: &mut Renderer, title: &str, row_y: i32, ctx: &MenuRowContext<'_>) {
    let bg = rgba_to_argb(ctx.theme.titlebar_bg);
    let text_color = rgba_to_argb(ctx.theme.titlebar_text);

    renderer.fill_rect(
        ctx.content_x_px + MENU_BORDER * ctx.scale,
        ctx.content_y_px + row_y * ctx.scale,
        (ctx.menu_w - MENU_BORDER * 2) * ctx.scale,
        ctx.item_h * ctx.scale,
        bg,
    );

    let text_start_x = MENU_BORDER + ITEM_PADDING_X;
    let text_area_w = ctx.menu_w - MENU_BORDER * 2 - text_start_x + MENU_BORDER;
    renderer.render_text(
        title,
        ctx.content_x_px + text_start_x * ctx.scale,
        ctx.content_y_px + row_y * ctx.scale,
        text_area_w * ctx.scale,
        ctx.item_h * ctx.scale,
        ctx.scale,
        text_color,
        ctx.theme.font_size,
        ctx.theme.font_name.as_deref(),
        0,
    );
}

fn draw_menu_row(
    renderer: &mut Renderer,
    item: &MenuItem,
    row_y: i32,
    is_active: bool,
    ctx: &MenuRowContext<'_>,
) {
    let bg = if is_active {
        rgba_to_argb(ctx.theme.highlight_bg)
    } else {
        rgba_to_argb(ctx.theme.bg)
    };
    let text_color = if is_active {
        rgba_to_argb(ctx.theme.highlight_text)
    } else {
        rgba_to_argb(ctx.theme.text)
    };
    let icon_color = if is_active {
        text_color
    } else {
        rgba_to_argb(ctx.theme.text)
    };

    renderer.fill_rect(
        ctx.content_x_px + MENU_BORDER * ctx.scale,
        ctx.content_y_px + row_y * ctx.scale,
        (ctx.menu_w - MENU_BORDER * 2) * ctx.scale,
        ctx.item_h * ctx.scale,
        bg,
    );

    let start_x = MENU_BORDER + ITEM_PADDING_X;
    if item.hidden {
        draw_dashed_rect(
            renderer,
            ctx.content_x_px + start_x * ctx.scale,
            ctx.content_y_px + (row_y + (ctx.item_h - ICON_SIZE) / 2) * ctx.scale,
            ICON_SIZE * ctx.scale,
            ICON_SIZE * ctx.scale,
            icon_color,
        );
    }
    if item.active {
        draw_diamond(
            renderer,
            ctx.content_x_px + (start_x + (ICON_SIZE - ACTIVE_DIAMOND_SIZE) / 2) * ctx.scale,
            ctx.content_y_px + (row_y + (ctx.item_h - ACTIVE_DIAMOND_SIZE) / 2) * ctx.scale,
            ACTIVE_DIAMOND_SIZE * ctx.scale,
            text_color,
        );
    }

    let text_start_x = start_x + ICON_SIZE + ICON_GAP;
    let text_area_w = ctx.menu_w - MENU_BORDER * 2 - text_start_x + MENU_BORDER;
    renderer.render_text(
        &item.title,
        ctx.content_x_px + text_start_x * ctx.scale,
        ctx.content_y_px + row_y * ctx.scale,
        text_area_w * ctx.scale,
        ctx.item_h * ctx.scale,
        ctx.scale,
        text_color,
        ctx.theme.font_size,
        ctx.theme.font_name.as_deref(),
        0,
    );
}

fn rgba_to_argb(rgba: u32) -> u32 {
    let r = (rgba >> 24) & 0xff;
    let g = (rgba >> 16) & 0xff;
    let b = (rgba >> 8) & 0xff;
    let a = rgba & 0xff;
    (a << 24) | (r << 16) | (g << 8) | b
}

fn clear_buffer(pixels: &mut [u8]) {
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.copy_from_slice(&[0, 0, 0, 0]);
    }
}

fn draw_border_rect(
    renderer: &mut Renderer,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    color_argb: u32,
) {
    if width <= 0 || height <= 0 {
        return;
    }

    renderer.fill_rect(x, y, width, 1, color_argb);
    renderer.fill_rect(x, y + height - 1, width, 1, color_argb);
    renderer.fill_rect(x, y, 1, height, color_argb);
    renderer.fill_rect(x + width - 1, y, 1, height, color_argb);
}

fn draw_menu_shadow(
    renderer: &mut Renderer,
    theme: &MenuTheme,
    menu_w: i32,
    menu_h: i32,
    content_x_px: i32,
    content_y_px: i32,
    scale: i32,
) {
    if menu_w <= 0 || menu_h <= 0 {
        return;
    }

    if theme.shadows_enabled {
        if theme.shadow_size <= 0 {
            return;
        }
        // Shrink the rendered shape height by size/2 so the falloff sits
        // lower in the buffer; combined with the asymmetric padding from
        // MenuTheme::shadow_top/bottom this produces the same "light from
        // above" offset as the window drop shadow.
        let frame_h_shrunk = (menu_h - theme.shadow_size / 2).max(1);
        draw_shadow_soft(
            renderer,
            menu_w,
            frame_h_shrunk,
            theme.shadow_size,
            0,
            theme.shadow_color,
            scale,
        );
    } else {
        let l_size = L_SHADOW_SIZE * scale;
        let menu_w_px = menu_w * scale;
        let menu_h_px = menu_h * scale;
        let color = rgba_to_argb(L_SHADOW_COLOR);
        renderer.fill_rect(
            content_x_px + l_size,
            content_y_px + menu_h_px,
            menu_w_px,
            l_size,
            color,
        );
        renderer.fill_rect(
            content_x_px + menu_w_px,
            content_y_px + l_size,
            l_size,
            menu_h_px,
            color,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_dashed_rect(
    renderer: &mut Renderer,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    color_argb: u32,
) {
    let dash = 2;
    let gap = 2;
    let mut px = x;
    while px < x + width {
        let segment = (x + width - px).min(dash);
        renderer.fill_rect(px, y, segment, 1, color_argb);
        renderer.fill_rect(px, y + height - 1, segment, 1, color_argb);
        px += dash + gap;
    }

    let mut py = y;
    while py < y + height {
        let segment = (y + height - py).min(dash);
        renderer.fill_rect(x, py, 1, segment, color_argb);
        renderer.fill_rect(x + width - 1, py, 1, segment, color_argb);
        py += dash + gap;
    }
}

fn draw_diamond(renderer: &mut Renderer, x: i32, y: i32, size: i32, color_argb: u32) {
    let half = size / 2;
    for row in 0..size {
        let dist = (half - row).abs();
        let span = size - dist * 2;
        let draw_x = x + dist;
        renderer.fill_rect(draw_x, y + row, span.max(1), 1, color_argb);
    }
}
