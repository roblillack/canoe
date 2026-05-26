//! Seat (input) management

#![allow(dead_code)]

use crate::binding::{Action, PointerBinding, XkbBinding};
use crate::config::Mode;
use crate::protocol::{
    RiverLayerShellSeatV1, RiverPointerBindingV1, RiverSeatV1, RiverXkbBindingV1,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Weak;
use std::time::Instant;
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape as CursorShape, WpCursorShapeDeviceV1,
};

use super::{OutputId, WindowId};

/// Seat identifier
pub type SeatId = usize;

/// A managed seat (input device group)
pub struct Seat {
    /// Unique seat ID
    pub id: SeatId,
    /// River seat protocol object
    pub rwm_seat: Option<RiverSeatV1>,
    /// Layer shell seat object
    pub layer_shell_seat: Option<RiverLayerShellSeatV1>,
    /// Wayland seat object
    pub wl_seat: Option<WlSeat>,
    /// Global name of the wl_seat
    pub wl_seat_name: u32,
    /// Wayland pointer object
    pub wl_pointer: Option<WlPointer>,
    /// Cursor shape device for this seat
    pub cursor_shape_device: Option<WpCursorShapeDeviceV1>,
    /// Last pointer enter serial for cursor shape updates
    pub pointer_enter_serial: u32,
    /// Last cursor shape set for this seat
    pub cursor_shape: Option<CursorShape>,

    /// Current input mode
    pub mode: Mode,
    /// Whether focus is exclusive (layer shell has keyboard)
    pub focus_exclusive: bool,

    /// Pointer position
    pub pointer_x: i32,
    pub pointer_y: i32,
    /// Last pointer position in surface-local coordinates
    pub last_surface_x: i32,
    pub last_surface_y: i32,
    /// Pointer target for WM-owned surfaces
    pub pointer_target: PointerTarget,

    /// Window currently under the pointer
    pub window_below_pointer: Option<Weak<RefCell<super::Window>>>,

    /// Mouse button pressed for menu activation
    pub menu_click_button: Option<u32>,

    /// Last close-button click for double-click detection
    pub last_close_click: Option<(WindowId, Instant)>,

    /// Last desktop icon click for double-click detection
    pub last_icon_click: Option<(WindowId, Instant)>,

    /// Pending actions to execute during manage phase
    pub unhandled_actions: VecDeque<Action>,

    /// Active XKB bindings
    pub xkb_bindings: Vec<(XkbBinding, Option<RiverXkbBindingV1>)>,
    /// Active pointer bindings
    pub pointer_bindings: Vec<(PointerBinding, Option<RiverPointerBindingV1>)>,

    /// Whether this seat has been removed
    pub removed: bool,
}

impl Seat {
    /// Create a new seat
    pub fn new(id: SeatId) -> Self {
        Self {
            id,
            rwm_seat: None,
            layer_shell_seat: None,
            wl_seat: None,
            wl_seat_name: 0,
            wl_pointer: None,
            cursor_shape_device: None,
            pointer_enter_serial: 0,
            cursor_shape: None,
            mode: Mode::Default,
            focus_exclusive: false,
            pointer_x: 0,
            pointer_y: 0,
            last_surface_x: 0,
            last_surface_y: 0,
            pointer_target: PointerTarget::None,
            window_below_pointer: None,
            menu_click_button: None,
            last_close_click: None,
            last_icon_click: None,
            unhandled_actions: VecDeque::new(),
            xkb_bindings: Vec::new(),
            pointer_bindings: Vec::new(),
            removed: false,
        }
    }

    /// Queue an action for execution during manage phase
    pub fn queue_action(&mut self, action: Action) {
        self.unhandled_actions.push_back(action);
    }

    /// Drain and return all pending actions
    pub fn drain_actions(&mut self) -> Vec<Action> {
        self.unhandled_actions.drain(..).collect()
    }

