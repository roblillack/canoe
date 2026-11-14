//! Canoe in a window of another Wayland or X11 session, for development.

use std::time::Duration;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportDma;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::utils::Transform;

use super::Backend;
use crate::compositor::render::CLEAR_COLOR;
use crate::compositor::State;

pub struct WinitData {
    backend: WinitGraphicsBackend<GlesRenderer>,
    output: Output,
    damage_tracker: OutputDamageTracker,
}

impl WinitData {
    pub fn renderer(&mut self) -> &mut GlesRenderer {
        self.backend.renderer()
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        self.backend.renderer().import_dmabuf(dmabuf, None).is_ok()
    }
}

pub fn init(state: &mut State) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init::<GlesRenderer>()?;
    let size = backend.window_size();
    let mode = Mode {
        size,
        refresh: 60_000,
    };
    let output = Output::new(
        "WINIT-1".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "canoe".into(),
            model: "nested".into(),
        },
    );
    output.add_mode(mode);
    output.set_preferred(mode);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );

    // Hardware-accelerated clients.
    #[cfg(drm_backend)]
    super::init_dmabuf(state, backend.renderer(), None);
    #[cfg(not(drm_backend))]
    super::init_dmabuf(state, backend.renderer());

    let damage_tracker = OutputDamageTracker::from_output(&output);
    state.backend = Backend::Winit(WinitData {
        backend,
        output: output.clone(),
        damage_tracker,
    });
    // The host draws the cursor over the window... but only ours knows the
    // right shape, so hide the host's.
    if let Backend::Winit(ref data) = state.backend {
        data.backend.window().set_cursor_visible(false);
    }
    state.add_output(output);

    state
        .loop_handle
        .insert_source(winit, |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                let output = match state.backend {
                    Backend::Winit(ref data) => data.output.clone(),
                    _ => return,
                };
                let mode = Mode {
                    size,
                    refresh: 60_000,
                };
                output.change_current_state(Some(mode), None, None, None);
                output.set_preferred(mode);
                state.output_changed(&output);
                state.queue_redraw_all();
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => render(state),
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            _ => {}
        })?;
    Ok(())
}

pub fn schedule_redraws(state: &mut State) {
    if let Backend::Winit(ref data) = state.backend {
        data.backend.window().request_redraw();
    }
}

fn render(state: &mut State) {
    let Backend::Winit(mut data) = std::mem::replace(&mut state.backend, Backend::None) else {
        return;
    };
    let output = data.output.clone();
    // Powered off: the window shows black.
    let displayed = state.managed_output(&output).is_some_and(|o| o.displayed());
    let mut frame_damage = Vec::new();
    let rendered = {
        let WinitData {
            backend,
            damage_tracker,
            ..
        } = &mut data;
        let age = backend.buffer_age().unwrap_or(0);
        let damage = match backend.bind() {
            Ok((renderer, mut framebuffer)) => {
                let elements = if displayed {
                    state.output_elements(renderer, &output)
                } else {
                    Vec::new()
                };
                damage_tracker
                    .render_output(renderer, &mut framebuffer, age, &elements, CLEAR_COLOR)
                    .ok()
                    .and_then(|result| result.damage.cloned())
                    .inspect(|damage| frame_damage = damage.clone())
            }
            Err(_) => None,
        };
        match damage {
            Some(damage) => backend.submit(Some(&damage)).is_ok(),
            None => backend.submit(None).is_ok(),
        }
    };
    state.backend = Backend::Winit(data);
    if rendered {
        if let Some(managed) = state.outputs.iter_mut().find(|o| o.output == output) {
            managed.redraw_needed = false;
        }
        if !displayed {
            return;
        }
        state.post_render(&output);
        state.capture_damage(&output, &frame_damage);
    }
    let _ = Duration::ZERO;
}
