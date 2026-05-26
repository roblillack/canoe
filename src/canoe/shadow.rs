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

        let mut pixels = vec![0u8; (self.buffer_width * self.buffer_height * 4) as usize];
        clear_buffer(&mut pixels);
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

fn clear_buffer(pixels: &mut [u8]) {
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.copy_from_slice(&[0, 0, 0, 0]);
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
    let width = renderer.width();
    let height = renderer.height();
    let inner_x0 = shadow_size_px;
    let inner_y0 = shadow_size_px;
    let _inner_x1 = inner_x0 + frame_width_px;
    let _inner_y1 = inner_y0 + frame_height_px;

    let r_px = (corner_radius * scale).clamp(0, (frame_width_px.min(frame_height_px) / 2).max(0));
    let cx = inner_x0 + frame_width_px / 2;
    let cy = inner_y0 + frame_height_px / 2;
    let bx = frame_width_px as f32 / 2.0;
    let by = frame_height_px as f32 / 2.0;
    let r = r_px as f32;

    let pixels = renderer.data_mut();
    let stride = width * 4;
    for y in 0..height {
        let row = (y * stride) as usize;
        for x in 0..width {
            let px = (x as f32 + 0.5) - cx as f32;
            let py = (y as f32 + 0.5) - cy as f32;
            let qx = px.abs() - (bx - r);
            let qy = py.abs() - (by - r);
            let mx = qx.max(0.0);
            let my = qy.max(0.0);
            let outside = (mx * mx + my * my).sqrt();
            let inside = qx.max(qy).min(0.0);
            let dist = outside + inside - r;
            if dist <= 0.0 || dist > shadow_size_px as f32 {
                continue;
            }
            let t = (dist / shadow_size_px as f32).min(1.0);
            let falloff = 1.0 - t;
            let alpha = (base_alpha as f32 * falloff * falloff)
                .round()
                .clamp(0.0, 255.0) as u8;
            if alpha == 0 {
                continue;
            }
            let color = base_rgb | alpha as u32;
            let argb = rgba_to_argb(color).to_ne_bytes();
            let idx = row + (x * 4) as usize;
            if idx + 4 <= pixels.len() {
                pixels[idx..idx + 4].copy_from_slice(&argb);
            }
        }
    }
}
