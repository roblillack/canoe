//! Screen capture: whole outputs (wlr-screencopy, ext-image-copy-capture)
//! and single windows (ext-image-copy-capture with foreign-toplevel
//! sources), as screenshot tools and xdg-desktop-portal-wlr use them; and
//! screenshots canoe hands out itself ([`Screenshot`]), of outputs, areas of
//! the layout and windows with their frames.

use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::backend::Backend;
use super::protocols::capture_render::ShmCaptureRenderer;
use super::protocols::image_capture::{
    delegate_image_copy_capture, output_capture_size, FailureReason, ImageCaptureSource,
    ImageCopyCaptureHandler, ImageCopyCaptureState, ImageCopyFrame,
};
use super::protocols::screencopy::{
    delegate_screencopy, ScreencopyFrame, ScreencopyHandler, ScreencopyManagerState,
};
use super::render::{canoe_surface, surface_with_popups, CanoeElement, CLEAR_COLOR};
use super::State;
use crate::wm::WindowKey;

/// What to draw into a capture buffer.
enum CaptureTarget {
    /// Part of an output.
    Output {
        output: Output,
        region: Rectangle<i32, Physical>,
        transform: Transform,
        cursor: bool,
    },
    /// A window, alone.
    Window(WindowCapture),
}

/// How to capture a window.
#[derive(Debug, Clone, Copy)]
pub struct WindowCapture {
    pub key: WindowKey,
    /// With the frame canoe draws around it.
    pub decoration: bool,
    /// With its shadow.
    pub shadow: bool,
    /// The scale to draw it at; `None` for the scale of its output.
    pub scale: Option<f64>,
}

impl WindowCapture {
    /// Just the window's content, as capture protocols want it.
    fn content(key: WindowKey) -> Self {
        Self {
            key,
            decoration: false,
            shadow: false,
            scale: None,
        }
    }
}

/// Where a capture goes.
enum CaptureDest<'a> {
    /// A client's shm buffer.
    Shm(&'a WlBuffer),
    /// Memory of this size, returned.
    Mem(Size<i32, Physical>),
}

/// A screenshot in memory: rows of `width` ARGB8888 pixels (premultiplied,
/// bytes B, G, R, A), top row first, with no padding.
#[derive(Debug, Clone)]
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    /// Pixels per logical pixel.
    pub scale: f64,
    pub data: Vec<u8>,
}

impl State {
    /// Report what was redrawn on `output` to the capture protocols, which
    /// complete captures that were waiting for something to change.
    pub fn capture_damage(&mut self, output: &Output, damage: &[Rectangle<i32, Physical>]) {
        ScreencopyManagerState::damage_output(self, output, damage);
        ImageCopyCaptureState::damage_output(self, output, damage);
    }

    /// A window drew something new.
    pub fn capture_window_damage(&mut self, key: crate::wm::WindowKey) {
        let Some(window) = self.window_by_key(key) else {
            return;
        };
        let Some(foreign) = window.foreign.clone() else {
            return;
        };
        let size = self.window_capture_size(key).unwrap_or_default();
        let rect = Rectangle::from_size((size.w, size.h).into());
        ImageCopyCaptureState::damage_toplevel(self, &foreign, &[rect]);
    }

    /// Tell cursor capture sessions where the pointer is.
    pub fn update_capture_cursors(&mut self) {
        ImageCopyCaptureState::update_cursors(self);
    }

