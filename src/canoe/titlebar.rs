//! Titlebar rendering for windows

use super::render::Renderer;
use super::shmfile::ShmPool;
use crate::config::UiConfig;
use crate::protocol::RiverDecorationV1;
use resvg::{tiny_skia, usvg};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::QueueHandle;

/// Titlebar height in pixels
pub fn titlebar_height(ui: &UiConfig) -> i32 {
    let base = (0.75 * ui.font_size).round() as i32;
    (base * 2 + 1).max(1)
}

/// Button background color (pressed left edge)
const BUTTON_BG_PRESSED_LEFT: u32 = 0xA0A0A0FF;

const BORDER_OUTER: i32 = 1;
const BORDER_INNER: i32 = 1;

const BUTTON_PADDING_X: i32 = 0;
const BUTTON_GAP: i32 = 1;

const ICON_CLOSE_SVG: &str = include_str!("../../assets/icons/close.svg");
const ICON_MINIMIZE_SVG: &str = include_str!("../../assets/icons/minimize.svg");
const ICON_MAXIMIZE_SVG: &str = include_str!("../../assets/icons/maximize.svg");
const ICON_UNMAXIMIZE_SVG: &str = include_str!("../../assets/icons/unmaximize.svg");

struct BaseFrameParams<'a> {
    ui: &'a UiConfig,
    is_active: bool,
    content_width: i32,
    titlebar_height: i32,
    buffer_width: i32,
    buffer_height: i32,
    height: i32,
    scale: i32,
    show_minimize: bool,
    show_maximize: bool,
    frame_style: FrameStyle,
}

#[derive(Clone, Copy, Debug)]
struct ButtonCacheKey {
    button_bg: u32,
    button_highlight: u32,
    button_shadow: u32,
}

#[derive(Clone, Debug)]
struct ButtonCache {
    size_px: i32,
    titlebar_height: i32,
    key: ButtonCacheKey,
    close_normal: Vec<u8>,
    close_pressed: Vec<u8>,
    hide_normal: Vec<u8>,
    hide_pressed: Vec<u8>,
    maximize_normal: Vec<u8>,
    maximize_pressed: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TitlebarButtons {
    pub close: Rect,
    pub hide: Option<Rect>,
    pub maximize: Option<Rect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitlebarButton {
    Close,
    Hide,
    Maximize,
}

/// How the SSD frame around a window should be painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStyle {
    /// Standard 3-layer border using the configured palette.
    Normal,
    /// Parented dialog: mid layer takes the titlebar background colour.
    Dialog,
    /// Non-resizable toplevel: a single 1px outline in the outer border
    /// colour, hugging the content/titlebar with no chunky multi-layer band.
    FixedSize,
}

/// The SSD border thickness to reserve and draw for a given frame style.
///
/// Non-resizable toplevels ([`FrameStyle::FixedSize`]) collapse the border to
/// a single 1px outline that sits directly against the content/titlebar;
/// every other style uses the configured border width.
pub fn border_width(ui: &UiConfig, frame_style: FrameStyle) -> i32 {
    match frame_style {
        FrameStyle::FixedSize => BORDER_OUTER,
        _ => ui.border_width,
    }
}

struct IconCache {
    size_px: i32,
    close: tiny_skia::Pixmap,
    minimize: tiny_skia::Pixmap,
    maximize: tiny_skia::Pixmap,
    unmaximize: tiny_skia::Pixmap,
}

impl IconCache {
    fn build(size_px: i32) -> Option<Self> {
        let close = rasterize_icon(ICON_CLOSE_SVG, size_px)?;
        let minimize = rasterize_icon(ICON_MINIMIZE_SVG, size_px)?;
        let maximize = rasterize_icon(ICON_MAXIMIZE_SVG, size_px)?;
        let unmaximize = rasterize_icon(ICON_UNMAXIMIZE_SVG, size_px)?;
        Some(Self {
            size_px,
            close,
            minimize,
            maximize,
            unmaximize,
        })
    }
}

pub fn button_rects(
    content_width: i32,
    titlebar_height: i32,
    show_minimize: bool,
    show_maximize: bool,
) -> TitlebarButtons {
    let size = titlebar_height;
    let y = 0;
    let close = Rect {
        x: BUTTON_PADDING_X,
        y,
        width: size,
        height: size,
    };

    // Right-to-left: maximize is the rightmost (if present), then hide; if
    // maximize is hidden, the hide button slides into the rightmost slot.
    let mut right_x = content_width - BUTTON_PADDING_X - size;
    let maximize = if show_maximize {
        let r = Rect {
            x: right_x.max(0),
            y,
            width: size,
            height: size,
        };
        right_x -= size + BUTTON_GAP;
        Some(r)
    } else {
        None
    };
    let hide = if show_minimize {
        Some(Rect {
            x: right_x.max(0),
            y,
            width: size,
            height: size,
        })
    } else {
        None
    };

    TitlebarButtons {
        close,
        hide,
        maximize,
    }
}

pub fn button_at(
    content_width: i32,
    border_width: i32,
    local_x: i32,
    local_y: i32,
    titlebar_height: i32,
    show_minimize: bool,
    show_maximize: bool,
) -> Option<TitlebarButton> {
    if content_width <= 0 {
        return None;
    }

    let rel_x = local_x - border_width;
    let rel_y = local_y - border_width;
    if rel_x < 0 || rel_y < 0 || rel_x >= content_width || rel_y >= titlebar_height {
        return None;
    }

    let buttons = button_rects(content_width, titlebar_height, show_minimize, show_maximize);
    if buttons.close.contains(rel_x, rel_y) {
        return Some(TitlebarButton::Close);
    }
    if let Some(hide) = buttons.hide {
        if hide.contains(rel_x, rel_y) {
            return Some(TitlebarButton::Hide);
        }
    }
    if let Some(maximize) = buttons.maximize {
        if maximize.contains(rel_x, rel_y) {
            return Some(TitlebarButton::Maximize);
        }
    }

    None
}

/// Titlebar state for a window
pub struct Titlebar {
    /// The wl_surface for the titlebar
    pub surface: wl_surface::WlSurface,
    /// The river decoration object
    pub decoration: RiverDecorationV1,
    /// Double-buffered shm pool backing the wl_buffer.
    shm_pool: ShmPool,
    /// Current buffer width
    pub width: i32,
    /// Current buffer height
    pub height: i32,
    /// Current buffer width in pixels
    pub buffer_width: i32,
    /// Current buffer height in pixels
    pub buffer_height: i32,
    /// Current content width
    pub content_width: i32,
    /// Current content height
    pub content_height: i32,
    /// Border width (logical px) of the current frame
    pub border_width: i32,
    /// Titlebar height (logical px) of the current frame
    pub titlebar_height: i32,
    /// Output scale factor
    pub scale: i32,
    /// Cached icon bitmaps (rasterized from embedded SVGs)
    icon_cache: Option<IconCache>,
    button_cache: Option<ButtonCache>,
    /// wl_output names the titlebar surface is currently on
    pub output_names: Vec<u32>,
    /// Whether titlebar needs redraw
    pub dirty: bool,
    /// Whether the surface currently has a buffer attached (i.e. is visible)
    pub mapped: bool,
    /// Force a full-surface damage on the next commit (set after a (re)alloc
    /// so the compositor picks up the whole new buffer).
    needs_full_damage: bool,
    last_title: Option<String>,
    last_is_active: bool,
    last_is_maximized: bool,
    last_show_minimize: bool,
    last_show_maximize: bool,
    last_frame_style: FrameStyle,
    last_hovered: Option<TitlebarButton>,
    last_left_down: bool,
}

impl Titlebar {
    /// Create a new titlebar
    pub fn new(surface: wl_surface::WlSurface, decoration: RiverDecorationV1) -> Self {
        Self {
            surface,
            decoration,
            shm_pool: ShmPool::new("canoe-titlebar"),
            width: 0,
            height: 0,
            buffer_width: 0,
            buffer_height: 0,
            content_width: 0,
            content_height: 0,
            border_width: 0,
            titlebar_height: 0,
            scale: 1,
            icon_cache: None,
            button_cache: None,
            output_names: Vec::new(),
            dirty: true,
            mapped: false,
            needs_full_damage: true,
            last_title: None,
            last_is_active: false,
            last_is_maximized: false,
            last_show_minimize: true,
            last_show_maximize: true,
            last_frame_style: FrameStyle::Normal,
            last_hovered: None,
            last_left_down: false,
        }
    }

