//! Where outputs and input come from: a DRM/KMS session (udev), a window
//! in another compositor (winit), or nowhere (headless, for tests).

#[cfg(target_os = "linux")]
mod edid;
pub mod headless;
#[cfg(target_os = "linux")]
pub mod udev;
#[cfg(feature = "winit")]
pub mod winit;

use smithay::backend::allocator::dmabuf::Dmabuf;
#[cfg(target_os = "linux")]
use smithay::backend::drm::{DrmNode, NodeType};
use smithay::backend::egl::EGLDevice;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportDma;
use smithay::output::{Mode, Output};
#[cfg(target_os = "linux")]
use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
use smithay::wayland::dmabuf::DmabufState;

use super::handlers::DmabufGlobal;
use super::State;

/// Offer linux-dmabuf for buffers `renderer` can import.
///
/// Clients' EGL (Mesa) finds the GPU to render on through the default
/// feedback's main device; without it (and without wl_drm) it gives up on
/// hardware rendering ("failed to get driver name for fd -1"). So name the
/// renderer's render node, or `fallback` when EGL doesn't know it.
#[cfg(target_os = "linux")]
fn init_dmabuf(state: &mut State, renderer: &mut GlesRenderer, fallback: Option<DrmNode>) {
    let formats = renderer.dmabuf_formats();
    let node = EGLDevice::device_for_display(renderer.egl_context().display())
        .ok()
        .and_then(|device| device.try_get_render_node().ok().flatten())
        .or_else(|| {
            let node = fallback?;
            node.node_with_type(NodeType::Render)
                .and_then(Result::ok)
                .or(Some(node))
        });
    let mut dmabuf_state = DmabufState::new();
    let feedback = node.and_then(|node| {
        DmabufFeedbackBuilder::new(node.dev_id(), formats.clone())
            .build()
            .map_err(|err| eprintln!("canoe: dmabuf feedback failed: {err}"))
            .ok()
    });
    let global = match feedback {
        Some(feedback) => dmabuf_state
            .create_global_with_default_feedback::<State>(&state.display_handle, &feedback),
        None => {
            eprintln!("canoe: render node unknown, clients may fall back to software rendering");
            dmabuf_state.create_global::<State>(&state.display_handle, formats)
        }
    };
    state.dmabuf = Some(DmabufGlobal {
        state: dmabuf_state,
        global,
    });
}

/// Non-Linux variant: smithay's DRM reexports aren't compiled in, so we
/// can't derive a render node from the EGL device. Expose the dmabuf
/// global without a default-feedback hint; clients that need one may
/// fall back to software rendering, which is fine for development.
#[cfg(not(target_os = "linux"))]
fn init_dmabuf(state: &mut State, renderer: &mut GlesRenderer) {
    // Touch EGLDevice just to keep the import honest on all cfgs.
    let _ = EGLDevice::device_for_display(renderer.egl_context().display());
    let formats = renderer.dmabuf_formats();
    let mut dmabuf_state = DmabufState::new();
    let global = dmabuf_state.create_global::<State>(&state.display_handle, formats);
    state.dmabuf = Some(DmabufGlobal {
        state: dmabuf_state,
        global,
    });
}

#[allow(clippy::large_enum_variant)] // there is exactly one
pub enum Backend {
    None,
    Headless(headless::HeadlessData),
    #[cfg(feature = "winit")]
    Winit(winit::WinitData),
    #[cfg(target_os = "linux")]
    Udev(udev::UdevData),
}

impl Backend {
    /// Render outputs that need it, now or at their next frame.
    pub fn schedule_redraws(state: &mut State) {
        match state.backend {
            Backend::None => {}
            Backend::Headless(_) => headless::schedule_redraws(state),
            #[cfg(feature = "winit")]
            Backend::Winit(_) => winit::schedule_redraws(state),
            #[cfg(target_os = "linux")]
            Backend::Udev(_) => udev::schedule_redraws(state),
        }
    }

    pub fn import_dmabuf(state: &mut State, dmabuf: &Dmabuf) -> bool {
        match state.backend {
            #[cfg(feature = "winit")]
            Backend::Winit(ref mut data) => data.import_dmabuf(dmabuf),
            #[cfg(target_os = "linux")]
            Backend::Udev(ref mut data) => data.import_dmabuf(dmabuf),
            _ => false,
        }
    }

    pub fn switch_vt(state: &mut State, vt: i32) {
        #[cfg(target_os = "linux")]
        if let Backend::Udev(ref mut data) = state.backend {
            data.switch_vt(vt);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (state, vt);
        }
    }

    pub fn can_set_mode(state: &State, output: &Output, mode: Mode) -> bool {
        match state.backend {
            Backend::Headless(_) => true,
            #[cfg(target_os = "linux")]
            Backend::Udev(ref data) => data.can_set_mode(output, mode),
            // The window's size decides.
            _ => output.current_mode() == Some(mode),
        }
    }

    pub fn set_mode(state: &mut State, output: &Output, mode: Mode) -> bool {
        match state.backend {
            Backend::Headless(_) => {
                output.change_current_state(Some(mode), None, None, None);
                headless::output_resized(state, output);
                true
            }
            #[cfg(target_os = "linux")]
            Backend::Udev(_) => udev::set_mode(state, output, mode),
            _ => output.current_mode() == Some(mode),
        }
    }

    /// An output was enabled or disabled, or powered on or off: show it, or
    /// turn the monitor off (see `ManagedOutput::displayed`).
    pub fn update_output_power(state: &mut State, output: &Output) {
        match state.backend {
            #[cfg(target_os = "linux")]
            Backend::Udev(_) => udev::update_output_power(state, output),
            // Nothing to switch: such outputs are just not drawn.
            _ => state.queue_redraw_all(),
        }
        let _ = output;
    }

    /// Start a shown output over: forget frames in flight and set its mode
    /// again with the next frame. Recovers a monitor that went dark behind
    /// canoe's back.
    pub fn restart_output(state: &mut State, output: &Output) {
        match state.backend {
            #[cfg(target_os = "linux")]
            Backend::Udev(_) => udev::restart_output(state, output),
            _ => state.queue_redraw_all(),
        }
        let _ = output;
    }

    /// The number of entries in each of `output`'s gamma ramps, or `None`
    /// if its gamma can't be set.
    pub fn gamma_size(state: &State, output: &Output) -> Option<u32> {
        let _ = output;
        match state.backend {
            Backend::Headless(_) => Some(headless::GAMMA_SIZE),
            #[cfg(target_os = "linux")]
            Backend::Udev(ref data) => data.gamma_size(output),
            // The host compositor owns the monitor.
            _ => None,
        }
    }

    /// Apply gamma ramps (red, green and blue, each `gamma_size` entries)
    /// to `output`, or linear ones for `None`. Returns whether that worked.
    pub fn set_gamma(state: &mut State, output: &Output, ramps: Option<&[u16]>) -> bool {
        let _ = output;
        match state.backend {
            // No monitor to apply them to; accepted so tests can use the
            // protocol.
            Backend::Headless(_) => {
                ramps.is_none_or(|r| r.len() == 3 * headless::GAMMA_SIZE as usize)
            }
            #[cfg(target_os = "linux")]
            Backend::Udev(ref mut data) => data.set_gamma(output, ramps),
            _ => false,
        }
    }
}
