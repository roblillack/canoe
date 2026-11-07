//! DRM/KMS outputs and libinput devices of a logind/seatd session.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::output::{Mode, Output};

use crate::compositor::State;

pub struct UdevData {}

impl UdevData {
    pub fn import_dmabuf(&mut self, _dmabuf: &Dmabuf) -> bool {
        false
    }

    pub fn switch_vt(&mut self, _vt: i32) {}

    pub fn renderer(&mut self) -> Option<&mut smithay::backend::renderer::gles::GlesRenderer> {
        None
    }

    pub fn can_set_mode(&self, _output: &Output, _mode: Mode) -> bool {
        false
    }
}

pub fn init(_state: &mut State) -> Result<(), Box<dyn std::error::Error>> {
    Err("the udev backend is not implemented yet".into())
}

pub fn schedule_redraws(_state: &mut State) {}

pub fn set_mode(_state: &mut State, _output: &Output, _mode: Mode) -> bool {
    false
}

pub fn output_enabled(_state: &mut State, _output: &Output, _enabled: bool) {}