    /// True once the next commit() will find an attached wl_buffer.
    pub fn is_ready(&self) -> bool {
        self.shm_pool.is_ready()
    }

    /// Record the geometry the next `render()` will draw into. The actual
    /// shm slot is rotated/allocated lazily inside `render()` so a frame that
    /// turns out to be a no-op doesn't burn a new wl_buffer.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_buffer<D>(
        &mut self,
        content_width: i32,
        content_height: i32,
        _shm: &wl_shm::WlShm,
        _qh: &QueueHandle<D>,
        scale: i32,
        ui: &UiConfig,
        frame_style: FrameStyle,
    ) where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, super::shmfile::ReleaseFlag>,
    {
        if content_width <= 0 || content_height <= 0 {
            return;
        }

        let scale = scale.max(1);
        let titlebar_height = titlebar_height(ui);
        let border_width = border_width(ui, frame_style);
        // Remember the frame geometry so commit() can damage just the painted
        // border ring + titlebar instead of the whole (mostly transparent) buffer.
        self.border_width = border_width;
        self.titlebar_height = titlebar_height;
        let width = content_width + border_width * 2;
        let height = content_height + titlebar_height + border_width * 2;
        let buffer_width = width * scale;
        let buffer_height = height * scale;
        if buffer_width <= 0 || buffer_height <= 0 {
            return;
        }

        if self.width != width
            || self.height != height
            || self.buffer_width != buffer_width
            || self.buffer_height != buffer_height
            || self.scale != scale
        {
            self.width = width;
            self.height = height;
            self.buffer_width = buffer_width;
            self.buffer_height = buffer_height;
            self.content_width = content_width;
            self.content_height = content_height;
            self.scale = scale;
            self.icon_cache = None;
            self.button_cache = None;
            self.dirty = true;
            self.needs_full_damage = true;
        } else {
            self.content_width = content_width;
            self.content_height = content_height;
        }

        self.surface.set_buffer_scale(scale);
    }

