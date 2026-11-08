//! Rendering helpers for the screen-capture protocols.
//!
//! [`ShmCaptureRenderer::render_to_shm`] renders a slice of render elements
//! into a client-provided `wl_shm` buffer (as handed out by
//! [`ScreencopyFrame`](super::screencopy::ScreencopyFrame) and
//! [`ImageCopyFrame`](super::image_capture::ImageCopyFrame)). It is
//! implemented for [`PixmanRenderer`] (renders straight into the shared
//! memory) and [`GlesRenderer`] (renders into an offscreen texture and reads
//! it back). [`render_to_shm_via_readback`] is the generic read-back path and
//! can be used for other renderers implementing [`Offscreen`] + [`ExportMem`].
//!
//! The elements are expected in the *capture space*: physical pixels, in the
//! orientation of the logical (untransformed) source, with `(0, 0)` at the
//! top-left corner of the captured region. The `transform` is the output
//! transform (as in [`Output::current_transform`]), exactly like for
//! [`OutputDamageTracker`]; the buffer receives the transformed image, which
//! is what both capture protocols advertise.
//!
//! [`Output::current_transform`]: smithay::output::Output::current_transform

// A complete protocol API: not every accessor is used by canoe.
#![allow(dead_code)]

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::{Error as DamageError, OutputDamageTracker},
            element::RenderElement,
            gles::{GlesRenderer, GlesTexture},
            pixman::PixmanRenderer,
            Color32F, ExportMem, Offscreen, Renderer, Texture,
        },
    },
    reexports::{
        pixman,
        wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm},
    },
    utils::{Physical, Rectangle, Scale, Size, Transform},
    wayland::shm::{self, BufferAccessError, BufferData},
};

