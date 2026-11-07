//! Where outputs and input come from: a DRM/KMS session (udev), a window
//! in another compositor (winit), or nowhere (headless, for tests).

pub mod headless;
pub mod udev;
pub mod winit;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::output::{Mode, Output};

use super::State;

pub enum Backend {
    None,
    Headless(headless::HeadlessData),
    Winit(winit::WinitData),
    Udev(udev::UdevData),
}

impl Backend {
    /// Render outputs that need it, now or at their next frame.
    pub fn schedule_redraws(state: &mut State) {
        match state.backend {
            Backend::None => {}
            Backend::Headless(_) => headless::schedule_redraws(state),
            Backend::Winit(_) => winit::schedule_redraws(state),
            Backend::Udev(_) => udev::schedule_redraws(state),
        }
    }

    pub fn import_dmabuf(state: &mut State, dmabuf: &Dmabuf) -> bool {
        match state.backend {
            Backend::Winit(ref mut data) => data.import_dmabuf(dmabuf),
            Backend::Udev(ref mut data) => data.import_dmabuf(dmabuf),
            _ => false,
        }
    }

    pub fn switch_vt(state: &mut State, vt: i32) {
        if let Backend::Udev(ref mut data) = state.backend {
            data.switch_vt(vt);
        }
    }

    pub fn can_set_mode(state: &State, output: &Output, mode: Mode) -> bool {
        match state.backend {
            Backend::Headless(_) => true,
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
            Backend::Udev(_) => udev::set_mode(state, output, mode),
            _ => output.current_mode() == Some(mode),
        }
    }

    pub fn output_enabled(state: &mut State, output: &Output, enabled: bool) {
        if let Backend::Udev(_) = state.backend {
            udev::output_enabled(state, output, enabled);
        }
    }
}