    fn ensure_icon_cache(&mut self, size_px: i32) -> bool {
        if size_px <= 0 {
            return false;
        }

        let rebuild = match self.icon_cache {
            Some(ref cache) => cache.size_px != size_px,
            None => true,
        };
        if rebuild {
            self.icon_cache = IconCache::build(size_px);
        }

        self.icon_cache.is_some()
    }

    /// Paint the static base frame (borders + titlebar background) directly
    /// into `pixels`. Caller must have cleared `pixels` to fully transparent
    /// (the base frame leaves the content cut-out untouched).
    fn draw_base_frame(pixels: &mut [u8], params: &BaseFrameParams<'_>) -> bool {
        if params.buffer_width <= 0 || params.buffer_height <= 0 {
            return false;
        }
        let mut renderer =
            match Renderer::new(pixels, params.buffer_width, params.buffer_height) {
                Some(renderer) => renderer,
                None => return false,
            };

        let border_offset = 0;
        let mut border_colors = if params.is_active {
            params.ui.border_active
        } else {
            params.ui.border_inactive
        };
        let titlebar_bg = if params.is_active {
            params.ui.titlebar_bg_active
        } else {
            params.ui.titlebar_bg_inactive
        };
        match params.frame_style {
            FrameStyle::Normal | FrameStyle::FixedSize => {}
            FrameStyle::Dialog => {
                // The bulk border picks up the titlebar background so the
                // dialog reads as a continuation of the title; outer/inner
                // thin frames stay on the normal palette.
                border_colors.mid = titlebar_bg;
            }
        }
        let border_width = border_width(params.ui, params.frame_style);
        // Always paint the 1px outer outline. Non-resizable toplevels stop
        // here: the border is exactly that outline, hugging the content.
        draw_border_layer(
            &mut renderer,
            border_offset,
            BORDER_OUTER * params.scale,
            border_colors.outer,
        );
        if !matches!(params.frame_style, FrameStyle::FixedSize) {
            let mid_width = (border_width - BORDER_INNER - BORDER_OUTER).max(0);
            draw_border_layer(
                &mut renderer,
                border_offset + BORDER_OUTER * params.scale,
                mid_width * params.scale,
                border_colors.mid,
            );
            draw_border_layer(
                &mut renderer,
                border_offset + (BORDER_OUTER + mid_width) * params.scale,
                BORDER_INNER * params.scale,
                border_colors.inner,
            );
        }

        let bg_color = if params.is_active {
            params.ui.titlebar_bg_active
        } else {
            params.ui.titlebar_bg_inactive
        };
        let bg_argb = rgba_to_argb(bg_color);
        let title_height = params
            .titlebar_height
            .min(params.height - border_width * 2);
        if title_height > 0 {
            let title_x = border_width;
            let title_y = border_width;
            renderer.fill_rect(
                title_x * params.scale,
                title_y * params.scale,
                params.content_width * params.scale,
                title_height * params.scale,
                bg_argb,
            );

            let buttons = button_rects(
                params.content_width,
                params.titlebar_height,
                params.show_minimize,
                params.show_maximize,
            );
            let button_border = rgba_to_argb(border_colors.outer);
            draw_left_border(
                &mut renderer,
                (title_x + buttons.close.x + buttons.close.width) * params.scale,
                title_y * params.scale,
                title_height * params.scale,
                button_border,
                params.titlebar_height,
            );
            if let Some(hide) = buttons.hide {
                draw_left_border(
                    &mut renderer,
                    (title_x + hide.x - 1) * params.scale,
                    title_y * params.scale,
                    title_height * params.scale,
                    button_border,
                    params.titlebar_height,
                );
            }
            if let Some(maximize) = buttons.maximize {
                draw_left_border(
                    &mut renderer,
                    (title_x + maximize.x - 1) * params.scale,
                    title_y * params.scale,
                    title_height * params.scale,
                    button_border,
                    params.titlebar_height,
                );
            }

            let separator_y = title_y + title_height;
            if separator_y >= 0 && separator_y < params.height - border_width {
                renderer.fill_rect(
                    title_x * params.scale,
                    separator_y * params.scale,
                    params.content_width * params.scale,
                    params.scale,
                    rgba_to_argb(border_colors.outer),
                );
            }
        }

        true
    }

    fn button_cache_key(ui: &UiConfig) -> ButtonCacheKey {
        ButtonCacheKey {
            button_bg: ui.button_bg,
            button_highlight: ui.button_highlight,
            button_shadow: ui.button_shadow,
        }
    }