    /// The scale a window is shown at: that of the output it is mostly on.
    fn window_scale(&self, key: crate::wm::WindowKey) -> f64 {
        self.window_by_key(key)
            .and_then(|w| self.output_for_surface(w.wl_surface()))
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0)
    }

    fn window_capture_size(&self, key: crate::wm::WindowKey) -> Option<Size<i32, Buffer>> {
        let window = self.window_by_key(key)?;
        let size = window.window.geometry().size;
        if size.w <= 0 || size.h <= 0 {
            return None;
        }
        let scale = self.window_scale(key);
        let size = size.to_f64().to_physical(scale).to_i32_round::<i32>();
        Some((size.w, size.h).into())
    }

    pub(super) fn window_key_for_identifier(&self, identifier: &str) -> Option<WindowKey> {
        self.windows
            .iter()
            .find(|w| {
                w.foreign
                    .as_ref()
                    .is_some_and(|f| f.identifier() == identifier)
            })
            .map(|w| w.key)
    }

    /// Draw `target` into `buffer` with the backend's renderer.
    fn render_capture(&mut self, target: CaptureTarget, buffer: &WlBuffer) -> bool {
        self.render_capture_to(target, CaptureDest::Shm(buffer))
            .is_some()
    }

    /// Draw `target` into memory of `size`.
    fn render_capture_mem(
        &mut self,
        target: CaptureTarget,
        size: Size<i32, Physical>,
    ) -> Option<Vec<u8>> {
        self.render_capture_to(target, CaptureDest::Mem(size))
            .flatten()
    }

    /// Draw `target` into `dest` with the backend's renderer. `None` if
    /// that failed; the pixels if `dest` is memory.
    fn render_capture_to(
        &mut self,
        target: CaptureTarget,
        dest: CaptureDest<'_>,
    ) -> Option<Option<Vec<u8>>> {
        let mut backend = std::mem::replace(&mut self.backend, Backend::None);
        let result = match backend {
            Backend::Headless(ref mut data) => self.capture_with(&mut data.renderer, target, dest),
            #[cfg(feature = "winit")]
            Backend::Winit(ref mut data) => self.capture_with(data.renderer(), target, dest),
            Backend::Udev(ref mut data) => match data.renderer() {
                Some(renderer) => self.capture_with(renderer, target, dest),
                None => None,
            },
            Backend::None => None,
        };
        self.backend = backend;
        result
    }

    fn capture_with<R>(
        &mut self,
        renderer: &mut R,
        target: CaptureTarget,
        dest: CaptureDest<'_>,
    ) -> Option<Option<Vec<u8>>>
    where
        R: ShmCaptureRenderer + ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let (elements, scale, transform, clear) = match target {
            CaptureTarget::Output {
                output,
                region,
                transform,
                cursor,
            } => {
                let scale = output.current_scale().fractional_scale();
                self.suppress_cursor = !cursor;
                let elements = self.output_elements(renderer, &output);
                self.suppress_cursor = false;
                let elements: Vec<RelocateRenderElement<CanoeElement<R>>> = elements
                    .into_iter()
                    .map(|e| {
                        RelocateRenderElement::from_element(
                            e,
                            Point::from((-region.loc.x, -region.loc.y)),
                            Relocate::Relative,
                        )
                    })
                    .collect();
                (elements, scale, transform, CLEAR_COLOR)
            }
            CaptureTarget::Window(capture) => {
                let (elements, scale) = self.window_elements(renderer, capture)?;
                let elements = elements
                    .into_iter()
                    .map(|e| {
                        RelocateRenderElement::from_element(
                            e,
                            Point::from((0, 0)),
                            Relocate::Relative,
                        )
                    })
                    .collect();
                (elements, scale, Transform::Normal, [0.0, 0.0, 0.0, 0.0])
            }
        };
        match dest {
            CaptureDest::Shm(buffer) => renderer
                .render_to_shm(buffer, scale, transform, &elements, clear)
                .ok()
                .map(|_| None),
            CaptureDest::Mem(size) => renderer
                .render_to_mem(size, scale, transform, &elements, clear)
                .ok()
                .map(Some),
        }
    }

    /// The scale a window capture is drawn at.
    fn window_capture_scale(&self, capture: WindowCapture) -> f64 {
        capture
            .scale
            .unwrap_or_else(|| self.window_scale(capture.key))
    }

    /// What a window capture covers, in global logical coordinates: the
    /// window's content, and its frame and shadow if asked for.
    fn window_capture_bounds(&self, capture: WindowCapture) -> Option<Rectangle<i32, Logical>> {
        let window = self.window_by_key(capture.key)?;
        let geometry = window.window.geometry();
        if geometry.size.w <= 0 || geometry.size.h <= 0 {
            return None;
        }
        let fullscreen_at = window
            .fullscreen
            .and_then(|key| self.output_by_key(key))
            .and_then(|o| self.output_geometry(&o.output))
            .map(|g| g.loc);
        let origin = fullscreen_at.unwrap_or(window.pos);
        let mut bounds = Rectangle::new(origin, geometry.size);
        if !capture.decoration && !capture.shadow || fullscreen_at.is_some() {
            return Some(bounds);
        }
        let context = self.context.borrow();
        let Some(w) = context.windows.get(&window.id) else {
            return Some(bounds);
        };
        let w = w.borrow();
        let mut add = |surface: &crate::canoe::surface::Surface| {
            let crate::canoe::surface::Role::Decoration { offset, .. } = surface.role() else {
                return;
            };
            let Some(content) = surface.content() else {
                return;
            };
            let (width, height) = content.logical_size;
            bounds = bounds.merge(Rectangle::new(
                origin + Point::from(offset),
                (width, height).into(),
            ));
        };
        if capture.decoration && window.ssd {
            if let Some(titlebar) = w.titlebar.as_ref() {
                add(&titlebar.surface);
            }
        }
        if capture.shadow && window.ssd {
            if let Some(shadow) = w.shadow.as_ref() {
                add(&shadow.surface);
            }
        }
        Some(bounds)
    }

    /// The physical size of a window capture.
    fn window_capture_buffer_size(&self, capture: WindowCapture) -> Option<Size<i32, Physical>> {
        let bounds = self.window_capture_bounds(capture)?;
        let scale = self.window_capture_scale(capture);
        let size = bounds.size.to_f64().to_physical(scale).to_i32_round::<i32>();
        (size.w > 0 && size.h > 0).then_some(size)
    }

    /// A window's elements, topmost first, placed relative to its capture
    /// bounds, and the scale they were made for.
    fn window_elements<R>(
        &mut self,
        renderer: &mut R,
        capture: WindowCapture,
    ) -> Option<(Vec<CanoeElement<R>>, f64)>
    where
        R: ShmCaptureRenderer + ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        let bounds = self.window_capture_bounds(capture)?;
        let scale = self.window_capture_scale(capture);
        let window = self.window_by_key(capture.key)?;
        let fullscreen_at = window
            .fullscreen
            .and_then(|key| self.output_by_key(key))
            .and_then(|o| self.output_geometry(&o.output))
            .map(|g| g.loc);
        let origin = bounds.loc.to_f64();
        let surface_origin = window.surface_origin(fullscreen_at).to_f64();
        let to_phys = |p: Point<f64, Logical>| (p - origin).to_physical(Scale::from(scale));
        let (main, popups) = surface_with_popups(
            renderer,
            window.wl_surface(),
            to_phys(surface_origin),
            Scale::from(scale),
            1.0,
        );
        let mut elements: Vec<CanoeElement<R>> = popups.into_iter().map(Into::into).collect();
        let decorated = window.ssd && fullscreen_at.is_none();
        let pos = window.pos.to_f64();
        let context = self.context.clone();
        let context = context.borrow();
        let w = context.windows.get(&window.id).map(|w| w.borrow());
        let decoration = |surface: &crate::canoe::surface::Surface, renderer: &mut R| {
            let crate::canoe::surface::Role::Decoration { offset, .. } = surface.role() else {
                return None;
            };
            canoe_surface(
                renderer,
                surface,
                pos + Point::<i32, Logical>::from(offset).to_f64(),
                origin,
                Scale::from(scale),
                1.0,
            )
        };
        if capture.decoration && decorated {
            if let Some(titlebar) = w.as_ref().and_then(|w| w.titlebar.as_ref()) {
                elements.extend(decoration(&titlebar.surface, renderer));
            }
        }
        elements.extend(main.into_iter().map(Into::into));
        if capture.shadow && decorated {
            if let Some(shadow) = w.as_ref().and_then(|w| w.shadow.as_ref()) {
                elements.extend(decoration(&shadow.surface, renderer));
            }
        }
        Some((elements, scale))
    }

    /// All of `output`, at its scale, upright.
    pub fn screenshot_output(&mut self, output: &Output, cursor: bool) -> Option<Screenshot> {
        let mode = output.current_mode()?;
        let size = output.current_transform().transform_size(mode.size);
        let scale = output.current_scale().fractional_scale();
        let target = CaptureTarget::Output {
            output: output.clone(),
            region: Rectangle::from_size(size),
            transform: Transform::Normal,
            cursor,
        };
        let data = self.render_capture_mem(target, size)?;
        Some(Screenshot {
            width: size.w as u32,
            height: size.h as u32,
            scale,
            data,
        })
    }

    /// A window, alone.
    pub fn screenshot_window(&mut self, capture: WindowCapture) -> Option<Screenshot> {
        let size = self.window_capture_buffer_size(capture)?;
        let scale = self.window_capture_scale(capture);
        let data = self.render_capture_mem(CaptureTarget::Window(capture), size)?;
        Some(Screenshot {
            width: size.w as u32,
            height: size.h as u32,
            scale,
            data,
        })
    }

    /// The part `area` (global logical coordinates) of the layout, at
    /// `scale`, or at the largest scale of the outputs it covers if `None`.
    /// Outputs at another scale are resampled; what no output covers is
    /// transparent.
    pub fn screenshot_area(
        &mut self,
        area: Rectangle<i32, Logical>,
        scale: Option<f64>,
        cursor: bool,
    ) -> Option<Screenshot> {
        if area.size.w <= 0 || area.size.h <= 0 {
            return None;
        }
        let outputs: Vec<(Output, Rectangle<i32, Logical>)> = self
            .outputs
            .iter()
            .filter(|o| o.displayed())
            .filter_map(|o| Some((o.output.clone(), self.output_geometry(&o.output)?)))
            .filter(|(_, geometry)| geometry.overlaps(area))
            .collect();
        if outputs.is_empty() {
            return None;
        }
        let scale = scale.unwrap_or_else(|| {
            outputs
                .iter()
                .map(|(o, _)| o.current_scale().fractional_scale())
                .fold(1.0, f64::max)
        });
        let size = area.size.to_f64().to_physical(scale).to_i32_round::<i32>();
        let (width, height) = (size.w.max(1) as usize, size.h.max(1) as usize);
        let mut data = vec![0u8; width * height * 4];
        for (output, geometry) in outputs {
            let shot = self.screenshot_output(&output, cursor)?;
            let Some(part) = geometry.intersection(area) else {
                continue;
            };
            compose(
                &mut data,
                (width, height),
                scale,
                area.loc,
                &shot,
                geometry.loc,
                part,
            );
        }
        Some(Screenshot {
            width: width as u32,
            height: height as u32,
            scale,
            data,
        })
    }
}

