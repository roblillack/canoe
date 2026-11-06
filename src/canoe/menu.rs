//! Window menu rendering and interaction.

use crate::binding::Direction;
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

use super::scale::{FractionalSurface, Origin, Scale};
use super::shadow::{draw_shadow_soft, SoftShadowRect};
use super::titlebar::Rect;
use super::{font, render::Renderer, shmfile, OutputId, WindowId};

/// Menu entry data.
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub window_id: WindowId,
    pub title: String,
    pub hidden: bool,
    pub active: bool,
}

/// Window menu surface and state.
///
/// The menu is laid out in logical pixels and drawn at the surface's
/// [`Scale`].
pub struct WindowMenu {
    pub surface: wl_surface::WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    pub fractional: FractionalSurface,
    pub buffer: Option<wl_buffer::WlBuffer>,
    pub pool: Option<wl_shm_pool::WlShmPool>,
    pub memfile: Option<File>,
    pub mmap: Option<MmapMut>,
    /// Surface width (logical px)
    pub width: i32,
    /// Surface height (logical px)
    pub height: i32,
    pub buffer_width: i32,
    pub buffer_height: i32,
    pub scale: Scale,
    pub configured: bool,
    pub items: Vec<MenuItem>,
    pub header_title: Option<String>,
    pub hovered: Option<usize>,
    pub output_id: OutputId,
    pub origin_x: i32,
    pub origin_y: i32,
    pub theme: MenuTheme,
    /// Width of the rows' content, between the borders (logical px)
    inner_width: i32,
    cache: Option<MenuCache>,
}

// Layout metrics, in logical pixels.
const ITEM_PADDING_X: i32 = 11;
const ITEM_PADDING_Y: i32 = 5;
const ICON_SIZE: i32 = 13;
const ICON_GAP: i32 = 8;
const ACTIVE_DIAMOND_SIZE: i32 = 11;
const FALLBACK_WIDTH: i32 = 160;

/// Fallback flat drop-shadow size used when soft shadows are disabled.
const L_SHADOW_SIZE: i32 = 4;
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
    scale: Scale,
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

    /// Logical pixels of shadow that extend to the left of the menu content.
    pub fn shadow_left(&self) -> i32 {
        if self.shadows_enabled {
            self.shadow_size
        } else {
            0
        }
    }

    /// Logical pixels of shadow that extend above the menu content. Soft mode
    /// uses half the nominal size so the shadow looks like it's cast by a
    /// light source above the menu (matching the window drop shadow).
    pub fn shadow_top(&self) -> i32 {
        if self.shadows_enabled {
            self.shadow_size / 2
        } else {
            0
        }
    }

    /// Logical pixels of shadow that extend to the right of the menu content.
    pub fn shadow_right(&self) -> i32 {
        if self.shadows_enabled {
            self.shadow_size
        } else {
            L_SHADOW_SIZE
        }
    }

    /// Logical pixels of shadow that extend below the menu content. Soft mode
    /// uses 1.5x the nominal size to account for the downward offset of the
    /// rendered shape; matches the window drop shadow.
    pub fn shadow_bottom(&self) -> i32 {
        if self.shadows_enabled {
            self.shadow_size + self.shadow_size / 2
        } else {
            L_SHADOW_SIZE
        }
    }
}

/// Where the menu's parts land in the buffer, in device pixels.
struct MenuGeometry {
    /// The menu box, border included.
    menu: Rect,
    /// Thin-line thickness: the border.
    line: i32,
}

impl MenuGeometry {
    /// The device-pixel rows `[top, bottom)` of the row starting `top`
    /// logical pixels below the top border.
    fn row(&self, scale: &Scale, top: i32, height: i32) -> (i32, i32) {
        let origin = self.menu.y + self.line;
        (origin + scale.px(top), origin + scale.px(top + height))
    }
}