    fn ensure_button_cache(
        &mut self,
        ui: &UiConfig,
        titlebar_height: i32,
        scale: i32,
    ) -> Option<&ButtonCache> {
        let size_px = titlebar_height * scale.max(1);
        if size_px <= 0 {
            return None;
        }

        let key = Self::button_cache_key(ui);
        let needs_rebuild = match self.button_cache {
            Some(ref cache) => {
                cache.size_px != size_px
                    || cache.titlebar_height != titlebar_height
                    || cache.key.button_bg != key.button_bg
                    || cache.key.button_highlight != key.button_highlight
                    || cache.key.button_shadow != key.button_shadow
            }
            None => true,
        };

        if needs_rebuild {
            let button_bg = rgba_to_argb(ui.button_bg);
            let highlight = rgba_to_argb(ui.button_highlight);
            let shadow = rgba_to_argb(ui.button_shadow);
            let close_pressed_bg = rgba_to_argb(BUTTON_BG_PRESSED_LEFT);

            let close_normal = build_button_fill(size_px, button_bg);
            let close_pressed = build_button_fill(size_px, close_pressed_bg);
            let hide_normal = build_button_bevel_cache(
                size_px,
                titlebar_height,
                button_bg,
                highlight,
                shadow,
                false,
            );
            let hide_pressed = build_button_bevel_cache(
                size_px,
                titlebar_height,
                button_bg,
                highlight,
                shadow,
                true,
            );
            let maximize_normal = build_button_bevel_cache(
                size_px,
                titlebar_height,
                button_bg,
                highlight,
                shadow,
                false,
            );
            let maximize_pressed = build_button_bevel_cache(
                size_px,
                titlebar_height,
                button_bg,
                highlight,
                shadow,
                true,
            );

            self.button_cache = Some(ButtonCache {
                size_px,
                titlebar_height,
                key,
                close_normal,
                close_pressed,
                hide_normal,
                hide_pressed,
                maximize_normal,
                maximize_pressed,
            });
        }

        self.button_cache.as_ref()
    }

