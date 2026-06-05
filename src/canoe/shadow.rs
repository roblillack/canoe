//! Window shadow rendering.

use super::render::Renderer;
use super::shmfile::ShmPool;
use crate::protocol::RiverDecorationV1;
use std::cell::RefCell;
use std::rc::Rc;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::QueueHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShadowKey {
    frame_width: i32,
    frame_height: i32,
    shadow_size: i32,
    shadow_color: u32,
    scale: i32,
}

/// Shadow surface for a window.
pub struct WindowShadow {
    /// The wl_surface for the shadow.
    pub surface: wl_surface::WlSurface,
    /// The river decoration object.
    pub decoration: RiverDecorationV1,
    /// Double-buffered shm pool backing the wl_buffer.
    shm_pool: ShmPool,
    /// Current buffer width.
    pub width: i32,
    /// Current buffer height.
    pub height: i32,
    /// Current buffer width in pixels.
    pub buffer_width: i32,
    /// Current buffer height in pixels.
    pub buffer_height: i32,
    /// Output scale factor.
    pub scale: i32,
    /// wl_output names the shadow surface is currently on.
    pub output_names: Vec<u32>,
    last_key: Option<ShadowKey>,
}

impl WindowShadow {
    /// Create a new shadow surface.
    pub fn new(surface: wl_surface::WlSurface, decoration: RiverDecorationV1) -> Self {
        Self {
            surface,
            decoration,
            shm_pool: ShmPool::new("canoe-shadow"),
            width: 0,
            height: 0,
            buffer_width: 0,
            buffer_height: 0,
            scale: 1,
            output_names: Vec::new(),
            last_key: None,
        }
    }

    /// True once a wl_buffer is available for commit.
    pub fn is_ready(&self) -> bool {
        self.shm_pool.is_ready()
    }

    /// Record the geometry the next `render()` will draw into. The actual
    /// shm slot is rotated/allocated lazily inside `render()` so we don't
    /// burn a wl_buffer on a frame that turns out to be a no-op.
    pub fn ensure_buffer<D>(
        &mut self,
        frame_width: i32,
        frame_height: i32,
        shadow_size: i32,
        _shm: &wl_shm::WlShm,
        _qh: &QueueHandle<D>,
        scale: i32,
    ) where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, super::shmfile::ReleaseFlag>,
    {
        if frame_width <= 0 || frame_height <= 0 {
            return;
        }

        let shadow_size = shadow_size.max(0);
        let scale = scale.max(1);
        let width = frame_width + shadow_size * 2;
        let height = frame_height + shadow_size * 2;
        let buffer_width = width * scale;
        let buffer_height = height * scale;
        if buffer_width <= 0 || buffer_height <= 0 {
            return;
        }

        self.width = width;
        self.height = height;
        self.buffer_width = buffer_width;
        self.buffer_height = buffer_height;
        self.scale = scale;

        self.surface.set_buffer_scale(scale);
    }

    /// Clear input region so the shadow does not intercept clicks.
    pub fn update_input_region<D>(
        &self,
        compositor: &wl_compositor::WlCompositor,
        qh: &QueueHandle<D>,
    ) where
        D: 'static + wayland_client::Dispatch<wl_region::WlRegion, ()>,
    {
        let region = compositor.create_region(qh, ());
        self.surface.set_input_region(Some(&region));
        region.destroy();
    }