/// Copy the part `part` (global logical) of `src`, an output's screenshot
/// with its top left corner at `src_loc`, into `dst`, an image of `dst_size`
/// at `dst_scale` with its top left corner at `dst_loc`. Pixels are sampled
/// bilinearly where the scales differ.
fn compose(
    dst: &mut [u8],
    dst_size: (usize, usize),
    dst_scale: f64,
    dst_loc: Point<i32, Logical>,
    src: &Screenshot,
    src_loc: Point<i32, Logical>,
    part: Rectangle<i32, Logical>,
) {
    let (dst_w, dst_h) = dst_size;
    let to_dst = |v: i32, origin: i32, limit: usize| {
        (((v - origin) as f64 * dst_scale).round().max(0.0) as usize).min(limit)
    };
    let x0 = to_dst(part.loc.x, dst_loc.x, dst_w);
    let x1 = to_dst(part.loc.x + part.size.w, dst_loc.x, dst_w);
    let y0 = to_dst(part.loc.y, dst_loc.y, dst_h);
    let y1 = to_dst(part.loc.y + part.size.h, dst_loc.y, dst_h);
    let (src_w, src_h) = (src.width as usize, src.height as usize);
    if src_w == 0 || src_h == 0 {
        return;
    }
    let ratio = src.scale / dst_scale;
    // Where destination pixel 0 lies in the source, in source pixels.
    let offset_x = (dst_loc.x - src_loc.x) as f64 * src.scale;
    let offset_y = (dst_loc.y - src_loc.y) as f64 * src.scale;
    let same_scale = (ratio - 1.0).abs() < 1e-9;
    for y in y0..y1 {
        let row = &mut dst[y * dst_w * 4..(y + 1) * dst_w * 4];
        if same_scale {
            let sy = (offset_y + y as f64).round() as isize;
            if sy < 0 || sy as usize >= src_h {
                continue;
            }
            let sx0 = (offset_x + x0 as f64).round() as isize;
            for x in x0..x1 {
                let sx = sx0 + (x - x0) as isize;
                if sx < 0 || sx as usize >= src_w {
                    continue;
                }
                let at = (sy as usize * src_w + sx as usize) * 4;
                row[x * 4..x * 4 + 4].copy_from_slice(&src.data[at..at + 4]);
            }
            continue;
        }
        let fy = ((y as f64 + 0.5) * ratio + offset_y - 0.5).clamp(0.0, (src_h - 1) as f64);
        let (sy0, wy) = (fy.floor() as usize, fy.fract());
        let sy1 = (sy0 + 1).min(src_h - 1);
        for x in x0..x1 {
            let fx = ((x as f64 + 0.5) * ratio + offset_x - 0.5).clamp(0.0, (src_w - 1) as f64);
            let (sx0, wx) = (fx.floor() as usize, fx.fract());
            let sx1 = (sx0 + 1).min(src_w - 1);
            let px = |sx: usize, sy: usize, c: usize| src.data[(sy * src_w + sx) * 4 + c] as f64;
            for c in 0..4 {
                let top = px(sx0, sy0, c) * (1.0 - wx) + px(sx1, sy0, c) * wx;
                let bottom = px(sx0, sy1, c) * (1.0 - wx) + px(sx1, sy1, c) * wx;
                row[x * 4 + c] = (top * (1.0 - wy) + bottom * wy).round() as u8;
            }
        }
    }
}

