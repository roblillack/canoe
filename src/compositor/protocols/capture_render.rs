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
//! [`ShmCaptureRenderer::render_to_mem`] renders the same way into memory of
//! the compositor's own, for screenshots it hands out itself.
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

    /// Render `elements` like [`Self::render_to_shm`], into memory of
    /// `size`: rows of `size.w` ARGB8888 pixels (premultiplied, in
    /// little-endian byte order B, G, R, A), top row first, with no padding.
    fn render_to_mem<E: RenderElement<Self>>(
        &mut self,
        size: Size<i32, Physical>,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
        elements: &[E],
        clear_color: impl Into<Color32F>,
    ) -> Result<Vec<u8>, CaptureRenderError<Self::Error>>;
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

    fn render_to_mem<E: RenderElement<Self>>(
        &mut self,
        size: Size<i32, Physical>,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
        elements: &[E],
        clear_color: impl Into<Color32F>,
    ) -> Result<Vec<u8>, CaptureRenderError<Self::Error>> {
        render_to_mem_via_readback::<Self, pixman::Image<'static, 'static>, E>(
            self,
            Fourcc::Argb8888,
            size,
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

    fn render_to_mem<E: RenderElement<Self>>(
        &mut self,
        size: Size<i32, Physical>,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
        elements: &[E],
        clear_color: impl Into<Color32F>,
    ) -> Result<Vec<u8>, CaptureRenderError<Self::Error>> {
        render_to_mem_via_readback::<Self, GlesTexture, E>(
            self,
            Fourcc::Abgr8888,
            size,
            scale,
            transform,
            elements,
            clear_color,
        )
    }
}

/// Pixels read back from an offscreen render.
struct Readback<'a> {
    pixels: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
    /// The rows are in RGBA byte order rather than BGRA.
    swizzled: bool,
}

/// The parts of a capture that a render changed.
type Damage = Vec<Rectangle<i32, Physical>>;

/// Renders `elements` into an offscreen buffer of type `T` in
/// `offscreen_format` and `size`, reads it back and hands the pixels to
/// `consume`. Returns the rendered damage along with what `consume` returned.
#[allow(clippy::too_many_arguments)]
fn render_readback<R, T, E, F, O>(
    renderer: &mut R,
    offscreen_format: Fourcc,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: &[E],
    clear_color: Color32F,
    consume: F,
) -> Result<(Damage, O), CaptureRenderError<R::Error>>
where
    R: Renderer + Offscreen<T> + ExportMem,
    E: RenderElement<R>,
    F: FnOnce(Readback<'_>) -> O,
{
    let buffer_size = Size::<i32, smithay::utils::Buffer>::from((size.w, size.h));
    let mut target = renderer
        .create_buffer(offscreen_format, buffer_size)
        .map_err(CaptureRenderError::Render)?;
    let (damage, mapping, swizzled) = {
        let mut framebuffer = renderer
            .bind(&mut target)
            .map_err(CaptureRenderError::Render)?;
        let damage = render_elements(
            renderer,
            &mut framebuffer,
            size,
            scale,
            transform,
            elements,
            clear_color,
        )
        .map_err(CaptureRenderError::Render)?;
        let region = Rectangle::new((0, 0).into(), buffer_size);
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
    let stride = pixels.len() / height.max(1);
    if stride < width * 4 {
        return Err(CaptureRenderError::InvalidLayout);
    }
    let out = consume(Readback {
        pixels,
        width,
        height,
        stride,
        swizzled,
    });
    Ok((damage, out))
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
    let opaque = data.format == wl_shm::Format::Xrgb8888;

    let (damage, copied) = render_readback::<R, T, E, _, _>(
        renderer,
        offscreen_format,
        size,
        scale.into(),
        transform,
        elements,
        clear_color.into(),
        |readback| {
            shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
                check_layout::<R::Error>(&data, len)?;
                let row_len = readback.width * 4;
                for y in 0..readback.height.min(data.height as usize) {
                    let start = y * readback.stride;
                    let src = &readback.pixels[start..start + row_len];
                    // SAFETY: `check_layout` made sure every row lies within
                    // the pool, which stays mapped while this closure runs. We
                    // only write through raw pointers since the client may
                    // access the memory concurrently.
                    unsafe {
                        let dst = ptr.add(data.offset as usize + y * data.stride as usize);
                        if !readback.swizzled && !opaque {
                            std::ptr::copy_nonoverlapping(src.as_ptr(), dst, row_len);
                        } else {
                            for (x, px) in src.as_chunks::<4>().0.iter().enumerate() {
                                let [b, g, r, a] = bgra(*px, readback.swizzled);
                                let d = dst.add(x * 4);
                                *d = b;
                                *d.add(1) = g;
                                *d.add(2) = r;
                                *d.add(3) = if opaque { 0xff } else { a };
                            }
                        }
                    }
                }
                Ok(())
            })
            .map_err(map_access_error)
        },
    )?;
    copied??;
    Ok(damage)
}

/// Renders `elements` into an offscreen buffer of type `T` in
/// `offscreen_format` and returns its pixels. See
/// [`ShmCaptureRenderer::render_to_mem`].
pub fn render_to_mem_via_readback<R, T, E>(
    renderer: &mut R,
    offscreen_format: Fourcc,
    size: Size<i32, Physical>,
    scale: impl Into<Scale<f64>>,
    transform: Transform,
    elements: &[E],
    clear_color: impl Into<Color32F>,
) -> Result<Vec<u8>, CaptureRenderError<R::Error>>
where
    R: Renderer + Offscreen<T> + ExportMem,
    E: RenderElement<R>,
{
    if size.w <= 0 || size.h <= 0 {
        return Err(CaptureRenderError::InvalidLayout);
    }
    let (_, pixels) = render_readback::<R, T, E, _, _>(
        renderer,
        offscreen_format,
        size,
        scale.into(),
        transform,
        elements,
        clear_color.into(),
        |readback| {
            let row_len = readback.width * 4;
            let mut out = Vec::with_capacity(row_len * readback.height);
            for y in 0..readback.height {
                let start = y * readback.stride;
                let src = &readback.pixels[start..start + row_len];
                if readback.swizzled {
                    for px in src.as_chunks::<4>().0 {
                        out.extend_from_slice(&bgra(*px, true));
                    }
                } else {
                    out.extend_from_slice(src);
                }
            }
            out
        },
    )?;
    Ok(pixels)
}

/// A read-back pixel in B, G, R, A byte order.
fn bgra(px: [u8; 4], swizzled: bool) -> [u8; 4] {
    if swizzled {
        [px[2], px[1], px[0], px[3]]
    } else {
        px
    }
}
