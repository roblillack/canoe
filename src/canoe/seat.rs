//! Seat (input) management

#![allow(dead_code)]

use crate::binding::{Action, PointerBinding, XkbBinding};
use crate::config::{Mode, XCursorTheme};
use crate::wm::{CursorShape, SeatHandle};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Weak;
use std::time::Instant;

use super::{OutputId, WindowId};

/// Seat identifier
pub type SeatId = usize;

/// A managed seat (input device group)
pub struct Seat {
    /// Unique seat ID
    pub id: SeatId,
    /// The compositor's seat
    pub rwm_seat: Option<SeatHandle>,
    /// Last cursor shape set for this seat while the pointer is on one of
    /// canoe's surfaces; reset when it moves on to a client
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
    pub xkb_bindings: Vec<XkbBinding>,
    /// Active pointer bindings
    pub pointer_bindings: Vec<PointerBinding>,

    /// Whether this seat has been removed
    pub removed: bool,
}

impl Seat {
    /// Create a new seat
    pub fn new(id: SeatId) -> Self {
        Self {
            id,
            rwm_seat: None,
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

        // The compositor matches key and button presses against the
        // enabled bindings of the seat's current mode.
        for binding in &mut self.xkb_bindings {
            binding.enabled = binding.mode == mode;
        }
        for binding in &mut self.pointer_bindings {
            binding.enabled = binding.mode == mode;
        }
        let _ = old_mode;
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

    /// Set the XCursor theme for compositor-drawn cursors, using the
    /// defaults for unset parts.
    pub fn set_xcursor_theme(&self, theme: &XCursorTheme) {
        if let Some(ref rwm_seat) = self.rwm_seat {
            rwm_seat
                .set_xcursor_theme(theme.name_or_default().to_string(), theme.size_or_default());
        }
    }

    /// Set the cursor shown over canoe's own surfaces and during pointer
    /// operations; `None` asks for the default arrow.
    pub fn set_cursor_shape(&mut self, shape: Option<CursorShape>) {
        let desired = shape.unwrap_or(CursorShape::Default);
        if self.cursor_shape == Some(desired) {
            return;
        }
        if let Some(ref rwm_seat) = self.rwm_seat {
            rwm_seat.set_cursor_shape(Some(desired));
        }
        self.cursor_shape = Some(desired);
    }

    /// Hide the cursor while the pointer is on canoe's surfaces.
    pub fn hide_cursor(&mut self) {
        if let Some(ref rwm_seat) = self.rwm_seat {
            rwm_seat.set_cursor_shape(None);
        }
        self.cursor_shape = None;
    }

    /// Add an XKB binding
    pub fn add_xkb_binding(&mut self, binding: XkbBinding) {
        self.xkb_bindings.push(binding);
    }

    /// Add a pointer binding
    pub fn add_pointer_binding(&mut self, binding: PointerBinding) {
        self.pointer_bindings.push(binding);
    }

    /// Initialize all bindings (call after seat is fully set up)
    pub fn initialize_bindings(&mut self) {
        // Enable bindings for current mode
        for binding in &mut self.xkb_bindings {
            if binding.mode == self.mode {
                binding.enabled = true;
            }
        }
        for binding in &mut self.pointer_bindings {
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
    SharePicker(OutputId),
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
