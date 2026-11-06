//! Titlebar rendering for windows

use super::render::Renderer;
use super::scale::{FractionalSurface, Origin, Scale};
use super::shmfile::ShmPool;
use crate::config::UiConfig;
use crate::protocol::RiverDecorationV1;
use resvg::{tiny_skia, usvg};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::QueueHandle;

/// Titlebar height in logical pixels: `0.8·font_size` rounded, doubled plus
/// one so the bar always has a center row.
pub fn titlebar_height(ui: &UiConfig) -> i32 {
    let base = (0.8 * ui.font_size).round() as i32;
    (base * 2 + 1).max(1)
}

/// Button background color (pressed left edge)
const BUTTON_BG_PRESSED_LEFT: u32 = 0xA0A0A0FF;

/// The fixed-size frame's outline, in logical pixels.
const BORDER_OUTER: i32 = 1;

/// The gap between the minimize and maximize buttons, in logical pixels.
const BUTTON_GAP: i32 = 1;

const ICON_CLOSE_SVG: &str = include_str!("../../assets/icons/close.svg");
const ICON_MINIMIZE_SVG: &str = include_str!("../../assets/icons/minimize.svg");
const ICON_MAXIMIZE_SVG: &str = include_str!("../../assets/icons/maximize.svg");
const ICON_UNMAXIMIZE_SVG: &str = include_str!("../../assets/icons/unmaximize.svg");

struct BaseFrameParams<'a> {
    ui: &'a UiConfig,
    is_active: bool,
    rects: &'a FrameRects,
    buffer_width: i32,
    buffer_height: i32,
    scale: Scale,
    show_minimize: bool,
    show_maximize: bool,
    frame_style: FrameStyle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ButtonCacheKey {
    size_px: i32,
    bevel: (i32, i32),
    button_bg: u32,
    button_highlight: u32,
    button_shadow: u32,
}

#[derive(Clone, Debug)]
struct ButtonCache {
    key: ButtonCacheKey,
    close_normal: Vec<u8>,
    close_pressed: Vec<u8>,
    hide_normal: Vec<u8>,
    hide_pressed: Vec<u8>,
    maximize_normal: Vec<u8>,
    maximize_pressed: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

    fn right(&self) -> i32 {
        self.x + self.width
    }

    fn bottom(&self) -> i32 {
        self.y + self.height
    }

    /// Shrunk by `n` on every side; grown for a negative `n`.
    fn inset(&self, n: i32) -> Rect {
        Rect {
            x: self.x + n,
            y: self.y + n,
            width: (self.width - 2 * n).max(0),
            height: (self.height - 2 * n).max(0),
        }
    }
}

/// Where the frame's parts land in the buffer, in device pixels. Each edge is
/// rounded from the output's origin, as the compositor rounds the client's
/// content (see [`Scale::edge`]), so the cut-out lines up with the content at
/// any fractional scale wherever the window sits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FrameRects {
    /// The whole buffer.
    outer: Rect,
    /// Inside the border: titlebar and content.
    inside: Rect,
    /// The titlebar strip.
    titlebar: Rect,
    /// The transparent content cut-out.
    content: Rect,
}