/// Errors of [`ShmCaptureRenderer::render_to_shm`].
#[derive(Debug, thiserror::Error)]
pub enum CaptureRenderError<E: std::error::Error> {
    /// The buffer is not a `wl_shm` buffer.
    #[error("the buffer is not a wl_shm buffer")]
    NotShm,
    /// Accessing the shared memory failed (e.g. the client truncated the pool).
    #[error("accessing the shm buffer failed: {0}")]
    BufferAccess(BufferAccessError),
    /// The shm buffer has a pixel format other than ARGB8888/XRGB8888.
    #[error("unsupported shm format {0:?}")]
    UnsupportedFormat(wl_shm::Format),
    /// The buffer layout (stride, offset, size) does not fit its pool or the renderer.
    #[error("invalid shm buffer layout")]
    InvalidLayout,
    /// The renderer failed.
    #[error("rendering failed: {0}")]
    Render(#[source] E),
}

/// Renderers that can render a set of elements into a client's `wl_shm` buffer.
pub trait ShmCaptureRenderer: Renderer + Sized {
    /// Render `elements` (front-to-back, like for [`OutputDamageTracker::render_output`])
    /// into `buffer`, clearing everything else with `clear_color`.
    ///
    /// The target size is the size of the shm buffer. `scale` is the scale
    /// the elements were created with (only used to query their geometry),
    /// `transform` the output transform to apply (`Transform::Normal` for
    /// toplevel captures).
    ///
    /// Returns the rendered damage (always the whole buffer, in capture
    /// space). The buffer must be ARGB8888 or XRGB8888.
    fn render_to_shm<E: RenderElement<Self>>(
        &mut self,
        buffer: &WlBuffer,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
        elements: &[E],
        clear_color: impl Into<Color32F>,
    ) -> Result<Vec<Rectangle<i32, Physical>>, CaptureRenderError<Self::Error>>;
}

/// Checks that the shm buffer is something we can render into and that it fits its pool.
fn check_layout<E: std::error::Error>(
    data: &BufferData,
    pool_len: usize,
) -> Result<(), CaptureRenderError<E>> {
    if !matches!(
        data.format,
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
    ) {
        return Err(CaptureRenderError::UnsupportedFormat(data.format));
    }
    if data.width <= 0 || data.height <= 0 || data.offset < 0 || data.stride < data.width * 4 {
        return Err(CaptureRenderError::InvalidLayout);
    }
    let end = data.offset as usize
        + data.stride as usize * (data.height as usize - 1)
        + data.width as usize * 4;
    if end > pool_len {
        return Err(CaptureRenderError::InvalidLayout);
    }
    Ok(())
}

fn map_access_error<E: std::error::Error>(err: BufferAccessError) -> CaptureRenderError<E> {
    match err {
        BufferAccessError::NotManaged => CaptureRenderError::NotShm,
        err => CaptureRenderError::BufferAccess(err),
    }
}

/// Renders `elements` into the bound framebuffer with a fresh damage tracker (i.e. fully).
fn render_elements<R, E>(
    renderer: &mut R,
    framebuffer: &mut R::Framebuffer<'_>,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: &[E],
    clear_color: Color32F,
) -> Result<Vec<Rectangle<i32, Physical>>, R::Error>
where
    R: Renderer,
    E: RenderElement<R>,
{
    let mut tracker = OutputDamageTracker::new(size, scale, transform);
    let result = tracker
        .render_output(renderer, framebuffer, 0, elements, clear_color)
        .map_err(|err| match err {
            DamageError::Rendering(err) => err,
            // A static damage tracker always has a mode.
            DamageError::OutputNoMode(_) => unreachable!(),
        })?;
    let damage = result.damage.cloned().unwrap_or_default();
    let sync = result.sync;
    renderer.wait(&sync)?;
    Ok(damage)
}

impl ShmCaptureRenderer for PixmanRenderer {
    fn render_to_shm<E: RenderElement<Self>>(
        &mut self,
        buffer: &WlBuffer,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
        elements: &[E],
        clear_color: impl Into<Color32F>,
    ) -> Result<Vec<Rectangle<i32, Physical>>, CaptureRenderError<Self::Error>> {
        // Rendering straight into the shm memory is not possible: smithay
        // forbids nested shm pool access, and the pixman renderer reads client
        // shm buffers while drawing their surfaces.
        render_to_shm_via_readback::<Self, pixman::Image<'static, 'static>, E>(
            self,
            buffer,
            Fourcc::Argb8888,
            scale,
            transform,
            elements,
            clear_color,
        )
    }
}

impl ShmCaptureRenderer for GlesRenderer {
    fn render_to_shm<E: RenderElement<Self>>(
        &mut self,
        buffer: &WlBuffer,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
        elements: &[E],
        clear_color: impl Into<Color32F>,
    ) -> Result<Vec<Rectangle<i32, Physical>>, CaptureRenderError<Self::Error>> {
        // Abgr8888 (GL_RGBA) is the one format every GLES implementation can
        // render to and read back.
        render_to_shm_via_readback::<Self, GlesTexture, E>(
            self,
            buffer,
            Fourcc::Abgr8888,
            scale,
            transform,
            elements,
            clear_color,
        )
    }
}

/// Renders `elements` into an offscreen buffer of type `T` in `offscreen_format`,
/// reads it back and copies it into the shm `buffer`. See
/// [`ShmCaptureRenderer::render_to_shm`].
pub fn render_to_shm_via_readback<R, T, E>(
    renderer: &mut R,
    buffer: &WlBuffer,
    offscreen_format: Fourcc,
    scale: impl Into<Scale<f64>>,
    transform: Transform,
    elements: &[E],
    clear_color: impl Into<Color32F>,
) -> Result<Vec<Rectangle<i32, Physical>>, CaptureRenderError<R::Error>>
where
    R: Renderer + Offscreen<T> + ExportMem,
    E: RenderElement<R>,
{
    let data = shm::with_buffer_contents(buffer, |_, len, data| {
        check_layout::<R::Error>(&data, len).map(|_| data)
    })
    .map_err(map_access_error)??;
    let size: Size<i32, Physical> = (data.width, data.height).into();
    let region = Rectangle::from_size((data.width, data.height).into());

    let mut target = renderer
        .create_buffer(offscreen_format, region.size)
        .map_err(CaptureRenderError::Render)?;
    let (damage, mapping, swizzle) = {
        let mut framebuffer = renderer
            .bind(&mut target)
            .map_err(CaptureRenderError::Render)?;
        let damage = render_elements(
            renderer,
            &mut framebuffer,
            size,
            scale.into(),
            transform,
            elements,
            clear_color.into(),
        )
        .map_err(CaptureRenderError::Render)?;
        // Prefer reading back in the shm byte order; not every driver supports
        // BGRA read-back, so fall back to RGBA and swap channels on the CPU.
        match renderer.copy_framebuffer(&framebuffer, region, Fourcc::Argb8888) {
            Ok(mapping) => (damage, mapping, false),
            Err(_) => {
                let mapping = renderer
                    .copy_framebuffer(&framebuffer, region, Fourcc::Abgr8888)
                    .map_err(CaptureRenderError::Render)?;
                (damage, mapping, true)
            }
        }
    };
    // Smithay renders offscreen targets with the top row first, so the
    // read-back rows are in buffer order (the GLES mapping reports `flipped()`
    // regardless, so it is deliberately not consulted).
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(CaptureRenderError::Render)?;
    let width = mapping.width() as usize;
    let height = mapping.height() as usize;
    let src_stride = pixels.len() / height.max(1);
    if src_stride < width * 4 {
        return Err(CaptureRenderError::InvalidLayout);
    }
    let opaque = data.format == wl_shm::Format::Xrgb8888;

    shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
        check_layout::<R::Error>(&data, len)?;
        let row_len = width * 4;
        for y in 0..height.min(data.height as usize) {
            let src = &pixels[y * src_stride..y * src_stride + row_len];
            // SAFETY: `check_layout` made sure every row lies within the pool,
            // which stays mapped while this closure runs. We only write through
            // raw pointers since the client may access the memory concurrently.
            unsafe {
                let dst = ptr.add(data.offset as usize + y * data.stride as usize);
                if !swizzle && !opaque {
                    std::ptr::copy_nonoverlapping(src.as_ptr(), dst, row_len);
                } else {
                    for (x, px) in src.as_chunks::<4>().0.iter().enumerate() {
                        let (b, r) = if swizzle {
                            (px[2], px[0])
                        } else {
                            (px[0], px[2])
                        };
                        let a = if opaque { 0xff } else { px[3] };
                        let d = dst.add(x * 4);
                        *d = b;
                        *d.add(1) = px[1];
                        *d.add(2) = r;
                        *d.add(3) = a;
                    }
                }
            }
        }
        Ok(damage)
    })
    .map_err(map_access_error)?
}