    /// Render the shadow directly into the next shm slot. The shm pool grows
    /// (with headroom) only when the requested slot size exceeds capacity;
    /// otherwise rendering reuses the existing fd/mmap/pool.
    #[allow(clippy::too_many_arguments)]
    pub fn render<D>(
        &mut self,
        frame_width: i32,
        frame_height: i32,
        shadow_size: i32,
        shadow_color: u32,
        scale: i32,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
    ) -> bool
    where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, super::shmfile::ReleaseFlag>,
    {
        if self.width <= 0
            || self.height <= 0
            || self.buffer_width <= 0
            || self.buffer_height <= 0
            || frame_width <= 0
            || frame_height <= 0
        {
            return false;
        }

        let key = ShadowKey {
            frame_width,
            frame_height,
            shadow_size: shadow_size.max(0),
            shadow_color,
            scale: scale.max(1),
        };
        // Same band geometry as the last render -> the surface still has the
        // right buffer attached. Skip the slot rotation entirely so we don't
        // burn a fresh wl_buffer just to commit the same pixels.
        if self.last_key == Some(key) {
            return false;
        }

        let buffer_width = self.buffer_width;
        let buffer_height = self.buffer_height;
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

        let pixels = match self.shm_pool.slot_bytes_mut(slot_idx) {
            Some(p) => p,
            None => return false,
        };

        #[cfg(feature = "debug-logging")]
        let fill_t0 = std::time::Instant::now();
        // The shadow only paints the perimeter band; the interior must stay
        // transparent. Either the slot's bytes are stale from a previous frame
        // (pool reused) or zero from a fresh allocation -- in both cases we
        // need a clean slate before drawing.
        pixels.fill(0);
        #[cfg(feature = "debug-logging")]
        let fill_us = fill_t0.elapsed().as_micros();

        #[cfg(feature = "debug-logging")]
        let draw_t0 = std::time::Instant::now();
        if let Some(mut renderer) = Renderer::new(pixels, buffer_width, buffer_height) {
            draw_shadow_soft(
                &mut renderer,
                frame_width,
                frame_height,
                shadow_size.max(0),
                shadow_size.max(0) / 2,
                shadow_color,
                scale.max(1),
            );
        }
        #[cfg(feature = "debug-logging")]
        let draw_us = draw_t0.elapsed().as_micros();

        #[cfg(feature = "debug-logging")]
        eprintln!(
            "[canoe shadow] {}x{} bytes={} alloc_fresh={} prepare={:.2}ms fill={:.2}ms draw={:.2}ms",
            buffer_width,
            buffer_height,
            buffer_width as i64 * buffer_height as i64 * 4,
            allocated_fresh,
            prep_us as f64 / 1000.0,
            fill_us as f64 / 1000.0,
            draw_us as f64 / 1000.0,
        );
        #[cfg(not(feature = "debug-logging"))]
        let _ = allocated_fresh;

        self.last_key = Some(key);
        true
    }

    /// Set the offset position relative to the window.
    pub fn set_offset(&self, x: i32, y: i32) {
        self.decoration.set_offset(x, y);
    }

    /// Sync the next commit with render_finish.
    pub fn sync_next_commit(&self) {
        self.decoration.sync_next_commit();
    }

    /// Commit the shadow surface.
    pub fn commit(&self) {
        if let Some(buffer) = self.shm_pool.current_buffer() {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface
                .damage_buffer(0, 0, self.buffer_width, self.buffer_height);
            self.surface.commit();
            // The compositor now owns this buffer until it sends a release
            // event; mark the slot off-limits for the next render.
            self.shm_pool.mark_attached();
        }
    }
}