    /// Render the titlebar with the given title and state
    #[allow(clippy::too_many_arguments)]
    pub fn render<D>(
        &mut self,
        title: Option<&str>,
        is_active: bool,
        is_maximized: bool,
        show_minimize: bool,
        show_maximize: bool,
        frame_style: FrameStyle,
        hovered_button: Option<TitlebarButton>,
        left_down: bool,
        ui: &UiConfig,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
    ) -> bool
    where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, super::shmfile::ReleaseFlag>,
    {
        let title_changed = self.last_title.as_deref() != title;
        let state_changed = title_changed
            || self.last_is_active != is_active
            || self.last_is_maximized != is_maximized
            || self.last_show_minimize != show_minimize
            || self.last_show_maximize != show_maximize
            || self.last_frame_style != frame_style
            || self.last_hovered != hovered_button
            || self.last_left_down != left_down;
        if state_changed {
            if title_changed {
                self.last_title = title.map(str::to_owned);
            }
            self.last_is_active = is_active;
            self.last_is_maximized = is_maximized;
            self.last_show_minimize = show_minimize;
            self.last_show_maximize = show_maximize;
            self.last_frame_style = frame_style;
            self.last_hovered = hovered_button;
            self.last_left_down = left_down;
            self.dirty = true;
        }

        if !self.dirty {
            return false;
        }

        let scale = self.scale.max(1);
        let titlebar_height = titlebar_height(ui);
        let icon_size = icon_size_for_titlebar(titlebar_height);
        let icon_size_px = icon_size * scale;
        let icons_ready = self.ensure_icon_cache(icon_size_px);
        self.ensure_button_cache(ui, titlebar_height, scale);

        let width = self.width;
        let height = self.height;
        let buffer_width = self.buffer_width;
        let buffer_height = self.buffer_height;
        let content_width = self.content_width;
        let content_height = self.content_height;
        if width <= 0
            || height <= 0
            || buffer_width <= 0
            || buffer_height <= 0
            || content_width <= 0
            || content_height <= 0
        {
            return false;
        }

        let stride = match buffer_width.checked_mul(4) {
            Some(s) => s,
            None => return false,
        };

        #[cfg(feature = "debug-logging")]
        let prep_t0 = std::time::Instant::now();
        let allocated_before = self.shm_pool.allocate_fresh_count();
        let slot_idx = match self.shm_pool.prepare(buffer_width, buffer_height, stride, shm, qh) {
            Some(s) => s,
            None => return false,
        };
        let allocated_fresh = self.shm_pool.allocate_fresh_count() != allocated_before;
        #[cfg(feature = "debug-logging")]
        let prep_us = prep_t0.elapsed().as_micros();

        let base_params = BaseFrameParams {
            ui,
            is_active,
            content_width,
            titlebar_height,
            buffer_width,
            buffer_height,
            height,
            scale,
            show_minimize,
            show_maximize,
            frame_style,
        };

        let pixels = match self.shm_pool.slot_bytes_mut(slot_idx) {
            Some(p) => p,
            None => return false,
        };

        #[cfg(feature = "debug-logging")]
        let fill_t0 = std::time::Instant::now();
        // The titlebar surface spans the whole window but only the border ring
        // and titlebar strip carry pixels; the rest must stay fully transparent.
        // The pool rotates slots each prepare, so even when geometry is steady
        // the slot we just took may still hold the *other* focus state's
        // pixels -- start from zero.
        pixels.fill(0);
        #[cfg(feature = "debug-logging")]
        let fill_us = fill_t0.elapsed().as_micros();

        #[cfg(feature = "debug-logging")]
        let base_t0 = std::time::Instant::now();
        if !Self::draw_base_frame(pixels, &base_params) {
            return false;
        }
        #[cfg(feature = "debug-logging")]
        let base_us = base_t0.elapsed().as_micros();

        #[cfg(feature = "debug-logging")]
        let dyn_t0 = std::time::Instant::now();
        #[cfg(not(feature = "debug-logging"))]
        let _ = allocated_fresh;
        {
            let mut renderer = match Renderer::new(pixels, buffer_width, buffer_height) {
                Some(renderer) => renderer,
                None => return false,
            };
            let button_cache = self.button_cache.as_ref();

            let border_colors = if is_active {
                ui.border_active
            } else {
                ui.border_inactive
            };
            let border_width = border_width(ui, frame_style);
            let title_height = titlebar_height.min(height - border_width * 2);
            if title_height > 0 {
                let title_x = border_width;
                let title_y = border_width;

                let buttons =
                    button_rects(content_width, titlebar_height, show_minimize, show_maximize);
                let pressed_hover = if left_down { hovered_button } else { None };
                let close_pressed = pressed_hover == Some(TitlebarButton::Close);
                if let Some(cache) = button_cache {
                    let close_bg = if close_pressed {
                        &cache.close_pressed
                    } else {
                        &cache.close_normal
                    };
                    renderer.blit_argb(
                        close_bg,
                        cache.size_px,
                        cache.size_px,
                        (title_x + buttons.close.x) * scale,
                        (title_y + buttons.close.y) * scale,
                    );
                } else {
                    let close_bg = if close_pressed {
                        rgba_to_argb(BUTTON_BG_PRESSED_LEFT)
                    } else {
                        rgba_to_argb(ui.button_bg)
                    };
                    renderer.fill_rect(
                        (title_x + buttons.close.x) * scale,
                        (title_y + buttons.close.y) * scale,
                        buttons.close.width * scale,
                        title_height * scale,
                        close_bg,
                    );
                }
                if icons_ready {
                    let icon_x =
                        (title_x + buttons.close.x + (buttons.close.width - icon_size) / 2) * scale;
                    let icon_y =
                        (title_y + buttons.close.y + (buttons.close.height - icon_size) / 2)
                            * scale;
                    if let Some(ref icons) = self.icon_cache {
                        renderer.blit_pixmap(&icons.close, icon_x, icon_y);
                    }
                } else {
                    draw_glyph_close(
                        &mut renderer,
                        (title_x + buttons.close.x) * scale,
                        (title_y + buttons.close.y) * scale,
                        buttons.close.width * scale,
                        rgba_to_argb(border_colors.outer),
                        titlebar_height,
                    );
                }

                if let Some(hide) = buttons.hide {
                    let hide_pressed = pressed_hover == Some(TitlebarButton::Hide);
                    let hide_offset = if hide_pressed { scale } else { 0 };
                    if let Some(cache) = button_cache {
                        let hide_bg = if hide_pressed {
                            &cache.hide_pressed
                        } else {
                            &cache.hide_normal
                        };
                        renderer.blit_argb(
                            hide_bg,
                            cache.size_px,
                            cache.size_px,
                            (title_x + hide.x) * scale,
                            (title_y + hide.y) * scale,
                        );
                    } else {
                        draw_button_bevel(
                            &mut renderer,
                            (title_x + hide.x) * scale,
                            (title_y + hide.y) * scale,
                            hide.width * scale,
                            rgba_to_argb(ui.button_bg),
                            rgba_to_argb(ui.button_highlight),
                            rgba_to_argb(ui.button_shadow),
                            hide_pressed,
                            titlebar_height,
                        );
                    }
                    if icons_ready {
                        let icon_x = (title_x + hide.x + (hide.width - icon_size) / 2) * scale
                            + hide_offset;
                        let icon_y = (title_y + hide.y + (hide.height - icon_size) / 2) * scale
                            + hide_offset;
                        if let Some(ref icons) = self.icon_cache {
                            renderer.blit_pixmap(&icons.minimize, icon_x, icon_y);
                        }
                    } else {
                        draw_glyph_caret(
                            &mut renderer,
                            (title_x + hide.x) * scale + hide_offset,
                            (title_y + hide.y) * scale + hide_offset,
                            hide.width * scale,
                            rgba_to_argb(border_colors.outer),
                            true,
                            titlebar_height,
                        );
                    }
                }

                if let Some(maximize) = buttons.maximize {
                    let maximize_pressed = pressed_hover == Some(TitlebarButton::Maximize);
                    let maximize_offset = if maximize_pressed { scale } else { 0 };
                    if let Some(cache) = button_cache {
                        let maximize_bg = if maximize_pressed {
                            &cache.maximize_pressed
                        } else {
                            &cache.maximize_normal
                        };
                        renderer.blit_argb(
                            maximize_bg,
                            cache.size_px,
                            cache.size_px,
                            (title_x + maximize.x) * scale,
                            (title_y + maximize.y) * scale,
                        );
                    } else {
                        draw_button_bevel(
                            &mut renderer,
                            (title_x + maximize.x) * scale,
                            (title_y + maximize.y) * scale,
                            maximize.width * scale,
                            rgba_to_argb(ui.button_bg),
                            rgba_to_argb(ui.button_highlight),
                            rgba_to_argb(ui.button_shadow),
                            maximize_pressed,
                            titlebar_height,
                        );
                    }
                    if icons_ready {
                        let icon_x = (title_x + maximize.x + (maximize.width - icon_size) / 2)
                            * scale
                            + maximize_offset;
                        let icon_y = (title_y + maximize.y + (maximize.height - icon_size) / 2)
                            * scale
                            + maximize_offset;
                        if let Some(ref icons) = self.icon_cache {
                            let icon = if is_maximized {
                                &icons.unmaximize
                            } else {
                                &icons.maximize
                            };
                            renderer.blit_pixmap(icon, icon_x, icon_y);
                        }
                    } else if is_maximized {
                        draw_glyph_caret_pair(
                            &mut renderer,
                            (title_x + maximize.x) * scale + maximize_offset,
                            (title_y + maximize.y) * scale + maximize_offset,
                            maximize.width * scale,
                            rgba_to_argb(border_colors.outer),
                            titlebar_height,
                        );
                    } else {
                        draw_glyph_caret(
                            &mut renderer,
                            (title_x + maximize.x) * scale + maximize_offset,
                            (title_y + maximize.y) * scale + maximize_offset,
                            maximize.width * scale,
                            rgba_to_argb(border_colors.outer),
                            false,
                            titlebar_height,
                        );
                    }
                }

                // Render title text if we have a title and font
                if let Some(title_str) = title {
                    if !title_str.is_empty() {
                        let text_start =
                            (buttons.close.x + buttons.close.width + BUTTON_GAP).max(0);
                        let text_padding = (ui.font_size * 0.5).round().max(0.0) as i32;
                        let right_x = buttons
                            .hide
                            .map(|r| r.x)
                            .or_else(|| buttons.maximize.map(|r| r.x))
                            .unwrap_or(content_width - BUTTON_PADDING_X);
                        let text_end =
                            (right_x - BUTTON_GAP - text_padding).min(content_width);
                        let text_width = (text_end - text_start).max(0);
                        if text_width > 0 {
                            let text_color = if is_active {
                                ui.titlebar_text_active
                            } else {
                                ui.titlebar_text_inactive
                            };
                            let text_argb = rgba_to_argb(text_color);
                            renderer.render_text(
                                title_str,
                                (title_x + text_start) * scale,
                                title_y * scale,
                                text_width * scale,
                                title_height * scale,
                                scale,
                                text_argb,
                                ui.font_size,
                                ui.font_name.as_deref(),
                                text_padding * scale,
                            );
                        }
                    }
                }
            }

            self.dirty = false;
        }
        #[cfg(feature = "debug-logging")]
        let dyn_us = dyn_t0.elapsed().as_micros();
        #[cfg(feature = "debug-logging")]
        eprintln!(
            "[canoe titlebar] {}x{} bytes={} alloc_fresh={} prepare={:.2}ms fill={:.2}ms base={:.2}ms dynamic={:.2}ms",
            buffer_width,
            buffer_height,
            buffer_width as i64 * buffer_height as i64 * 4,
            allocated_fresh,
            prep_us as f64 / 1000.0,
            fill_us as f64 / 1000.0,
            base_us as f64 / 1000.0,
            dyn_us as f64 / 1000.0,
        );
        true
    }