impl FrameRects {
    /// The frame of a surface whose corner sits at `origin`.
    fn new(
        scale: &Scale,
        origin: Origin,
        border_width: i32,
        titlebar_height: i32,
        content_width: i32,
        content_height: i32,
    ) -> Self {
        let width = content_width + border_width * 2;
        let height = content_height + titlebar_height + border_width * 2;
        let edge_x = |at: i32| scale.edge(origin.x, at);
        let edge_y = |at: i32| scale.edge(origin.y, at);
        let (x, w) = (
            edge_x(border_width),
            edge_x(border_width + content_width) - edge_x(border_width),
        );
        let (y, h) = (
            edge_y(border_width),
            edge_y(height - border_width) - edge_y(border_width),
        );
        let title_h = edge_y(border_width + titlebar_height) - y;
        Self {
            outer: Rect {
                x: 0,
                y: 0,
                width: edge_x(width),
                height: edge_y(height),
            },
            inside: Rect {
                x,
                y,
                width: w,
                height: h,
            },
            titlebar: Rect {
                x,
                y,
                width: w,
                height: title_h.min(h),
            },
            content: Rect {
                x,
                y: y + title_h,
                width: w,
                height: (h - title_h).max(0),
            },
        }
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

/// The SSD border thickness to reserve and draw for a given frame style, in
/// logical pixels.
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

/// The titlebar buttons in logical pixels, relative to the titlebar's
/// top-left corner.
pub fn button_rects(
    content_width: i32,
    titlebar_height: i32,
    show_minimize: bool,
    show_maximize: bool,
) -> TitlebarButtons {
    let strip = Rect {
        x: 0,
        y: 0,
        width: content_width,
        height: titlebar_height,
    };
    layout_buttons(strip, BUTTON_GAP, show_minimize, show_maximize)
}

/// Square buttons as tall as `strip`: close at the left, maximize and hide
/// at the right, `gap` apart.
fn layout_buttons(
    strip: Rect,
    gap: i32,
    show_minimize: bool,
    show_maximize: bool,
) -> TitlebarButtons {
    let size = strip.height;
    let y = strip.y;
    let close = Rect {
        x: strip.x,
        y,
        width: size,
        height: size,
    };

    // Right-to-left: maximize is the rightmost (if present), then hide; if
    // maximize is hidden, the hide button slides into the rightmost slot.
    let mut right_x = strip.right() - size;
    let maximize = if show_maximize {
        let r = Rect {
            x: right_x.max(strip.x),
            y,
            width: size,
            height: size,
        };
        right_x -= size + gap;
        Some(r)
    } else {
        None
    };
    let hide = if show_minimize {
        Some(Rect {
            x: right_x.max(strip.x),
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
    /// Viewport and fractional-scale handles for the surface
    pub fractional: FractionalSurface,
    /// Double-buffered shm pool backing the wl_buffer.
    shm_pool: ShmPool,
    /// Current surface width (logical px)
    pub width: i32,
    /// Current surface height (logical px)
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
    /// The scale the buffer is drawn at
    pub scale: Scale,
    /// The frame's parts in buffer pixels
    rects: FrameRects,
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
    pub fn new(
        surface: wl_surface::WlSurface,
        decoration: RiverDecorationV1,
        fractional: FractionalSurface,
    ) -> Self {
        Self {
            surface,
            decoration,
            fractional,
            shm_pool: ShmPool::new("canoe-titlebar"),
            width: 0,
            height: 0,
            buffer_width: 0,
            buffer_height: 0,
            content_width: 0,
            content_height: 0,
            border_width: 0,
            titlebar_height: 0,
            scale: Scale::default(),
            rects: FrameRects::default(),
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
    ///
    /// `origin` is where the surface's corner sits on its output: where the
    /// frame's edges fall in the buffer depends on it, so moving the window
    /// redraws the frame whenever that shifts an edge.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_buffer<D>(
        &mut self,
        content_width: i32,
        content_height: i32,
        _shm: &wl_shm::WlShm,
        _qh: &QueueHandle<D>,
        scale: Scale,
        origin: Origin,
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

        let titlebar_height = titlebar_height(ui);
        let border_width = border_width(ui, frame_style);
        // Remember the frame geometry so commit() can damage just the painted
        // border ring + titlebar instead of the whole (mostly transparent) buffer.
        self.border_width = border_width;
        self.titlebar_height = titlebar_height;
        let width = content_width + border_width * 2;
        let height = content_height + titlebar_height + border_width * 2;
        let (buffer_width, buffer_height) =
            self.fractional
                .apply(&self.surface, width, height, &scale, origin);
        if buffer_width <= 0 || buffer_height <= 0 {
            return;
        }
        let rects = FrameRects::new(
            &scale,
            origin,
            border_width,
            titlebar_height,
            content_width,
            content_height,
        );

        if self.width != width
            || self.height != height
            || self.buffer_width != buffer_width
            || self.buffer_height != buffer_height
            || self.scale != scale
            || self.rects != rects
        {
            self.width = width;
            self.height = height;
            self.buffer_width = buffer_width;
            self.buffer_height = buffer_height;
            self.content_width = content_width;
            self.content_height = content_height;
            self.scale = scale;
            self.rects = rects;
            // The icon and button caches are keyed by their pixel size and
            // rebuild themselves when it changes; keeping them saves
            // re-rasterizing the icons every time a move shifts an edge.
            self.dirty = true;
            self.needs_full_damage = true;
        } else {
            self.content_width = content_width;
            self.content_height = content_height;
        }
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
        let mut renderer = match Renderer::new(pixels, params.buffer_width, params.buffer_height) {
            Some(renderer) => renderer,
            None => return false,
        };

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
        let rects = params.rects;
        let line = params.scale.line();
        if matches!(params.frame_style, FrameStyle::FixedSize) {
            // Non-resizable toplevels: the border is just the outline,
            // hugging the content.
            fill_ring(
                &mut renderer,
                rects.outer,
                rects.inside,
                border_colors.outer,
            );
        } else {
            // A thin outer and inner line with the bulk of the border between
            // them, however many device pixels the border comes to.
            let outer_in = rects.outer.inset(line);
            let inner_out = rects.inside.inset(-line);
            fill_ring(&mut renderer, rects.outer, outer_in, border_colors.outer);
            fill_ring(&mut renderer, outer_in, inner_out, border_colors.mid);
            fill_ring(&mut renderer, inner_out, rects.inside, border_colors.inner);
        }

        let bg_argb = rgba_to_argb(titlebar_bg);
        let title = rects.titlebar;
        if title.height > 0 {
            renderer.fill_rect(title.x, title.y, title.width, title.height, bg_argb);

            let buttons = layout_buttons(title, line, params.show_minimize, params.show_maximize);
            let button_border = rgba_to_argb(border_colors.outer);
            let mut divider = |x: i32| {
                if x >= title.x {
                    renderer.fill_rect(x, title.y, line, title.height, button_border);
                }
            };
            divider(buttons.close.right());
            if let Some(hide) = buttons.hide {
                divider(hide.x - line);
            }
            if let Some(maximize) = buttons.maximize {
                divider(maximize.x - line);
            }

            if rects.content.height > 0 {
                renderer.fill_rect(
                    title.x,
                    title.bottom(),
                    title.width,
                    line,
                    rgba_to_argb(border_colors.outer),
                );
            }
        }

        true
    }

    fn ensure_button_cache(&mut self, ui: &UiConfig, size_px: i32, bevel: (i32, i32)) {
        if size_px <= 0 {
            return;
        }

        let key = ButtonCacheKey {
            size_px,
            bevel,
            button_bg: ui.button_bg,
            button_highlight: ui.button_highlight,
            button_shadow: ui.button_shadow,
        };
        let needs_rebuild = match self.button_cache {
            Some(ref cache) => cache.key != key,
            None => true,
        };

        if needs_rebuild {
            let button_bg = rgba_to_argb(ui.button_bg);
            let highlight = rgba_to_argb(ui.button_highlight);
            let shadow = rgba_to_argb(ui.button_shadow);
            let close_pressed_bg = rgba_to_argb(BUTTON_BG_PRESSED_LEFT);
            let bevel_button = |pressed| {
                build_button_bevel_cache(size_px, bevel, button_bg, highlight, shadow, pressed)
            };

            self.button_cache = Some(ButtonCache {
                key,
                close_normal: build_button_fill(size_px, button_bg),
                close_pressed: build_button_fill(size_px, close_pressed_bg),
                hide_normal: bevel_button(false),
                hide_pressed: bevel_button(true),
                maximize_normal: bevel_button(false),
                maximize_pressed: bevel_button(true),
            });
        }
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

        let scale = self.scale;
        let rects = self.rects;
        let line = scale.line();
        let bevel = (scale.bevel(1), scale.bevel(2));
        let titlebar_height = titlebar_height(ui);
        let button_size = rects.titlebar.height;
        let icon_size = scale
            .px(icon_size_for_titlebar(titlebar_height))
            .clamp(1, button_size.max(1));
        let icons_ready = self.ensure_icon_cache(icon_size);
        self.ensure_button_cache(ui, button_size, bevel);

        let buffer_width = self.buffer_width;
        let buffer_height = self.buffer_height;
        if self.width <= 0
            || self.height <= 0
            || buffer_width <= 0
            || buffer_height <= 0
            || self.content_width <= 0
            || self.content_height <= 0
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
        let slot_idx = match self
            .shm_pool
            .prepare(buffer_width, buffer_height, stride, shm, qh)
        {
            Some(s) => s,
            None => return false,
        };
        let allocated_fresh = self.shm_pool.allocate_fresh_count() != allocated_before;
        #[cfg(feature = "debug-logging")]
        let prep_us = prep_t0.elapsed().as_micros();

        let base_params = BaseFrameParams {
            ui,
            is_active,
            rects: &rects,
            buffer_width,
            buffer_height,
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
            let strip = rects.titlebar;
            if strip.height > 0 {
                let buttons = layout_buttons(strip, line, show_minimize, show_maximize);
                let pressed_hover = if left_down { hovered_button } else { None };
                // Centers the icon in a button, nudged by `offset` when pressed.
                let icon_origin = |button: Rect, offset: i32| {
                    (
                        button.x + (button.width - icon_size) / 2 + offset,
                        button.y + (button.height - icon_size) / 2 + offset,
                    )
                };

                let close = buttons.close;
                let close_pressed = pressed_hover == Some(TitlebarButton::Close);
                if let Some(cache) = button_cache {
                    let close_bg = if close_pressed {
                        &cache.close_pressed
                    } else {
                        &cache.close_normal
                    };
                    renderer.blit_argb(close_bg, button_size, button_size, close.x, close.y);
                } else {
                    let close_bg = if close_pressed {
                        rgba_to_argb(BUTTON_BG_PRESSED_LEFT)
                    } else {
                        rgba_to_argb(ui.button_bg)
                    };
                    renderer.fill_rect(close.x, close.y, close.width, close.height, close_bg);
                }
                if icons_ready {
                    if let Some(ref icons) = self.icon_cache {
                        let (x, y) = icon_origin(close, 0);
                        renderer.blit_pixmap(&icons.close, x, y);
                    }
                } else {
                    draw_glyph_close(
                        &mut renderer,
                        close.x,
                        close.y,
                        close.width,
                        rgba_to_argb(border_colors.outer),
                        titlebar_height,
                    );
                }

                if let Some(hide) = buttons.hide {
                    let hide_pressed = pressed_hover == Some(TitlebarButton::Hide);
                    let hide_offset = if hide_pressed { bevel.0 } else { 0 };
                    if let Some(cache) = button_cache {
                        let hide_bg = if hide_pressed {
                            &cache.hide_pressed
                        } else {
                            &cache.hide_normal
                        };
                        renderer.blit_argb(hide_bg, button_size, button_size, hide.x, hide.y);
                    } else {
                        draw_button_bevel(
                            &mut renderer,
                            hide.x,
                            hide.y,
                            hide.width,
                            rgba_to_argb(ui.button_bg),
                            rgba_to_argb(ui.button_highlight),
                            rgba_to_argb(ui.button_shadow),
                            hide_pressed,
                            bevel,
                        );
                    }
                    if icons_ready {
                        if let Some(ref icons) = self.icon_cache {
                            let (x, y) = icon_origin(hide, hide_offset);
                            renderer.blit_pixmap(&icons.minimize, x, y);
                        }
                    } else {
                        draw_glyph_caret(
                            &mut renderer,
                            hide.x + hide_offset,
                            hide.y + hide_offset,
                            hide.width,
                            rgba_to_argb(border_colors.outer),
                            true,
                            titlebar_height,
                        );
                    }
                }

                if let Some(maximize) = buttons.maximize {
                    let maximize_pressed = pressed_hover == Some(TitlebarButton::Maximize);
                    let maximize_offset = if maximize_pressed { bevel.0 } else { 0 };
                    if let Some(cache) = button_cache {
                        let maximize_bg = if maximize_pressed {
                            &cache.maximize_pressed
                        } else {
                            &cache.maximize_normal
                        };
                        renderer.blit_argb(
                            maximize_bg,
                            button_size,
                            button_size,
                            maximize.x,
                            maximize.y,
                        );
                    } else {
                        draw_button_bevel(
                            &mut renderer,
                            maximize.x,
                            maximize.y,
                            maximize.width,
                            rgba_to_argb(ui.button_bg),
                            rgba_to_argb(ui.button_highlight),
                            rgba_to_argb(ui.button_shadow),
                            maximize_pressed,
                            bevel,
                        );
                    }
                    if icons_ready {
                        if let Some(ref icons) = self.icon_cache {
                            let icon = if is_maximized {
                                &icons.unmaximize
                            } else {
                                &icons.maximize
                            };
                            let (x, y) = icon_origin(maximize, maximize_offset);
                            renderer.blit_pixmap(icon, x, y);
                        }
                    } else if is_maximized {
                        draw_glyph_caret_pair(
                            &mut renderer,
                            maximize.x + maximize_offset,
                            maximize.y + maximize_offset,
                            maximize.width,
                            rgba_to_argb(border_colors.outer),
                            titlebar_height,
                        );
                    } else {
                        draw_glyph_caret(
                            &mut renderer,
                            maximize.x + maximize_offset,
                            maximize.y + maximize_offset,
                            maximize.width,
                            rgba_to_argb(border_colors.outer),
                            false,
                            titlebar_height,
                        );
                    }
                }

                // Render title text if we have a title and font
                if let Some(title_str) = title {
                    if !title_str.is_empty() {
                        let text_start = close.right() + line;
                        let text_padding = scale.px((ui.font_size * 0.5).round().max(0.0) as i32);
                        let right_x = buttons
                            .hide
                            .map(|r| r.x)
                            .or_else(|| buttons.maximize.map(|r| r.x))
                            .unwrap_or(strip.right());
                        let text_end = (right_x - line - text_padding).min(strip.right());
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
                                text_start,
                                strip.y,
                                text_width,
                                strip.height,
                                scale.factor(),
                                text_argb,
                                ui.font_size,
                                ui.font_name.as_deref(),
                                text_padding,
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
                    self.scale.device(),
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
        // Transparent content cut-out in buffer pixels. The separator line
        // between titlebar and content stays inside the damaged top strip.
        let content = self.rects.content;
        let cut_x0 = content.x;
        let cut_y0 = content.y + self.scale.line();
        let cut_x1 = content.right();
        let cut_y1 = content.bottom();

        // Degenerate geometry (tiny window, missing borders): just damage it all.
        if cut_x1 <= cut_x0 || cut_y1 <= cut_y0 {
            self.surface
                .damage_buffer(0, 0, self.buffer_width, self.buffer_height);
            #[cfg(feature = "debug-logging")]
            eprintln!(
                "[canoe damage] FRAME->FULL (degenerate) buf={}x{} scale={}",
                self.buffer_width,
                self.buffer_height,
                self.scale.device(),
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
                + (cut_x0 as i64 + (self.buffer_width - cut_x1) as i64) * (cut_y1 - cut_y0) as i64;
            eprintln!(
                "[canoe damage] FRAME buf={}x{} scale={} damaged={}px / full={}px ({:.1}%)",
                self.buffer_width,
                self.buffer_height,
                self.scale.device(),
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
    (titlebar_height * 4 / 5).clamp(6, titlebar_height.max(1))
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
    bevel: (i32, i32),
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
            bevel,
        );
    }
    pixels
}

/// Fill the ring between `outer` and `inner` (which lies within it).
fn fill_ring(renderer: &mut Renderer, outer: Rect, inner: Rect, color: u32) {
    let argb = rgba_to_argb(color);
    if inner.width <= 0 || inner.height <= 0 {
        renderer.fill_rect(outer.x, outer.y, outer.width, outer.height, argb);
        return;
    }
    renderer.fill_rect(outer.x, outer.y, outer.width, inner.y - outer.y, argb);
    renderer.fill_rect(
        outer.x,
        inner.bottom(),
        outer.width,
        outer.bottom() - inner.bottom(),
        argb,
    );
    renderer.fill_rect(outer.x, inner.y, inner.x - outer.x, inner.height, argb);
    renderer.fill_rect(
        inner.right(),
        inner.y,
        outer.right() - inner.right(),
        inner.height,
        argb,
    );
}

/// A raised button: highlight on the top/left edges, a doubled shadow on the
/// bottom/right. `one` and `two` are how deep the first and second logical
/// pixel of bevel run in device pixels ([`Scale::bevel`]). Pressed, the
/// button sinks: a shadow along the top/left edges only.
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
    (one, two): (i32, i32),
) {
    let (highlight_argb, shadow_argb) = if pressed {
        (shadow_argb, bg_argb)
    } else {
        (highlight_argb, shadow_argb)
    };

    renderer.fill_rect(x, y, size, size, bg_argb);

    // highlight: horizontal, vertical
    renderer.fill_rect(x, y, size, one, highlight_argb);
    renderer.fill_rect(x, y, one, size, highlight_argb);

    if pressed {
        return;
    }

    if size >= one + two {
        // shadow, inner (from `one` in to `two`): horizontal, vertical
        renderer.fill_rect(
            x + one,
            y + size - two,
            size - 2 * one,
            two - one,
            shadow_argb,
        );
        renderer.fill_rect(
            x + size - two,
            y + one,
            two - one,
            size - 2 * one,
            shadow_argb,
        );
    }

    // shadow, outer: horizontal, vertical
    renderer.fill_rect(x, y + size - one, size, one, shadow_argb);
    renderer.fill_rect(x + size - one, y, one, size, shadow_argb);
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
        self.fractional.destroy();
        // Decoration (role object) must be destroyed before the surface
        self.decoration.destroy();
        self.surface.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where wlroots draws a box at logical `x`, `w` wide, on an output at
    /// `scale`: device pixels `[round(x·s), round((x + w)·s))`.
    fn device_span(scale: f64, x: i32, w: i32) -> (i32, i32) {
        let at = |v: i32| (v as f32 * scale as f32).round() as i32;
        (at(x), at(x + w))
    }

    /// The frame's content cut-out, put where the compositor puts the frame,
    /// covers exactly the device pixels the client's content does.
    #[test]
    fn cut_out_matches_content_wherever_the_window_sits() {
        let (border, title) = (5, 25);
        let (content_w, content_h) = (200, 120);
        for device in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let scale = Scale::new(device);
            for window_x in -12..12 {
                for window_y in [-7, 0, 3, 30] {
                    let origin = Origin::new(window_x - border, window_y - border - title);
                    let rects =
                        FrameRects::new(&scale, origin, border, title, content_w, content_h);
                    let (frame_x, frame_x1) = device_span(device, origin.x, content_w + 2 * border);
                    let (frame_y, frame_y1) =
                        device_span(device, origin.y, content_h + title + 2 * border);
                    let (content_x, content_x1) = device_span(device, window_x, content_w);
                    let (content_y, content_y1) = device_span(device, window_y, content_h);

                    let at = format!("scale {device}, window at {window_x},{window_y}");
                    let content = rects.content;
                    assert_eq!(frame_x + content.x, content_x, "left edge, {at}");
                    assert_eq!(frame_x + content.right(), content_x1, "right edge, {at}");
                    assert_eq!(frame_y + content.y, content_y, "top edge, {at}");
                    assert_eq!(frame_y + content.bottom(), content_y1, "bottom edge, {at}");

                    // The buffer fills the box it's drawn into: no resampling.
                    assert_eq!(rects.outer.width, frame_x1 - frame_x, "buffer width, {at}");
                    assert_eq!(
                        rects.outer.height,
                        frame_y1 - frame_y,
                        "buffer height, {at}"
                    );
                }
            }
        }
    }

    #[test]
    fn titlebar_meets_the_content() {
        let scale = Scale::new(1.25);
        for y in -8..8 {
            let rects = FrameRects::new(&scale, Origin::new(0, y), 5, 25, 100, 80);
            assert_eq!(rects.titlebar.bottom(), rects.content.y);
            assert_eq!(rects.titlebar.y, rects.inside.y);
            assert_eq!(rects.content.bottom(), rects.inside.bottom());
            // 25 logical pixels at 1.25 are 31 or 32 device pixels, depending
            // on where the strip starts.
            assert!((31..=32).contains(&rects.titlebar.height));
        }
    }

    /// The case that used to leave a one-pixel gap: a 5px border at 1.25 with
    /// the window at x = 2. The content starts at device pixel round(2.5) = 3,
    /// and the frame at round(-3.75) = -4, so the cut-out must start 7 device
    /// pixels in, not the 6 a 5px border rounds to on its own.
    #[test]
    fn border_ends_where_content_starts() {
        let rects = FrameRects::new(&Scale::new(1.25), Origin::new(-3, 0), 5, 25, 100, 80);
        assert_eq!(rects.content.x, 7);
        let rects = FrameRects::new(&Scale::new(1.25), Origin::new(0, 0), 5, 25, 100, 80);
        assert_eq!(rects.content.x, 6);
    }
}