impl ScreencopyHandler for State {
    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState {
        &mut self.screencopy_state
    }

    fn screencopy_frame(&mut self, frame: ScreencopyFrame) {
        let target = CaptureTarget::Output {
            output: frame.output().clone(),
            region: frame.region(),
            transform: frame.transform(),
            cursor: frame.overlay_cursor(),
        };
        if self.render_capture(target, &frame.buffer().clone()) {
            frame.success(self.start_time.elapsed());
        } else {
            frame.fail();
        }
    }
}

impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_capture_state
    }

    fn capture_source_size(&mut self, source: &ImageCaptureSource) -> Option<Size<i32, Buffer>> {
        match source {
            ImageCaptureSource::Output(output) => output_capture_size(output),
            ImageCaptureSource::Toplevel(handle) => {
                let key = self.window_key_for_identifier(&handle.identifier())?;
                self.window_capture_size(key)
            }
        }
    }

    fn cursor_position(&mut self, source: &ImageCaptureSource) -> Option<Point<i32, Buffer>> {
        // Nothing is captured from behind the lock screen.
        if self.lock_state != super::LockState::Unlocked {
            return None;
        }
        let (origin, size, scale) = match source {
            ImageCaptureSource::Output(output) => {
                let geometry = self.output_geometry(output)?;
                let scale = output.current_scale().fractional_scale();
                (geometry.loc, geometry.size, scale)
            }
            ImageCaptureSource::Toplevel(handle) => {
                let key = self.window_key_for_identifier(&handle.identifier())?;
                let window = self.window_by_key(key)?;
                if window.hidden {
                    return None;
                }
                // A fullscreen window's content fills its output.
                let origin = match window.fullscreen {
                    Some(output) => {
                        self.output_geometry(&self.output_by_key(output)?.output)?
                            .loc
                    }
                    None => window.pos,
                };
                (
                    origin,
                    window.window.geometry().size,
                    self.window_scale(key),
                )
            }
        };
        let local = self.pointer_location - origin.to_f64();
        if !Rectangle::from_size(size).to_f64().contains(local) {
            return None;
        }
        let at = local.to_physical(Scale::from(scale));
        Some((at.x.floor() as i32, at.y.floor() as i32).into())
    }

    fn image_copy_frame(&mut self, frame: ImageCopyFrame) {
        let target = match frame.source() {
            ImageCaptureSource::Output(output) => CaptureTarget::Output {
                output: output.clone(),
                region: frame.region(),
                transform: frame.transform(),
                cursor: frame.paint_cursors(),
            },
            ImageCaptureSource::Toplevel(handle) => {
                match self.window_key_for_identifier(&handle.identifier()) {
                    Some(key) => CaptureTarget::Window(WindowCapture::content(key)),
                    None => {
                        frame.fail(FailureReason::Unknown);
                        return;
                    }
                }
            }
        };
        if self.render_capture(target, &frame.buffer().clone()) {
            frame.success(self.start_time.elapsed());
        } else {
            frame.fail(FailureReason::Unknown);
        }
    }
}

delegate_screencopy!(State);
delegate_image_copy_capture!(State);