    /// Commit the titlebar surface
    pub fn commit(&mut self) {
        if let Some(buffer) = self.shm_pool.current_buffer() {
            self.surface.attach(Some(buffer), 0, 0);
            // The decoration surface spans the whole window, but only the
            // border ring and titlebar strip are ever painted -- the content
            // area in the middle stays transparent and never changes. After a
            // (re)alloc we damage everything so the compositor picks up the
            // whole new buffer; afterwards we damage just the frame, which keeps
            // focus changes cheap under fractional scaling where the compositor
            // has to resample every damaged pixel.
            if self.needs_full_damage {
                self.surface
                    .damage_buffer(0, 0, self.buffer_width, self.buffer_height);
                self.needs_full_damage = false;
                #[cfg(feature = "debug-logging")]
                eprintln!(
                    "[canoe damage] FULL  buf={}x{} scale={} damaged={}px (100.0%)",
                    self.buffer_width,
                    self.buffer_height,
                    self.scale,
                    self.buffer_width as i64 * self.buffer_height as i64,
                );
            } else {
                self.damage_frame();
            }
            self.surface.commit();
            // The compositor now owns this buffer until it sends a release
            // event; mark the slot off-limits for the next render.
            self.shm_pool.mark_attached();
            self.mapped = true;
        }
    }

    /// Damage only the painted frame (border ring + titlebar), leaving the
    /// transparent content cut-out in the middle untouched.
    fn damage_frame(&self) {
        let bw = self.border_width.max(0);
        let tbh = self.titlebar_height.max(0);
        let scale = self.scale.max(1);

        // Transparent content cut-out in buffer pixels. The +1 keeps the row
        // separating the titlebar from the content inside the damaged top strip.
        let cut_x0 = bw * scale;
        let cut_y0 = (bw + tbh + 1) * scale;
        let cut_x1 = self.buffer_width - bw * scale;
        let cut_y1 = self.buffer_height - bw * scale;

        // Degenerate geometry (tiny window, missing borders): just damage it all.
        if cut_x1 <= cut_x0 || cut_y1 <= cut_y0 {
            self.surface
                .damage_buffer(0, 0, self.buffer_width, self.buffer_height);
            #[cfg(feature = "debug-logging")]
            eprintln!(
                "[canoe damage] FRAME->FULL (degenerate) buf={}x{} scale={}",
                self.buffer_width, self.buffer_height, self.scale,
            );
            return;
        }

        let damage = |x: i32, y: i32, w: i32, h: i32| {
            if w > 0 && h > 0 {
                self.surface.damage_buffer(x, y, w, h);
            }
        };
        damage(0, 0, self.buffer_width, cut_y0); // top border + titlebar
        damage(0, cut_y1, self.buffer_width, self.buffer_height - cut_y1); // bottom border
        damage(0, cut_y0, cut_x0, cut_y1 - cut_y0); // left border
        damage(cut_x1, cut_y0, self.buffer_width - cut_x1, cut_y1 - cut_y0); // right border

        #[cfg(feature = "debug-logging")]
        {
            let full_px = self.buffer_width as i64 * self.buffer_height as i64;
            let damaged_px = self.buffer_width as i64 * cut_y0 as i64
                + self.buffer_width as i64 * (self.buffer_height - cut_y1) as i64
                + (cut_x0 as i64 + (self.buffer_width - cut_x1) as i64)
                    * (cut_y1 - cut_y0) as i64;
            eprintln!(
                "[canoe damage] FRAME buf={}x{} scale={} damaged={}px / full={}px ({:.1}%)",
                self.buffer_width,
                self.buffer_height,
                self.scale,
                damaged_px,
                full_px,
                100.0 * damaged_px as f64 / full_px.max(1) as f64,
            );
        }
    }