impl WindowMenu {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        surface: wl_surface::WlSurface,
        layer_surface: ZwlrLayerSurfaceV1,
        fractional: FractionalSurface,
        output_id: OutputId,
        items: Vec<MenuItem>,
        header_title: Option<String>,
        origin_x: i32,
        origin_y: i32,
        theme: MenuTheme,
        scale: Scale,
    ) -> Self {
        let inner_width = measure_inner_width(&items, &theme, header_title.as_deref(), &scale);
        let row_count = items.len() as i32 + i32::from(header_title.is_some());
        let width = theme.shadow_left() + inner_width + 2 + theme.shadow_right();
        let height =
            theme.shadow_top() + row_count * item_height(&theme) + 2 + theme.shadow_bottom();
        Self {
            surface,
            layer_surface,
            fractional,
            buffer: None,
            pool: None,
            memfile: None,
            mmap: None,
            width,
            height,
            buffer_width: width,
            buffer_height: height,
            scale,
            configured: false,
            items,
            header_title,
            hovered: None,
            output_id,
            origin_x,
            origin_y,
            theme,
            inner_width,
            cache: None,
        }
    }

    fn row_count(&self) -> i32 {
        self.items.len() as i32 + i32::from(self.header_title.is_some())
    }

    fn geometry(&self) -> MenuGeometry {
        let scale = &self.scale;
        let line = scale.line();
        let rows = self.row_count() * item_height(&self.theme);
        MenuGeometry {
            menu: Rect {
                x: scale.px(self.theme.shadow_left()),
                y: scale.px(self.theme.shadow_top()),
                width: scale.px(self.inner_width) + line * 2,
                height: scale.px(rows) + line * 2,
            },
            line,
        }
    }

    pub fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        let geometry = self.geometry();
        let (x, y) = (self.scale.px(x), self.scale.px(y));
        let menu = geometry.menu;
        let line = geometry.line;
        if x < menu.x + line
            || x >= menu.x + menu.width - line
            || y < menu.y + line
            || y >= menu.y + menu.height - line
        {
            return None;
        }

        let item_h = item_height(&self.theme);
        let header_h = self.header_height();
        (0..self.items.len()).find(|&idx| {
            let (top, bottom) = geometry.row(&self.scale, header_h + idx as i32 * item_h, item_h);
            y >= top && y < bottom
        })
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
        self.select_step(Direction::Forward)
    }

    /// Move the selection one step in `direction`, wrapping around.
    pub fn select_step(&mut self, direction: Direction) -> bool {
        let next = step_index(self.hovered, self.items.len(), direction);
        if next.is_some() && self.hovered != next {
            self.hovered = next;
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

    pub fn ensure_buffer<D>(&mut self, shm: &wl_shm::WlShm, qh: &QueueHandle<D>, scale: Scale)
    where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ()>,
    {
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
            && self.buffer_width == buffer_width
            && self.buffer_height == buffer_height
            && self.scale == scale
        {
            return;
        }
        self.reset_buffer();

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
        if self.buffer.is_none() {
            return;
        }

        if !self.ensure_cache() {
            return;
        }

        let geometry = self.geometry();
        let theme = self.theme.clone();
        let hovered_idx = self.hovered;
        let hovered_item = hovered_idx.and_then(|idx| self.items.get(idx).cloned());
        let item_h = item_height(&theme);
        let header_h = self.header_height();
        let scale = self.scale;
        let row_ctx = MenuRowContext {
            geometry: &geometry,
            item_h,
            scale: &scale,
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
            let row_top = header_h + idx as i32 * item_h;
            draw_menu_row(&mut renderer, item, row_top, true, &row_ctx);
        }
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

        // The region is in logical pixels: the menu box, measured back from
        // where it's drawn.
        let menu = self.geometry().menu;
        let device = self.scale.device();
        let to_logical = |px: i32| (px as f64 / device).round() as i32;
        let x = to_logical(menu.x);
        let y = to_logical(menu.y);
        let region = compositor.create_region(qh, ());
        region.add(
            x,
            y,
            to_logical(menu.x + menu.width) - x,
            to_logical(menu.y + menu.height) - y,
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

    /// Surface-local offset (x, y) of the point that should sit under the
    /// pointer when the menu is opened: horizontally one half-row-height in
    /// from the menu's left edge, vertically at the vertical center of the
    /// title (first) row.
    pub fn pointer_anchor(&self) -> (i32, i32) {
        let half_row = item_height(&self.theme) / 2;
        (
            self.theme.shadow_left() + half_row,
            self.theme.shadow_top() + 1 + half_row,
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
        self.fractional.destroy();
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}

/// Height of a row, in logical pixels.
/// Index one step from `current` in `direction` among `len` items, wrapping
/// around. With nothing selected, forward starts at the first item and reverse
/// at the last.
fn step_index(current: Option<usize>, len: usize, direction: Direction) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (direction, current) {
        (Direction::Forward, Some(idx)) => (idx + 1) % len,
        (Direction::Forward, None) => 0,
        (Direction::Reverse, Some(0) | None) => len - 1,
        (Direction::Reverse, Some(idx)) => idx - 1,
    })
}

fn item_height(theme: &MenuTheme) -> i32 {
    let font_h = theme.font_size.ceil() as i32;
    (font_h + ITEM_PADDING_Y * 2).max(1)
}

/// Width of the rows between the borders, in logical pixels. The text is
/// measured at the size it will be drawn at, then taken back to logical
/// pixels, so hinting at that size can't push it past its slot.
fn measure_inner_width(
    items: &[MenuItem],
    theme: &MenuTheme,
    header_title: Option<&str>,
    scale: &Scale,
) -> i32 {
    let font_px = scale.px_f(theme.font_size);
    let widest = items
        .iter()
        .map(|item| item.title.as_str())
        .chain(header_title)
        .filter_map(|text| font::measure_text(theme.font_name.as_deref(), font_px, text))
        .reduce(f32::max);
    match widest {
        Some(width) => {
            let text_w = (width / scale.factor()).ceil() as i32;
            ITEM_PADDING_X * 2 + ICON_SIZE + ICON_GAP + text_w
        }
        None => FALLBACK_WIDTH,
    }
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
        let mut renderer = match Renderer::new(&mut pixels, self.buffer_width, self.buffer_height) {
            Some(renderer) => renderer,
            None => return false,
        };

        let geometry = self.geometry();
        let menu = geometry.menu;
        let scale = self.scale;

        draw_menu_shadow(&mut renderer, &self.theme, &geometry, &scale);

        renderer.fill_rect(
            menu.x,
            menu.y,
            menu.width,
            menu.height,
            rgba_to_argb(self.theme.bg),
        );

        draw_border_rect(
            &mut renderer,
            menu,
            geometry.line,
            rgba_to_argb(BORDER_COLOR),
        );

        let item_h = item_height(&self.theme);
        let header_h = self.header_height();
        let row_ctx = MenuRowContext {
            geometry: &geometry,
            item_h,
            scale: &scale,
            theme: &self.theme,
        };
        if let Some(title) = self.header_title.as_deref() {
            draw_menu_header(&mut renderer, title, &row_ctx);
        }
        for (idx, item) in self.items.iter().enumerate() {
            let row_top = header_h + idx as i32 * item_h;
            draw_menu_row(&mut renderer, item, row_top, false, &row_ctx);
        }

        self.cache = Some(MenuCache { key, pixels });
        true
    }
}

struct MenuRowContext<'a> {
    geometry: &'a MenuGeometry,
    /// Row height, in logical pixels.
    item_h: i32,
    scale: &'a Scale,
    theme: &'a MenuTheme,
}

impl MenuRowContext<'_> {
    /// Fill the row starting `row_top` logical pixels below the top border,
    /// border to border; returns its device-pixel `(top, height)`.
    fn fill_row(&self, renderer: &mut Renderer, row_top: i32, color_argb: u32) -> (i32, i32) {
        let menu = self.geometry.menu;
        let line = self.geometry.line;
        let (top, bottom) = self.geometry.row(self.scale, row_top, self.item_h);
        renderer.fill_rect(
            menu.x + line,
            top,
            menu.width - line * 2,
            bottom - top,
            color_argb,
        );
        (top, bottom - top)
    }

    /// Device x of a point `x` logical pixels in from the left border.
    fn x(&self, x: i32) -> i32 {
        self.geometry.menu.x + self.geometry.line + self.scale.px(x)
    }

    /// Draw `text` in the row, from `x` to the right border.
    fn text(
        &self,
        renderer: &mut Renderer,
        text: &str,
        x: i32,
        (top, height): (i32, i32),
        color_argb: u32,
    ) {
        let menu = self.geometry.menu;
        let x = self.x(x);
        renderer.render_text(
            text,
            x,
            top,
            menu.x + menu.width - self.geometry.line - x,
            height,
            self.scale.factor(),
            color_argb,
            self.theme.font_size,
            self.theme.font_name.as_deref(),
            0,
        );
    }
}

