//! Window shadow rendering.

use super::render::Renderer;
use crate::protocol::RiverDecorationV1;
use memmap2::MmapMut;
use std::fs::File;
use std::os::fd::AsFd;
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

#[derive(Clone, Debug)]
struct ShadowCache {
    key: ShadowKey,
    pixels: Vec<u8>,
}

/// Shadow surface for a window.
pub struct WindowShadow {
    /// The wl_surface for the shadow.
    pub surface: wl_surface::WlSurface,
    /// The river decoration object.
    pub decoration: RiverDecorationV1,
    /// Current buffer (if any).
    pub buffer: Option<wl_buffer::WlBuffer>,
    /// Shared memory pool.
    pub pool: Option<wl_shm_pool::WlShmPool>,
    /// Memory-mapped file for the buffer.
    pub memfile: Option<File>,
    /// Memory map pointer.
    pub mmap: Option<MmapMut>,
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
    cache: Option<ShadowCache>,
}

impl WindowShadow {
    /// Create a new shadow surface.
    pub fn new(surface: wl_surface::WlSurface, decoration: RiverDecorationV1) -> Self {
        Self {
            surface,
            decoration,
            buffer: None,
            pool: None,
            memfile: None,
            mmap: None,
            width: 0,
            height: 0,
            buffer_width: 0,
            buffer_height: 0,
            scale: 1,
            output_names: Vec::new(),
            cache: None,
        }
    }

    /// Ensure buffer is allocated for the given frame size.
    pub fn ensure_buffer<D>(
        &mut self,
        frame_width: i32,
        frame_height: i32,
        shadow_size: i32,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
        scale: i32,
    ) where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ()>,
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

        if self.width != width
            || self.height != height
            || self.buffer_width != buffer_width
            || self.buffer_height != buffer_height
            || self.scale != scale
            || self.buffer.is_none()
        {
            self.width = width;
            self.height = height;
            self.buffer_width = buffer_width;
            self.buffer_height = buffer_height;
            self.scale = scale;
            self.cache = None;

            if let Some(buffer) = self.buffer.take() {
                buffer.destroy();
            }
            if let Some(pool) = self.pool.take() {
                pool.destroy();
            }

            let stride = buffer_width * 4;
            let size = stride * buffer_height;

            let memfd = match memfd::MemfdOptions::default()
                .close_on_exec(true)
                .create("canoe-shadow")
            {
                Ok(fd) => fd,
                Err(_) => return,
            };

            if memfd.as_file().set_len(size as u64).is_err() {
                return;
            }

            let mmap = match unsafe { memmap2::MmapMut::map_mut(memfd.as_file()) } {
                Ok(m) => m,
                Err(_) => return,
            };

            let pool = shm.create_pool(memfd.as_file().as_fd(), size, qh, ());
            let buffer = pool.create_buffer(
                0,
                buffer_width,
                buffer_height,
                stride,
                wl_shm::Format::Argb8888,
                qh,
                (),
            );

            self.memfile = Some(memfd.into_file());
            self.mmap = Some(mmap);
            self.pool = Some(pool);
            self.buffer = Some(buffer);
        }

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