    /// Detach the buffer so the surface becomes invisible. Used when a window
    /// switches to client-side decoration at runtime.
    pub fn unmap(&mut self) {
        self.surface.attach(None, 0, 0);
        self.surface.commit();
        self.mapped = false;
        self.dirty = true;
        self.needs_full_damage = true;
        self.last_title = None;
        self.last_is_active = false;
        self.last_is_maximized = false;
        self.last_hovered = None;
        self.last_left_down = false;
    }

    /// Limit input to the frame (titlebar + borders), let content receive clicks.
    pub fn update_input_region<D>(
        &self,
        compositor: &wl_compositor::WlCompositor,
        qh: &QueueHandle<D>,
        ui: &UiConfig,
        frame_style: FrameStyle,
    ) where
        D: 'static + wayland_client::Dispatch<wl_region::WlRegion, ()>,
    {
        if self.width <= 0 || self.height <= 0 {
            return;
        }

        let region = compositor.create_region(qh, ());
        region.add(0, 0, self.width, self.height);
        if self.content_width > 0 && self.content_height > 0 {
            let titlebar_height = titlebar_height(ui);
            let border_width = border_width(ui, frame_style);
            region.subtract(
                border_width,
                border_width + titlebar_height,
                self.content_width,
                self.content_height,
            );
        }
        self.surface.set_input_region(Some(&region));
        region.destroy();
    }

    /// Set the offset position relative to window
    pub fn set_offset(&self, x: i32, y: i32) {
        self.decoration.set_offset(x, y);
    }

    /// Sync the next commit with render_finish
    pub fn sync_next_commit(&self) {
        self.decoration.sync_next_commit();
    }
}

fn icon_size_for_titlebar(titlebar_height: i32) -> i32 {
    (titlebar_height - 4).clamp(6, titlebar_height.max(1))
}

fn rasterize_icon(svg: &str, size_px: i32) -> Option<tiny_skia::Pixmap> {
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size_px as u32, size_px as u32)?;
    let size = tree.size();
    let scale_x = size_px as f32 / size.width();
    let scale_y = size_px as f32 / size.height();
    let scale = scale_x.min(scale_y);
    let scaled_w = size.width() * scale;
    let scaled_h = size.height() * scale;
    let tx = (size_px as f32 - scaled_w) * 0.5;
    let ty = (size_px as f32 - scaled_h) * 0.5;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);
    let mut pixmap_mut = pixmap.as_mut();
    resvg::render(&tree, transform, &mut pixmap_mut);
    Some(pixmap)
}

/// Convert RGBA (0xRRGGBBAA) to ARGB (0xAARRGGBB) for wl_shm format
fn rgba_to_argb(rgba: u32) -> u32 {
    let r = (rgba >> 24) & 0xff;
    let g = (rgba >> 16) & 0xff;
    let b = (rgba >> 8) & 0xff;
    let a = rgba & 0xff;
    (a << 24) | (r << 16) | (g << 8) | b
}


fn build_button_fill(size_px: i32, color_argb: u32) -> Vec<u8> {
    if size_px <= 0 {
        return Vec::new();
    }
    let mut pixels = vec![0u8; (size_px * size_px * 4) as usize];
    if let Some(mut renderer) = Renderer::new(&mut pixels, size_px, size_px) {
        renderer.fill_rect(0, 0, size_px, size_px, color_argb);
    }
    pixels
}

fn build_button_bevel_cache(
    size_px: i32,
    titlebar_height: i32,
    bg_argb: u32,
    highlight_argb: u32,
    shadow_argb: u32,
    pressed: bool,
) -> Vec<u8> {
    if size_px <= 0 {
        return Vec::new();
    }
    let mut pixels = vec![0u8; (size_px * size_px * 4) as usize];
    if let Some(mut renderer) = Renderer::new(&mut pixels, size_px, size_px) {
        draw_button_bevel(
            &mut renderer,
            0,
            0,
            size_px,
            bg_argb,
            highlight_argb,
            shadow_argb,
            pressed,
            titlebar_height,
        );
    }
    pixels
}

