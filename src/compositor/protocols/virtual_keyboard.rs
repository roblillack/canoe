//! `zwp_virtual_keyboard_v1`, with key events going through the compositor's
//! normal keyboard path.
//!
//! Smithay's implementation sends a virtual keyboard's keys straight to the
//! focused client, past the compositor. wlroots treats a virtual keyboard
//! like any other keyboard instead, so tools like `wtype` (and canoe's test
//! driver) can trigger the window manager's key bindings. This does the
//! same. The keys are interpreted with the seat's keymap; the client's
//! keymap is accepted but not used, so it must describe the same layout.

use std::collections::HashSet;
use std::sync::Mutex;

use smithay::input::keyboard::Keycode;
use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
    zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

use crate::compositor::State;

const VERSION: u32 = 1;

pub struct VirtualKeyboardManagerState;

impl VirtualKeyboardManagerState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwpVirtualKeyboardManagerV1, ()> + 'static,
    {
        display.create_global::<D, ZwpVirtualKeyboardManagerV1, ()>(VERSION, ());
        Self
    }
}

/// Keys a virtual keyboard holds, released when it goes away.
#[derive(Default)]
pub struct VirtualKeyboardData {
    pressed: Mutex<HashSet<u32>>,
}

impl GlobalDispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpVirtualKeyboardManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardManagerV1,
        request: zwp_virtual_keyboard_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_virtual_keyboard_manager_v1::Request::CreateVirtualKeyboard { id, .. } = request {
            data_init.init(id, VirtualKeyboardData::default());
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, VirtualKeyboardData> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardV1,
        request: zwp_virtual_keyboard_v1::Request,
        data: &VirtualKeyboardData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_virtual_keyboard_v1::Request::Key { time, key, state: key_state } => {
                let pressed = key_state == 1;
                {
                    let mut held = data.pressed.lock().unwrap();
                    if pressed {
                        held.insert(key);
                    } else {
                        held.remove(&key);
                    }
                }
                // evdev codes are offset by 8 in xkb.
                state.on_key(Keycode::new(key + 8), pressed, time);
            }
            // Modifier state follows from the key events.
            zwp_virtual_keyboard_v1::Request::Modifiers { .. } => {}
            zwp_virtual_keyboard_v1::Request::Keymap { .. } => {}
            zwp_virtual_keyboard_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, _resource: &ZwpVirtualKeyboardV1, data: &VirtualKeyboardData) {
        let held: Vec<u32> = data.pressed.lock().unwrap().drain().collect();
        let time = state.start_time.elapsed().as_millis() as u32;
        for key in held {
            state.on_key(Keycode::new(key + 8), false, time);
        }
    }
}
