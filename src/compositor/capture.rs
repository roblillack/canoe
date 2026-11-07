//! Screen capture: whole outputs (wlr-screencopy, ext-image-copy-capture)
//! and single windows (ext-image-copy-capture with foreign-toplevel
//! sources), as screenshot tools and xdg-desktop-portal-wlr use them.

use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ImportAll, ImportMem};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform};

use super::backend::Backend;
use super::protocols::capture_render::ShmCaptureRenderer;
use super::protocols::image_capture::{
    delegate_image_copy_capture, output_capture_size, FailureReason, ImageCaptureSource,
    ImageCopyCaptureHandler, ImageCopyCaptureState, ImageCopyFrame,
};
use super::protocols::screencopy::{
    delegate_screencopy, ScreencopyFrame, ScreencopyHandler, ScreencopyManagerState,
};
use super::render::{surface_tree_elements, CanoeElement, CLEAR_COLOR};
use super::State;

/// What to draw into a capture buffer.
enum CaptureTarget {
    /// Part of an output.
    Output {
        output: Output,
        region: Rectangle<i32, Physical>,
        transform: Transform,
        cursor: bool,
    },
    /// A window by its foreign-toplevel identifier.
    Window(String),
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

    fn window_key_for_identifier(&self, identifier: &str) -> Option<crate::wm::WindowKey> {
        self.windows
            .iter()
            .find(|w| w.foreign.as_ref().is_some_and(|f| f.identifier() == identifier))
            .map(|w| w.key)
    }

    /// Draw `target` into `buffer` with the backend's renderer.
    fn render_capture(&mut self, target: CaptureTarget, buffer: &WlBuffer) -> bool {
        let mut backend = std::mem::replace(&mut self.backend, Backend::None);
        let ok = match backend {
            Backend::Headless(ref mut data) => self.capture_with(&mut data.renderer, target, buffer),
            Backend::Winit(ref mut data) => self.capture_with(data.renderer(), target, buffer),
            Backend::Udev(ref mut data) => match data.renderer() {
                Some(renderer) => self.capture_with(renderer, target, buffer),
                None => false,
            },
            Backend::None => false,
        };
        self.backend = backend;
        ok
    }

    fn capture_with<R>(&mut self, renderer: &mut R, target: CaptureTarget, buffer: &WlBuffer) -> bool
    where
        R: ShmCaptureRenderer + ImportAll + ImportMem,
        R::TextureId: Clone + Send + 'static,
    {
        match target {
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
                    .map(|e| RelocateRenderElement::from_element(e, Point::from((-region.loc.x, -region.loc.y)), Relocate::Relative))
                    .collect();
                renderer
                    .render_to_shm(buffer, scale, transform, &elements, CLEAR_COLOR)
                    .is_ok()
            }
            CaptureTarget::Window(identifier) => {
                let Some(key) = self.window_key_for_identifier(&identifier) else {
                    return false;
                };
                let scale = self.window_scale(key);
                let window = self.window_by_key(key).unwrap();
                let geometry = window.window.geometry();
                let location = Point::<f64, smithay::utils::Logical>::from((-geometry.loc.x as f64, -geometry.loc.y as f64)).to_physical(Scale::from(scale));
                let mut elements: Vec<CanoeElement<R>> = Vec::new();
                for (popup, offset) in smithay::desktop::PopupManager::popups_for_surface(window.wl_surface()) {
                    let at = location
                        + (offset - popup.geometry().loc).to_f64().to_physical(Scale::from(scale));
                    elements.extend(
                        surface_tree_elements(renderer, popup.wl_surface(), at, scale.into(), 1.0, Kind::Unspecified)
                            .into_iter()
                            .map(CanoeElement::Surface),
                    );
                }
                elements.extend(
                    surface_tree_elements(renderer, window.wl_surface(), location, scale.into(), 1.0, Kind::Unspecified)
                        .into_iter()
                        .map(CanoeElement::Surface),
                );
                renderer
                    .render_to_shm(buffer, scale, Transform::Normal, &elements, [0.0, 0.0, 0.0, 0.0])
                    .is_ok()
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

    fn image_copy_frame(&mut self, frame: ImageCopyFrame) {
        let target = match frame.source() {
            ImageCaptureSource::Output(output) => CaptureTarget::Output {
                output: output.clone(),
                region: frame.region(),
                transform: frame.transform(),
                cursor: frame.paint_cursors(),
            },
            ImageCaptureSource::Toplevel(handle) => CaptureTarget::Window(handle.identifier()),
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
