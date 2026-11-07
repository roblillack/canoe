//! What the keyboard can be focused on: a Wayland surface, or an X11
//! window, which also needs the X server's input focus.

use std::borrow::Cow;

use smithay::backend::input::KeyState;
use smithay::desktop::PopupKind;
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use super::State;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocus {
    Surface(WlSurface),
    /// An X11 window and the surface Xwayland draws it with.
    X11(Box<X11Surface>, WlSurface),
}

impl KeyboardFocus {
    pub fn surface(&self) -> &WlSurface {
        match self {
            KeyboardFocus::Surface(surface) | KeyboardFocus::X11(_, surface) => surface,
        }
    }
}

impl From<WlSurface> for KeyboardFocus {
    fn from(surface: WlSurface) -> Self {
        KeyboardFocus::Surface(surface)
    }
}

impl From<PopupKind> for KeyboardFocus {
    fn from(popup: PopupKind) -> Self {
        KeyboardFocus::Surface(popup.wl_surface().clone())
    }
}

impl From<KeyboardFocus> for WlSurface {
    fn from(focus: KeyboardFocus) -> Self {
        match focus {
            KeyboardFocus::Surface(surface) | KeyboardFocus::X11(_, surface) => surface,
        }
    }
}

impl IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            KeyboardFocus::Surface(surface) => surface.alive(),
            KeyboardFocus::X11(x11, surface) => x11.alive() && surface.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        Some(Cow::Borrowed(self.surface()))
    }
}

impl KeyboardTarget<State> for KeyboardFocus {
    fn enter(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            KeyboardFocus::Surface(surface) => {
                KeyboardTarget::enter(surface, seat, data, keys, serial)
            }
            KeyboardFocus::X11(x11, _) => KeyboardTarget::enter(&**x11, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        match self {
            KeyboardFocus::Surface(surface) => KeyboardTarget::leave(surface, seat, data, serial),
            KeyboardFocus::X11(x11, _) => KeyboardTarget::leave(&**x11, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            KeyboardFocus::Surface(surface) => {
                KeyboardTarget::key(surface, seat, data, key, state, serial, time)
            }
            KeyboardFocus::X11(x11, _) => {
                KeyboardTarget::key(&**x11, seat, data, key, state, serial, time)
            }
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            KeyboardFocus::Surface(surface) => {
                KeyboardTarget::modifiers(surface, seat, data, modifiers, serial)
            }
            KeyboardFocus::X11(x11, _) => {
                KeyboardTarget::modifiers(&**x11, seat, data, modifiers, serial)
            }
        }
    }
}