fn draw_menu_header(renderer: &mut Renderer, title: &str, ctx: &MenuRowContext<'_>) {
    let bg = rgba_to_argb(ctx.theme.titlebar_bg);
    let text_color = rgba_to_argb(ctx.theme.titlebar_text);
    let row = ctx.fill_row(renderer, 0, bg);
    ctx.text(renderer, title, ITEM_PADDING_X, row, text_color);
}

fn draw_menu_row(
    renderer: &mut Renderer,
    item: &MenuItem,
    row_top: i32,
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

    let row = ctx.fill_row(renderer, row_top, bg);
    let (top, height) = row;
    let scale = ctx.scale;

    let icon_x = ctx.x(ITEM_PADDING_X);
    let icon_size = scale.px(ICON_SIZE);
    if item.hidden {
        draw_dashed_rect(
            renderer,
            Rect {
                x: icon_x,
                y: top + (height - icon_size) / 2,
                width: icon_size,
                height: icon_size,
            },
            ctx.geometry.line,
            icon_color,
        );
    }
    if item.active {
        let diamond = scale.px(ACTIVE_DIAMOND_SIZE);
        draw_diamond(
            renderer,
            icon_x + (icon_size - diamond) / 2,
            top + (height - diamond) / 2,
            diamond,
            text_color,
        );
    }

    let text_x = ITEM_PADDING_X + ICON_SIZE + ICON_GAP;
    ctx.text(renderer, &item.title, text_x, row, text_color);
}