    /// Render the shadow into the current buffer.
    pub fn render(
        &mut self,
        frame_width: i32,
        frame_height: i32,
        shadow_size: i32,
        shadow_color: u32,
        scale: i32,
    ) -> bool {
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
        let needs_rebuild = match self.cache {
            Some(ref cache) => cache.key != key,
            None => true,
        };

        if !needs_rebuild {
            return false;
        }

        // vec! already zero-fills (== fully transparent), so no explicit clear.
        let mut pixels = vec![0u8; (self.buffer_width * self.buffer_height * 4) as usize];
        if let Some(mut renderer) =
            Renderer::new(&mut pixels, self.buffer_width, self.buffer_height)
        {
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

        self.cache = Some(ShadowCache { key, pixels });
        let pixels = match self.cache.as_ref() {
            Some(cache) => cache.pixels.as_slice(),
            None => return false,
        };
        if let Some(ref mut mmap) = self.mmap {
            let dst = mmap.as_mut();
            if dst.len() != pixels.len() {
                return false;
            }
            dst.copy_from_slice(pixels);
            return true;
        }
        false
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
        if let Some(ref buffer) = self.buffer {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface
                .damage_buffer(0, 0, self.buffer_width, self.buffer_height);
            self.surface.commit();
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

    let base_rgb = shadow_color & 0xffffff00;
    let w = renderer.width();
    let s_px = shadow_size_px;
    let fw_px = frame_width_px;
    let fh_px = frame_height_px;
    let r_px = (corner_radius * scale).clamp(0, (fw_px.min(fh_px) / 2).max(0));
    // The window rect is anchored at the top-left of the buffer, inset by the
    // band width (NOT centred in the buffer): the buffer can be taller than the
    // rect+band because it is sized from the full frame height while the shadow
    // is drawn for a slightly shorter rect, leaving slack at the bottom.
    let inner_y1 = s_px + fh_px; // bottom edge of the rect, in buffer pixels
    // Corner-tile side: band width plus corner radius. Its contents depend only
    // on (shadow_size, radius, colour) -- not on the window size -- so one tile
    // serves all four corners (mirrored).
    let cs = s_px + r_px;
    let s = s_px as f32;

    // Premultiplied ARGB for a signed distance into the band; transparent
    // outside (0, s]. This is the single source of truth for the falloff, used
    // for both the edge gradient and the corner tile.
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

    // 1-D edge falloff shared by all four straight edges. `edge[i]` is the
    // colour at perpendicular distance `s - i - 0.5` from the frame, where `i`
    // is the buffer offset from the band's faint outer edge.
    let edge: Vec<[u8; 4]> = (0..s_px).map(|i| argb_for(s - i as f32 - 0.5)).collect();

    // Window-independent rounded-corner tile (bottom-right orientation):
    // tile[ty*cs + tx] sits at corner-local distances qx = tx + 0.5, qy = ty + 0.5.
    let corner: Vec<[u8; 4]> = (0..cs * cs)
        .map(|idx| {
            let tx = (idx % cs) as f32 + 0.5;
            let ty = (idx / cs) as f32 + 0.5;
            argb_for((tx * tx + ty * ty).sqrt() - r_px as f32)
        })
        .collect();

    let stride = (w * 4) as usize;
    let pixels = renderer.data_mut();

    // Buffer rows of the bottom corners' / bottom edge's top. Anchored to the
    // rect (inner_y1), not the buffer height, so the band hugs the window even
    // when the buffer has slack below. Both stay within the buffer because
    // inner_y1 + s_px <= h by construction.
    let bottom_corner_y0 = inner_y1 - r_px;
    let right_x0 = w - cs; // == inner_x1 - r_px (rect is horizontally centred)

    // Four corners: flip the single tile into each rect corner.
    for ty in 0..cs {
        for tx in 0..cs {
            let argb = corner[(ty * cs + tx) as usize];
            if argb[3] == 0 {
                continue;
            }
            for &(bx, by) in &[
                (right_x0 + tx, bottom_corner_y0 + ty), // bottom-right
                (right_x0 + tx, cs - 1 - ty),           // top-right
                (cs - 1 - tx, bottom_corner_y0 + ty),   // bottom-left
                (cs - 1 - tx, cs - 1 - ty),             // top-left
            ] {
                let idx = by as usize * stride + bx as usize * 4;
                pixels[idx..idx + 4].copy_from_slice(&argb);
            }
        }
    }

    // Top / bottom straight edges: one colour per row across the middle span.
    // Top row i and bottom row (inner_y1 + s_px - 1 - i) are equidistant from
    // the rect, so both use edge[i].
    let span_lo = cs as usize * 4;
    let span_hi = (w - cs) as usize * 4;
    for i in 0..s_px {
        let argb = edge[i as usize];
        if argb[3] == 0 {
            continue;
        }
        for &row_y in &[i, inner_y1 + s_px - 1 - i] {
            let row = row_y as usize * stride;
            fill_run(&mut pixels[row + span_lo..row + span_hi], argb);
        }
    }

    // Left / right straight edges: a fixed gradient run copied down each row of
    // the rect's straight vertical span [cs, inner_y1 - r_px).
    let run_bytes = (s_px * 4) as usize;
    let mut left_run = vec![0u8; run_bytes];
    let mut right_run = vec![0u8; run_bytes];
    for i in 0..s_px as usize {
        left_run[i * 4..i * 4 + 4].copy_from_slice(&edge[i]);
        let mirrored = s_px as usize - 1 - i;
        right_run[mirrored * 4..mirrored * 4 + 4].copy_from_slice(&edge[i]);
    }
    let right_off = (w - s_px) as usize * 4;
    for y in cs..(inner_y1 - r_px) {
        let row = y as usize * stride;
        pixels[row..row + run_bytes].copy_from_slice(&left_run);
        pixels[row + right_off..row + right_off + run_bytes].copy_from_slice(&right_run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straightforward per-pixel rounded-rect SDF with the rect anchored at the
    /// top-left of the buffer (inset by the band), matching how the real shadow
    /// is positioned. The buffer may be taller than rect+band (slack at the
    /// bottom). Used as an oracle for the 9-slice `draw_shadow_soft`.
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
        // Rect anchored top-left at (s_px, s_px); centre derived from that.
        let cx = s_px as f32 + fw_px as f32 / 2.0;
        let cy = s_px as f32 + fh_px as f32 / 2.0;
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
                let w = (fw + ss * 2) * scale;
                // Mirror production: the buffer is sized from the full frame
                // height while the shadow is drawn for a shorter rect, leaving
                // `extra` rows of slack below. Test both the slack case and the
                // exact-fit case (extra == 0).
                for extra in [(ss / 2) * scale, 0] {
                    let h = (fh + ss * 2) * scale + extra;
                    let mut got = vec![0u8; (w * h * 4) as usize];
                    let mut want = vec![0u8; (w * h * 4) as usize];
                    {
                        let mut r = Renderer::new(&mut got, w, h).unwrap();
                        draw_shadow_soft(&mut r, fw, fh, ss, cr, color, scale);
                    }
                    reference(&mut want, w, h, fw, fh, ss, cr, color, scale);

                    let mut max_alpha_diff = 0i32;
                    for i in (0..got.len()).step_by(4) {
                        let ga = (u32::from_ne_bytes([got[i], got[i + 1], got[i + 2], got[i + 3]])
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
                        "color={color:#x} fw={fw} fh={fh} ss={ss} scale={scale} extra={extra}: max alpha diff {max_alpha_diff}",
                    );
                }
            }
        }
    }
}