fn draw_border_layer(renderer: &mut Renderer, offset: i32, thickness: i32, color: u32) {
    if thickness <= 0 {
        return;
    }

    let layer_width = renderer.width() - offset * 2;
    let layer_height = renderer.height() - offset * 2;
    if layer_width <= 0 || layer_height <= 0 {
        return;
    }

    let argb = rgba_to_argb(color);
    renderer.fill_rect(offset, offset, layer_width, thickness, argb);
    renderer.fill_rect(
        offset,
        offset + layer_height - thickness,
        layer_width,
        thickness,
        argb,
    );
    renderer.fill_rect(
        offset,
        offset + thickness,
        thickness,
        layer_height - thickness * 2,
        argb,
    );
    renderer.fill_rect(
        offset + layer_width - thickness,
        offset + thickness,
        thickness,
        layer_height - thickness * 2,
        argb,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_button_bevel(
    renderer: &mut Renderer,
    x: i32,
    y: i32,
    size: i32,
    bg_argb: u32,
    highlight_argb: u32,
    shadow_argb: u32,
    pressed: bool,
    titlebar_height: i32,
) {
    let unit = (size / titlebar_height.max(1)).max(1);
    let (highlight_argb, shadow_argb) = if pressed {
        (shadow_argb, bg_argb)
    } else {
        (highlight_argb, shadow_argb)
    };

    renderer.fill_rect(x, y, size, size, bg_argb);

    // highlight: horizontal, vertical
    renderer.fill_rect(x, y, size, unit, highlight_argb);
    renderer.fill_rect(x, y, unit, size, highlight_argb);

    if pressed {
        return;
    }

    if size >= 3 * unit {
        // shadow, inner: horizontal, vertical
        renderer.fill_rect(
            x + unit,
            y + size - 2 * unit,
            size - 2 * unit,
            unit,
            shadow_argb,
        );
        renderer.fill_rect(
            x + size - 2 * unit,
            y + unit,
            unit,
            size - 2 * unit,
            shadow_argb,
        );
    }

    // shadow, outer: horizontal, vertical
    renderer.fill_rect(x, y + size - unit, size, unit, shadow_argb);
    renderer.fill_rect(x + size - unit, y, unit, size, shadow_argb);
}

#[allow(clippy::too_many_arguments)]
fn draw_left_border(
    renderer: &mut Renderer,
    x: i32,
    y: i32,
    height: i32,
    color_argb: u32,
    titlebar_height: i32,
) {
    if x < 0 {
        return;
    }
    let unit = (height / titlebar_height.max(1)).max(1);
    renderer.fill_rect(x, y, unit, height, color_argb);
}

#[allow(clippy::too_many_arguments)]
fn draw_glyph_close(
    renderer: &mut Renderer,
    x: i32,
    y: i32,
    size: i32,
    color_argb: u32,
    titlebar_height: i32,
) {
    let unit = (size / titlebar_height.max(1)).max(1);
    let inner_size = (size - 2 * unit).max(1);
    let line_y = y + unit + (inner_size - unit) / 2;
    let line_x = x + 6 * unit;
    let line_w = size - 12 * unit;
    if line_w > 0 {
        renderer.fill_rect(line_x, line_y, line_w, unit, color_argb);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_glyph_caret(
    renderer: &mut Renderer,
    x: i32,
    y: i32,
    size: i32,
    color_argb: u32,
    down: bool,
    titlebar_height: i32,
) {
    let unit = (size / titlebar_height.max(1)).max(1);
    let span = (titlebar_height / 4).max(2);
    let glyph_height = span * 2 + 1;
    let glyph_height_px = glyph_height * unit;
    let inner_size = (size - 2 * unit).max(1);
    let top_y = y + unit + (inner_size - glyph_height_px) / 2 + 4 * unit;
    let mid_x = x + size / 2;

    for i in 0..=span {
        let row = top_y + i * unit;
        let (start, width) = if down {
            let w = unit + (span - i) * 2 * unit;
            (mid_x - (span - i) * unit, w)
        } else {
            let w = unit + i * 2 * unit;
            (mid_x - i * unit, w)
        };
        renderer.fill_rect(start, row, width, unit, color_argb);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_glyph_caret_pair(
    renderer: &mut Renderer,
    x: i32,
    y: i32,
    size: i32,
    color_argb: u32,
    titlebar_height: i32,
) {
    let unit = (size / titlebar_height.max(1)).max(1);
    let span = (titlebar_height / 4).max(2);
    let glyph_height = span * 2 + 1;
    let glyph_height_px = glyph_height * unit;
    let inner_size = (size - 2 * unit).max(1);
    let gap = unit;
    let total_height = glyph_height_px * 2 + gap;
    let top_y = y + unit + (inner_size - total_height) / 2 + 5 * unit;
    let mid_x = x + size / 2;

    for i in 0..=span {
        let row = top_y + i * unit;
        let width = unit + i * 2 * unit;
        let start = mid_x - i * unit;
        renderer.fill_rect(start, row, width, unit, color_argb);
    }

    let down_top = top_y + glyph_height_px + gap - 5 * unit;
    for i in 0..=span {
        let row = down_top + i * unit;
        let width = unit + (span - i) * 2 * unit;
        let start = mid_x - (span - i) * unit;
        renderer.fill_rect(start, row, width, unit, color_argb);
    }
}

impl Drop for Titlebar {
    fn drop(&mut self) {
        self.shm_pool.destroy();
        // Decoration (role object) must be destroyed before the surface
        self.decoration.destroy();
        self.surface.destroy();
    }
}