fn rgba_to_argb(rgba: u32) -> u32 {
    let r = (rgba >> 24) & 0xff;
    let g = (rgba >> 16) & 0xff;
    let b = (rgba >> 8) & 0xff;
    let a = rgba & 0xff;
    (a << 24) | (r << 16) | (g << 8) | b
}

fn draw_border_rect(renderer: &mut Renderer, rect: Rect, line: i32, color_argb: u32) {
    if rect.width <= 0 || rect.height <= 0 {
        return;
    }

    let Rect {
        x,
        y,
        width,
        height,
    } = rect;
    renderer.fill_rect(x, y, width, line, color_argb);
    renderer.fill_rect(x, y + height - line, width, line, color_argb);
    renderer.fill_rect(x, y, line, height, color_argb);
    renderer.fill_rect(x + width - line, y, line, height, color_argb);
}

fn draw_menu_shadow(
    renderer: &mut Renderer,
    theme: &MenuTheme,
    geometry: &MenuGeometry,
    scale: &Scale,
) {
    let menu = geometry.menu;
    if menu.width <= 0 || menu.height <= 0 {
        return;
    }

    if theme.shadows_enabled {
        if theme.shadow_size <= 0 {
            return;
        }
        // Push the shape's top edge down by size/2 so the falloff sits lower
        // in the buffer; combined with the asymmetric padding from
        // MenuTheme::shadow_top/bottom this produces the same "light from
        // above" offset as the window drop shadow.
        let drop = scale.px(theme.shadow_size / 2);
        draw_shadow_soft(
            renderer,
            SoftShadowRect {
                x: menu.x,
                y: menu.y + drop,
                width: menu.width,
                height: (menu.height - drop).max(1),
            },
            scale.px(theme.shadow_size),
            0,
            theme.shadow_color,
        );
    } else {
        let l_size = scale.px(L_SHADOW_SIZE);
        let color = rgba_to_argb(L_SHADOW_COLOR);
        renderer.fill_rect(
            menu.x + l_size,
            menu.y + menu.height,
            menu.width,
            l_size,
            color,
        );
        renderer.fill_rect(
            menu.x + menu.width,
            menu.y + l_size,
            l_size,
            menu.height,
            color,
        );
    }
}

/// A dashed outline `line` device pixels thick, dashes and gaps two lines long.
fn draw_dashed_rect(renderer: &mut Renderer, rect: Rect, line: i32, color_argb: u32) {
    let Rect {
        x,
        y,
        width,
        height,
    } = rect;
    let dash = line * 2;
    let gap = line * 2;
    let mut px = x;
    while px < x + width {
        let segment = (x + width - px).min(dash);
        renderer.fill_rect(px, y, segment, line, color_argb);
        renderer.fill_rect(px, y + height - line, segment, line, color_argb);
        px += dash + gap;
    }

    let mut py = y;
    while py < y + height {
        let segment = (y + height - py).min(dash);
        renderer.fill_rect(x, py, line, segment, color_argb);
        renderer.fill_rect(x + width - line, py, line, segment, color_argb);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_forward_advances_and_wraps() {
        assert_eq!(step_index(Some(0), 3, Direction::Forward), Some(1));
        assert_eq!(step_index(Some(2), 3, Direction::Forward), Some(0));
    }

    #[test]
    fn step_reverse_retreats_and_wraps() {
        assert_eq!(step_index(Some(2), 3, Direction::Reverse), Some(1));
        assert_eq!(step_index(Some(0), 3, Direction::Reverse), Some(2));
    }

    #[test]
    fn step_without_selection_starts_at_the_matching_end() {
        assert_eq!(step_index(None, 3, Direction::Forward), Some(0));
        assert_eq!(step_index(None, 3, Direction::Reverse), Some(2));
    }

    #[test]
    fn step_with_single_item_stays_put() {
        assert_eq!(step_index(Some(0), 1, Direction::Forward), Some(0));
        assert_eq!(step_index(Some(0), 1, Direction::Reverse), Some(0));
    }

    #[test]
    fn step_without_items_selects_nothing() {
        assert_eq!(step_index(None, 0, Direction::Forward), None);
        assert_eq!(step_index(None, 0, Direction::Reverse), None);
    }
}