    /// Switch to a different input mode
    pub fn switch_mode(&mut self, mode: Mode) {
        if self.mode == mode {
            return;
        }

        let old_mode = self.mode;
        self.mode = mode;

        // Disable bindings for old mode
        for (binding, rwm_binding) in &mut self.xkb_bindings {
            if binding.mode == old_mode && binding.enabled {
                if let Some(ref rwm) = rwm_binding {
                    rwm.disable();
                }
                binding.enabled = false;
            }
        }
        for (binding, rwm_binding) in &mut self.pointer_bindings {
            if binding.mode == old_mode && binding.enabled {
                if let Some(ref rwm) = rwm_binding {
                    rwm.disable();
                }
                binding.enabled = false;
            }
        }

        // Enable bindings for new mode
        for (binding, rwm_binding) in &mut self.xkb_bindings {
            if binding.mode == mode && !binding.enabled {
                if let Some(ref rwm) = rwm_binding {
                    rwm.enable();
                }
                binding.enabled = true;
            }
        }
        for (binding, rwm_binding) in &mut self.pointer_bindings {
            if binding.mode == mode && !binding.enabled {
                if let Some(ref rwm) = rwm_binding {
                    rwm.enable();
                }
                binding.enabled = true;
            }
        }
    }

    /// Update pointer position
    pub fn update_pointer_position(&mut self, x: i32, y: i32) {
        self.pointer_x = x;
        self.pointer_y = y;
    }

    /// Focus a window
    pub fn focus_window(&self, window: &super::Window) {
        if let (Some(ref rwm_seat), Some(ref rwm_window)) = (&self.rwm_seat, &window.rwm_window) {
            rwm_seat.focus_window(rwm_window);
        }
    }

    /// Clear keyboard focus
    pub fn clear_focus(&self) {
        if let Some(ref rwm_seat) = self.rwm_seat {
            rwm_seat.clear_focus();
        }
    }

    /// Start a pointer operation
    pub fn start_pointer_op(&self) {
        if let Some(ref rwm_seat) = self.rwm_seat {
            rwm_seat.op_start_pointer();
        }
    }

    /// End a pointer operation
    pub fn end_pointer_op(&self) {
        if let Some(ref rwm_seat) = self.rwm_seat {
            rwm_seat.op_end();
        }
    }

    /// Set XCursor theme
    pub fn set_xcursor_theme(&self, name: &str, size: u32) {
        if let Some(ref rwm_seat) = self.rwm_seat {
            rwm_seat.set_xcursor_theme(name.to_string(), size);
        }
    }

    /// Set cursor shape if supported by the compositor
    pub fn set_cursor_shape(&mut self, shape: Option<CursorShape>) {
        let device = match self.cursor_shape_device.as_ref() {
            Some(device) => device,
            None => return,
        };
        if self.pointer_enter_serial == 0 {
            return;
        }

        let desired = shape.unwrap_or(CursorShape::Default);
        if self.cursor_shape == Some(desired) {
            return;
        }

        device.set_shape(self.pointer_enter_serial, desired);
        self.cursor_shape = Some(desired);
    }

    /// Add an XKB binding
    pub fn add_xkb_binding(&mut self, binding: XkbBinding) {
        self.xkb_bindings.push((binding, None));
    }

    /// Add a pointer binding
    pub fn add_pointer_binding(&mut self, binding: PointerBinding) {
        self.pointer_bindings.push((binding, None));
    }

    /// Initialize all bindings (call after seat is fully set up)
    pub fn initialize_bindings(&mut self) {
        // Enable bindings for current mode
        for (binding, _) in &mut self.xkb_bindings {
            if binding.mode == self.mode {
                binding.enabled = true;
            }
        }
        for (binding, _) in &mut self.pointer_bindings {
            if binding.mode == self.mode {
                binding.enabled = true;
            }
        }
    }
}

/// Pointer target for WM-owned surfaces
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerTarget {
    None,
    Desktop(OutputId),
    Menu,
    MenuShield(OutputId),
    Titlebar(WindowId),
}

impl std::fmt::Debug for Seat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seat")
            .field("id", &self.id)
            .field("mode", &self.mode)
            .field("pointer_x", &self.pointer_x)
            .field("pointer_y", &self.pointer_y)
            .field("focus_exclusive", &self.focus_exclusive)
            .finish()
    }
}