fn rgba_to_argb(rgba: u32) -> u32 {
    let r = (rgba >> 24) & 0xff;
    let g = (rgba >> 16) & 0xff;
    let b = (rgba >> 8) & 0xff;
    let a = rgba & 0xff;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Window-independent shadow pieces, derived only from (shadow_size, radius,
/// colour) in device pixels: a 1-D edge falloff and one rounded-corner tile.
struct ShadowParts {
    /// Corner-tile side length, in pixels.
    cs: i32,
    /// Edge falloff: `edge[i]` is the colour at distance `s_px - i - 0.5`.
    edge: Vec<[u8; 4]>,
    /// Bottom-right rounded-corner tile, `cs * cs`, row-major.
    corner: Vec<[u8; 4]>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ShadowPartsKey {
    s_px: i32,
    r_px: i32,
    color: u32,
}

thread_local! {
    /// Cache of computed parts, shared by every window's shadow. In practice it
    /// only ever holds the active/inactive sizes, so it survives focus toggles
    /// and is rebuilt only when the shadow config (size/radius/colour) changes.
    static SHADOW_PARTS: RefCell<Vec<(ShadowPartsKey, Rc<ShadowParts>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Fetch the shared parts for this key, building (and caching) them on a miss.
fn shadow_parts(s_px: i32, r_px: i32, color: u32) -> Rc<ShadowParts> {
    let key = ShadowPartsKey { s_px, r_px, color };
    SHADOW_PARTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((_, parts)) = cache.iter().find(|(k, _)| *k == key) {
            return Rc::clone(parts);
        }
        let parts = Rc::new(build_shadow_parts(s_px, r_px, color));
        cache.push((key, Rc::clone(&parts)));
        // Bound growth across config changes (and tiny windows whose radius gets
        // clamped to an unusual value); evict the oldest beyond this.
        const MAX_ENTRIES: usize = 8;
        if cache.len() > MAX_ENTRIES {
            cache.remove(0);
        }
        parts
    })
}

fn build_shadow_parts(s_px: i32, r_px: i32, color: u32) -> ShadowParts {
    let base_alpha = (color & 0xff) as u8;
    let base_rgb = color & 0xffffff00;
    let cs = s_px + r_px;
    let s = s_px as f32;

    // Premultiplied ARGB for a signed distance into the band; transparent
    // outside (0, s]. Single source of truth for both edge and corner.
    let argb_for = |dist: f32| -> [u8; 4] {
        if dist <= 0.0 || dist > s {
            return [0, 0, 0, 0];
        }
        let falloff = 1.0 - dist / s;
        let alpha = (base_alpha as f32 * falloff * falloff)
            .round()
            .clamp(0.0, 255.0) as u8;
        if alpha == 0 {
            return [0, 0, 0, 0];
        }
        rgba_to_argb(base_rgb | alpha as u32).to_ne_bytes()
    };

    let edge: Vec<[u8; 4]> = (0..s_px).map(|i| argb_for(s - i as f32 - 0.5)).collect();
    let corner: Vec<[u8; 4]> = (0..cs * cs)
        .map(|idx| {
            // Corner-local distances qx = tx + 0.5, qy = ty + 0.5.
            let tx = (idx % cs) as f32 + 0.5;
            let ty = (idx / cs) as f32 + 0.5;
            argb_for((tx * tx + ty * ty).sqrt() - r_px as f32)
        })
        .collect();

    ShadowParts { cs, edge, corner }
}

/// Fill a byte buffer with a repeating 4-byte pattern using memcpy doubling,
/// which is far faster than a per-pixel loop in unoptimized builds.
fn fill_run(buf: &mut [u8], argb: [u8; 4]) {
    let total = buf.len();
    if total < 4 {
        return;
    }
    buf[..4].copy_from_slice(&argb);
    let mut filled = 4;
    while filled < total {
        let chunk = filled.min(total - filled);
        let (head, tail) = buf.split_at_mut(filled);
        tail[..chunk].copy_from_slice(&head[..chunk]);
        filled += chunk;
    }
}

pub(super) fn draw_shadow_soft(
    renderer: &mut Renderer,
    frame_width: i32,
    frame_height: i32,
    shadow_size: i32,
    corner_radius: i32,
    shadow_color: u32,
    scale: i32,
) {
    if shadow_size <= 0 {
        return;
    }

    let base_alpha = (shadow_color & 0xff) as u8;
    if base_alpha == 0 {
        return;
    }

    let shadow_size_px = shadow_size * scale;
    let frame_width_px = frame_width * scale;
    let frame_height_px = frame_height * scale;
    if shadow_size_px <= 0 || frame_width_px <= 0 || frame_height_px <= 0 {
        return;
    }

    let w = renderer.width();
    let s_px = shadow_size_px;
    let fw_px = frame_width_px;
    let fh_px = frame_height_px;
    let r_px = (corner_radius * scale).clamp(0, (fw_px.min(fh_px) / 2).max(0));

    // The rect's inset within the buffer is derived from the buffer width, NOT
    // assumed equal to the band width. This lets the buffer be allocated for a
    // larger shadow (e.g. the active size) while a smaller band is drawn into it
    // (e.g. an inactive window), with transparent margin in between -- so the
    // buffer never has to be reallocated on focus changes. The rect is centred
    // horizontally; vertically it is anchored at the same inset, with whatever
    // slack the (taller) buffer has left at the bottom.
    let m = (w - fw_px) / 2; // inset of the rect from the buffer edges
    if m < s_px {
        return; // band wouldn't fit; never happens for buffers we allocate
    }
    let inner_x1 = m + fw_px; // right edge of the rect
    let inner_y1 = m + fh_px; // bottom edge of the rect

    // The edge gradient and corner tile depend only on (band, radius, colour),
    // not on the window, so they are computed once and shared across all windows
    // and focus toggles. Only the assembly below is per-window.
    let parts = shadow_parts(s_px, r_px, shadow_color);
    let cs = parts.cs;
    let edge = &parts.edge;
    let corner = &parts.corner;

    let stride = (w * 4) as usize;
    let pixels = renderer.data_mut();

    // Corner anchors (buffer pixels), all relative to the rect.
    let right_x0 = inner_x1 - r_px; // BR / TR left column
    let bottom_y0 = inner_y1 - r_px; // BR / BL top row
    let cl = m + r_px - 1; // flipped TL/BL column and TL/TR row base

    // Four corners: flip the single tile into each rect corner.
    for ty in 0..cs {
        for tx in 0..cs {
            let argb = corner[(ty * cs + tx) as usize];
            if argb[3] == 0 {
                continue;
            }
            for &(bx, by) in &[
                (right_x0 + tx, bottom_y0 + ty), // bottom-right
                (right_x0 + tx, cl - ty),        // top-right
                (cl - tx, bottom_y0 + ty),       // bottom-left
                (cl - tx, cl - ty),              // top-left
            ] {
                let idx = by as usize * stride + bx as usize * 4;
                pixels[idx..idx + 4].copy_from_slice(&argb);
            }
        }
    }

    // Top / bottom straight edges: one colour per row across the middle span.
    // Top row (m - s_px + i) and bottom row (inner_y1 + s_px - 1 - i) are
    // equidistant from the rect, so both use edge[i].
    let span_lo = (m + r_px) as usize * 4;
    let span_hi = (inner_x1 - r_px) as usize * 4;
    for i in 0..s_px {
        let argb = edge[i as usize];
        if argb[3] == 0 {
            continue;
        }
        for &row_y in &[m - s_px + i, inner_y1 + s_px - 1 - i] {
            let row = row_y as usize * stride;
            fill_run(&mut pixels[row + span_lo..row + span_hi], argb);
        }
    }

    // Left / right straight edges: a fixed gradient run copied down each row of
    // the rect's straight vertical span [m + r_px, inner_y1 - r_px).
    let run_bytes = (s_px * 4) as usize;
    let mut left_run = vec![0u8; run_bytes];
    let mut right_run = vec![0u8; run_bytes];
    for i in 0..s_px as usize {
        left_run[i * 4..i * 4 + 4].copy_from_slice(&edge[i]);
        let mirrored = s_px as usize - 1 - i;
        right_run[mirrored * 4..mirrored * 4 + 4].copy_from_slice(&edge[i]);
    }
    let left_off = (m - s_px) as usize * 4;
    let right_off = inner_x1 as usize * 4;
    for y in (m + r_px)..(inner_y1 - r_px) {
        let row = y as usize * stride;
        pixels[row + left_off..row + left_off + run_bytes].copy_from_slice(&left_run);
        pixels[row + right_off..row + right_off + run_bytes].copy_from_slice(&right_run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straightforward per-pixel rounded-rect SDF with the rect inset from the
    /// buffer edges by `m = (w - fw_px)/2` (the buffer may be allocated for a
    /// larger shadow than the band being drawn, and may be taller than rect+band
    /// with slack at the bottom). Used as an oracle for the 9-slice version.
    #[allow(clippy::too_many_arguments)]
    fn reference(
        buf: &mut [u8],
        w: i32,
        h: i32,
        frame_width: i32,
        frame_height: i32,
        shadow_size: i32,
        corner_radius: i32,
        shadow_color: u32,
        scale: i32,
    ) {
        let base_alpha = (shadow_color & 0xff) as u8;
        let s_px = shadow_size * scale;
        let fw_px = frame_width * scale;
        let fh_px = frame_height * scale;
        let base_rgb = shadow_color & 0xffffff00;
        let r_px = (corner_radius * scale).clamp(0, (fw_px.min(fh_px) / 2).max(0));
        // Rect inset by m on each side; centre derived from that.
        let m = (w - fw_px) / 2;
        let cx = m as f32 + fw_px as f32 / 2.0;
        let cy = m as f32 + fh_px as f32 / 2.0;
        let hx = fw_px as f32 / 2.0;
        let hy = fh_px as f32 / 2.0;
        let r = r_px as f32;
        let s = s_px as f32;
        let stride = w * 4;
        for y in 0..h {
            let row = (y * stride) as usize;
            for x in 0..w {
                let dx = (x as f32 + 0.5) - cx;
                let dy = (y as f32 + 0.5) - cy;
                let qx = dx.abs() - (hx - r);
                let qy = dy.abs() - (hy - r);
                let mx = qx.max(0.0);
                let my = qy.max(0.0);
                let outside = (mx * mx + my * my).sqrt();
                let inside = qx.max(qy).min(0.0);
                let dist = outside + inside - r;
                if dist <= 0.0 || dist > s {
                    continue;
                }
                let falloff = 1.0 - dist / s;
                let alpha = (base_alpha as f32 * falloff * falloff)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                if alpha == 0 {
                    continue;
                }
                let argb = rgba_to_argb(base_rgb | alpha as u32).to_ne_bytes();
                let idx = row + (x * 4) as usize;
                buf[idx..idx + 4].copy_from_slice(&argb);
            }
        }
    }

    #[test]
    fn nine_slice_matches_reference() {
        for &color in &[0x00000033u32, 0x000000ccu32] {
            for &(fw, fh, ss, scale) in &[
                (80, 40, 20, 1),
                (120, 90, 20, 2),
                (50, 50, 10, 2),
                (200, 30, 15, 2),
                (41, 37, 20, 1), // odd frame dimensions
                (40, 40, 20, 1), // radius == half-extent (zero-width edges)
            ] {
                let cr = ss / 2;
                // `bss` is the buffer's shadow size: bss == ss is the exact-fit
                // case, bss > ss is the "buffer allocated for the active size
                // while a smaller (inactive) band is drawn" case we now support.
                for bss in [ss, ss + ss / 2, ss * 2] {
                    let w = (fw + bss * 2) * scale;
                    // Mirror production: the buffer is sized from the full frame
                    // height while the shadow is drawn for a shorter rect, leaving
                    // `extra` rows of slack below. Test both slack and exact-fit.
                    for extra in [(ss / 2) * scale, 0] {
                        let h = (fh + bss * 2) * scale + extra;
                        let mut got = vec![0u8; (w * h * 4) as usize];
                        let mut want = vec![0u8; (w * h * 4) as usize];
                        {
                            let mut r = Renderer::new(&mut got, w, h).unwrap();
                            draw_shadow_soft(&mut r, fw, fh, ss, cr, color, scale);
                        }
                        reference(&mut want, w, h, fw, fh, ss, cr, color, scale);

                        let mut max_alpha_diff = 0i32;
                        for i in (0..got.len()).step_by(4) {
                            let ga =
                                (u32::from_ne_bytes([got[i], got[i + 1], got[i + 2], got[i + 3]])
                                    >> 24) as i32;
                            let wa = (u32::from_ne_bytes([
                                want[i],
                                want[i + 1],
                                want[i + 2],
                                want[i + 3],
                            ]) >> 24) as i32;
                            max_alpha_diff = max_alpha_diff.max((ga - wa).abs());
                        }
                        // <=1 absorbs only float-rounding between the two ways of
                        // computing the same distance; geometry must match exactly.
                        assert!(
                            max_alpha_diff <= 1,
                            "color={color:#x} fw={fw} fh={fh} ss={ss} scale={scale} bss={bss} extra={extra}: max alpha diff {max_alpha_diff}",
                        );
                    }
                }
            }
        }
    }
}
