//! Outputs that exist only in memory, rendered with Pixman. For automated
//! tests and benchmarks: clients and screen capture see them like real
//! monitors, input comes from virtual devices.
//!
//! `CANOE_HEADLESS_OUTPUTS` lists the outputs as `WIDTHxHEIGHT[@SCALE]`,
//! comma-separated (default `1280x720`). With `CANOE_HEADLESS_REVERSE` set
//! they are announced last to first.

use std::time::{Duration, Instant};

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::{Bind, Offscreen};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::pixman;
use smithay::utils::{Size, Transform};

use super::Backend;
use crate::compositor::render::CLEAR_COLOR;
use crate::compositor::State;

const REFRESH: Duration = Duration::from_micros(16_667);

pub struct HeadlessData {
    pub renderer: PixmanRenderer,
    outputs: Vec<HeadlessOutput>,
    timer_armed: bool,
    last_frame: Option<Instant>,
    /// Frames rendered, for benchmarks.
    pub frames: u64,
}

struct HeadlessOutput {
    output: Output,
    damage_tracker: OutputDamageTracker,
    image: pixman::Image<'static, 'static>,
}

/// Parse `1280x720@1.5`.
fn parse_output(spec: &str) -> Option<(i32, i32, f64)> {
    let (size, scale) = match spec.split_once('@') {
        Some((size, scale)) => (size, scale.parse().ok()?),
        None => (spec, 1.0),
    };
    let (w, h) = size.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?, scale))
}

pub fn init(state: &mut State) -> Result<(), Box<dyn std::error::Error>> {
    let renderer = PixmanRenderer::new()?;
    state.backend = Backend::Headless(HeadlessData {
        renderer,
        outputs: Vec::new(),
        timer_armed: false,
        last_frame: None,
        frames: 0,
    });

    let specs = std::env::var("CANOE_HEADLESS_OUTPUTS").unwrap_or_else(|_| "1280x720".into());
    let mut x = 0;
    let mut outputs = Vec::new();
    for (index, spec) in specs.split(',').enumerate() {
        let Some((width, height, scale)) = parse_output(spec) else {
            return Err(format!("bad output spec {spec:?} in CANOE_HEADLESS_OUTPUTS").into());
        };
        let output = Output::new(
            format!("HEADLESS-{}", index + 1),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "canoe".into(),
                model: "headless".into(),
            },
        );
        let mode = Mode {
            size: (width, height).into(),
            refresh: 60_000,
        };
        output.add_mode(mode);
        output.set_preferred(mode);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(scale)),
            Some((x, 0).into()),
        );
        x += (width as f64 / scale).round() as i32;
        outputs.push(output);
    }
    // Announcing outputs last to first (as wlroots' headless backend ends
    // up doing) helps reproduce order-dependent behavior in tests.
    if std::env::var_os("CANOE_HEADLESS_REVERSE").is_some() {
        outputs.reverse();
    }
    for output in outputs {
        add_backend_output(state, &output)?;
        state.add_output(output);
    }
    Ok(())
}

fn add_backend_output(
    state: &mut State,
    output: &Output,
) -> Result<(), Box<dyn std::error::Error>> {
    let Backend::Headless(ref mut data) = state.backend else {
        return Ok(());
    };
    let size = output
        .current_mode()
        .map(|m| m.size)
        .unwrap_or((1, 1).into());
    let image = Offscreen::<pixman::Image<'static, 'static>>::create_buffer(
        &mut data.renderer,
        smithay::backend::allocator::Fourcc::Argb8888,
        Size::from((size.w, size.h)),
    )?;
    data.outputs.push(HeadlessOutput {
        output: output.clone(),
        damage_tracker: OutputDamageTracker::from_output(output),
        image,
    });
    Ok(())
}

/// The output's mode changed: give it a new buffer.
pub fn output_resized(state: &mut State, output: &Output) {
    if let Backend::Headless(ref mut data) = state.backend {
        data.outputs.retain(|o| &o.output != output);
    }
    let _ = add_backend_output(state, output);
}

pub fn schedule_redraws(state: &mut State) {
    let Backend::Headless(ref mut data) = state.backend else {
        return;
    };
    if data.timer_armed {
        return;
    }
    data.timer_armed = true;
    // Pace frames like a 60 Hz display.
    let delay = data
        .last_frame
        .map(|last| REFRESH.saturating_sub(last.elapsed()))
        .unwrap_or(Duration::ZERO);
    state
        .loop_handle
        .insert_source(Timer::from_duration(delay), |_, _, state| {
            if let Backend::Headless(ref mut data) = state.backend {
                data.timer_armed = false;
                data.last_frame = Some(Instant::now());
            }
            render_all(state);
            TimeoutAction::Drop
        })
        .expect("failed to add frame timer");
}

fn render_all(state: &mut State) {
    let outputs: Vec<Output> = state
        .outputs
        .iter()
        .filter(|o| o.enabled && o.redraw_needed)
        .map(|o| o.output.clone())
        .collect();
    for output in outputs {
        render_output(state, &output);
    }
}

fn render_output(state: &mut State, output: &Output) {
    let Backend::Headless(data) = std::mem::replace(&mut state.backend, Backend::None) else {
        return;
    };
    let mut data = data;
    let elements = state.output_elements(&mut data.renderer, output);
    let damage = {
        let HeadlessData {
            renderer, outputs, ..
        } = &mut data;
        match outputs.iter_mut().find(|o| &o.output == output) {
            Some(target) => match renderer.bind(&mut target.image) {
                Ok(mut framebuffer) => target
                    .damage_tracker
                    .render_output(renderer, &mut framebuffer, 0, &elements, CLEAR_COLOR)
                    .ok()
                    .map(|result| result.damage.cloned().unwrap_or_default()),
                Err(_) => None,
            },
            None => None,
        }
    };
    data.frames += u64::from(damage.is_some());
    state.backend = Backend::Headless(data);
    if let Some(managed) = state.outputs.iter_mut().find(|o| &o.output == output) {
        managed.redraw_needed = false;
    }
    state.post_render(output);
    if let Some(damage) = damage {
        state.capture_damage(output, &damage);
    }
}
