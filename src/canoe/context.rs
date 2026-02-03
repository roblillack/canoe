//! Core context - central state management

use crate::binding::{Action, Direction, PointerBinding, XkbBinding};
use crate::config::{load_config, Config, WindowDecoration};
use crate::protocol::river_window_management_v1::client::river_window_v1::Edges;
use crate::protocol::*;
use crate::rule;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape as CursorShape;

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::rc::Rc;

use super::{
    MenuItem, Output, OutputId, Seat, SeatId, Window, WindowId, WindowMenu, WindowMenuMode,
};

/// The central window manager context
pub struct Context {
    // Wayland globals
    pub rwm: Option<RiverWindowManagerV1>,
    pub rwm_xkb_bindings: Option<RiverXkbBindingsV1>,
    pub rwm_layer_shell: Option<RiverLayerShellV1>,
    pub rwm_input_manager: Option<RiverInputManagerV1>,
    pub rwm_libinput_config: Option<RiverLibinputConfigV1>,

    // Managed objects
    pub windows: HashMap<WindowId, Rc<RefCell<Window>>>,
    pub outputs: HashMap<OutputId, Rc<RefCell<Output>>>,
    pub seats: HashMap<SeatId, Rc<RefCell<Seat>>>,

    // Focus management
    pub focus_stack: Vec<WindowId>,
    pub focused_window: Option<WindowId>,
    last_focused_output: Option<OutputId>,

    // Current state
    pub current_output: Option<OutputId>,
    pub current_seat: Option<SeatId>,

    // ID generators
    next_window_id: WindowId,
    next_output_id: OutputId,
    next_seat_id: SeatId,

    // Configuration
    pub config: Config,

    // Runtime state
    pub running: bool,
    pub session_locked: bool,
    startup_unminimize_done: bool,

    /// Window menu surface and state
    pub window_menu: Option<WindowMenu>,
    pub window_menu_mode: Option<WindowMenuMode>,
    pub window_menu_shield: Option<super::ShieldSurface>,
    window_menu_alt_tab_stack: Option<Vec<WindowId>>,
    window_menu_alt_tab_focused: Option<WindowId>,
    window_menu_alt_tab_preview: Option<WindowId>,
    window_menu_alt_tab_preview_was_hidden: bool,
}

impl Context {
    /// Create a new context with default configuration
    pub fn new() -> Self {
        Self {
            rwm: None,
            rwm_xkb_bindings: None,
            rwm_layer_shell: None,
            rwm_input_manager: None,
            rwm_libinput_config: None,

            windows: HashMap::new(),
            outputs: HashMap::new(),
            seats: HashMap::new(),

            focus_stack: Vec::new(),
            focused_window: None,
            last_focused_output: None,

            current_output: None,
            current_seat: None,

            next_window_id: 0,
            next_output_id: 0,
            next_seat_id: 0,

            config: load_config(),

            running: true,
            session_locked: false,
            startup_unminimize_done: false,

            window_menu: None,
            window_menu_mode: None,
            window_menu_shield: None,
            window_menu_alt_tab_stack: None,
            window_menu_alt_tab_focused: None,
            window_menu_alt_tab_preview: None,
            window_menu_alt_tab_preview_was_hidden: false,
        }
    }

    /// Create a new window and add to context
    pub fn create_window(&mut self, rwm_window: RiverWindowV1) -> Rc<RefCell<Window>> {
        let id = self.next_window_id;
        self.next_window_id += 1;

        let mut window = Window::new(id);
        window.rwm_window = Some(rwm_window);

        // Assign to the preferred output for new windows.
        let output_id = self.current_output_for_new_window();
        if let Some(output_id) = output_id {
            if let Some(output) = self.outputs.get(&output_id) {
                window.output = Some(Rc::downgrade(output));
            }
        }

        // Stacking WM: all windows are floating by default
        window.floating = true;

        let window = Rc::new(RefCell::new(window));
        self.windows.insert(id, window.clone());
        self.focus_stack.push(id);

        window
    }

    /// Remove a window from context
    pub fn destroy_window(&mut self, window_id: WindowId) {
        // Remove from focus stack
        self.focus_stack.retain(|&id| id != window_id);

        // Update focused window if necessary
        if self.focused_window == Some(window_id) {
            self.focused_window = self.focus_stack.first().copied();
        }

        self.windows.remove(&window_id);
    }

    /// Create a new output and add to context
    pub fn create_output(&mut self, rwm_output: RiverOutputV1) -> Rc<RefCell<Output>> {
        let id = self.next_output_id;
        self.next_output_id += 1;

        let mut output = Output::new(id);
        output.rwm_output = Some(rwm_output);

        let output = Rc::new(RefCell::new(output));
        self.outputs.insert(id, output.clone());

        // Set as current output if first
        if self.current_output.is_none() {
            self.current_output = Some(id);
        }

        output
    }

    /// Remove an output from context
    pub fn destroy_output(&mut self, output_id: OutputId) {
        // Update current output if necessary
        if self.current_output == Some(output_id) {
            self.current_output = self.outputs.keys().find(|&&id| id != output_id).copied();
            if let Some(current_output) = self.current_output {
                self.set_default_layer_shell_output(current_output);
            }
        }

        self.outputs.remove(&output_id);
    }

    /// Create a new seat and add to context
    pub fn create_seat(&mut self, rwm_seat: RiverSeatV1) -> Rc<RefCell<Seat>> {
        let id = self.next_seat_id;
        self.next_seat_id += 1;

        let mut seat = Seat::new(id);
        seat.rwm_seat = Some(rwm_seat);

        // Add default bindings
        self.setup_seat_bindings(&mut seat);

        let seat = Rc::new(RefCell::new(seat));
        self.seats.insert(id, seat.clone());

        // Set as current seat if first
        if self.current_seat.is_none() {
            self.current_seat = Some(id);
        }

        seat
    }

    /// Set up bindings for a seat
    fn setup_seat_bindings(&self, seat: &mut Seat) {
        use crate::binding::action::{default_pointer_bindings, default_xkb_bindings};

        // Add XKB bindings
        for (mode, keysym, modifiers, action, event) in
            default_xkb_bindings(self.config.main_modifier)
        {
            seat.add_xkb_binding(
                XkbBinding::new(mode, keysym, modifiers, action).with_event(event),
            );
        }

        // Add pointer bindings
        for (mode, button, modifiers, action) in default_pointer_bindings(self.config.main_modifier)
        {
            seat.add_pointer_binding(PointerBinding::new(mode, button, modifiers, action));
        }

        seat.initialize_bindings();
    }

    /// Remove a seat from context
    pub fn destroy_seat(&mut self, seat_id: SeatId) {
        // Update current seat if necessary
        if self.current_seat == Some(seat_id) {
            self.current_seat = self.seats.keys().find(|&&id| id != seat_id).copied();
        }

        self.seats.remove(&seat_id);
    }

    /// Apply rules to a newly created window
    pub fn apply_rules_to_window(&self, window: &mut Window) {
        let applied = rule::apply_rules(
            &self.config.rules,
            window.app_id.as_deref(),
            window.title.as_deref(),
            window.decoration_hint,
            window.parent.is_some(),
        );

        // hint 0 = only_supports_csd, hint 1 = prefers_csd
        // Respect CSD preference for both cases
        let prefers_csd = matches!(window.decoration_hint, Some(0) | Some(1));

        let decoration = if applied.force_ssd {
            WindowDecoration::Ssd
        } else if prefers_csd {
            WindowDecoration::Csd
        } else {
            WindowDecoration::Ssd
        };
        window.decoration = Some(decoration);
        window.set_swallow_top(applied.swallow_top.unwrap_or(0));
    }

    /// Focus a window
    pub fn focus(&mut self, window_id: WindowId) {
        // Move to front of focus stack
        self.focus_stack.retain(|&id| id != window_id);
        self.focus_stack.insert(0, window_id);
        self.focused_window = Some(window_id);

        // Actually focus the window via seat
        if let (Some(window), Some(seat_id)) = (self.windows.get(&window_id), self.current_seat) {
            if let Some(seat) = self.seats.get(&seat_id) {
                seat.borrow().focus_window(&window.borrow());
            }
        }

        self.update_window_output_from_position(window_id);
        if let Some(output_id) = self.output_for_window_id(window_id) {
            self.set_window_output(window_id, output_id);
        }
    }

    fn focus_preview(&mut self, window_id: WindowId) {
        self.focused_window = Some(window_id);

        if let (Some(window), Some(seat_id)) = (self.windows.get(&window_id), self.current_seat) {
            if let Some(seat) = self.seats.get(&seat_id) {
                seat.borrow().focus_window(&window.borrow());
            }
        }
    }

    /// Focus the next/previous window
    pub fn focus_iter(&mut self, direction: Direction) {
        let current_output = match self.current_output.and_then(|id| self.outputs.get(&id)) {
            Some(o) => o.clone(),
            None => return,
        };
        let output = current_output.borrow();

        // Get visible windows on current output
        let visible: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| {
                let w = w.borrow();
                w.is_visible_on(&output)
            })
            .map(|(&id, _)| id)
            .collect();

        if visible.is_empty() {
            return;
        }

        // Find current focused index
        let current_idx = self
            .focused_window
            .and_then(|id| visible.iter().position(|&wid| wid == id))
            .unwrap_or(0);

        // Calculate next index
        let next_idx = match direction {
            Direction::Forward => (current_idx + 1) % visible.len(),
            Direction::Reverse => {
                if current_idx == 0 {
                    visible.len() - 1
                } else {
                    current_idx - 1
                }
            }
        };

        self.focus(visible[next_idx]);
    }

    /// Arrange windows - stacking WM: do nothing, windows keep their positions
    pub fn arrange_output(&mut self, _output_id: OutputId) {
        // Stacking WM doesn't auto-arrange windows
        // New window positioning is handled in handle_window_event for Init
    }

    /// Execute an action
    pub fn execute_action(&mut self, action: Action, seat_id: SeatId) {
        match action {
            Action::Quit => {
                self.running = false;
            }
            Action::Close => {
                if let Some(window_id) = self.focused_window {
                    if let Some(window) = self.windows.get(&window_id) {
                        window.borrow().close();
                    }
                }
            }
            Action::Spawn { argv } => {
                self.spawn(&argv);
            }
            Action::SpawnShell { cmd } => {
                self.spawn_shell(&cmd);
            }
            Action::SpawnLauncher => {
                self.spawn(&self.config.launcher_cmd);
            }
            Action::FocusIter { direction } => {
                self.focus_iter(direction);
            }
            Action::FocusOutputIter { direction } => {
                self.focus_output_iter(direction);
            }
            Action::SendToOutput { direction } => {
                self.send_to_output(direction);
            }
            Action::PointerMove => {
                self.start_pointer_move(seat_id);
            }
            Action::PointerResize => {
                self.start_pointer_resize(seat_id);
            }
            Action::SwitchMode { mode } => {
                if let Some(seat) = self.seats.get(&seat_id) {
                    seat.borrow_mut().switch_mode(mode);
                }
            }
            Action::ToggleFullscreen { in_window } => {
                self.toggle_fullscreen(in_window);
            }
            Action::HideFocused => {
                if let Some(window_id) = self.focused_window {
                    self.hide_window(window_id);
                }
            }
            Action::SmartHideFocused => {
                let Some(window_id) = self.focused_window else {
                    return;
                };
                let Some(window) = self.windows.get(&window_id) else {
                    return;
                };
                let (is_fullscreen, is_maximized) = {
                    let w = window.borrow();
                    let fullscreen = !matches!(w.fullscreen, super::window::FullscreenState::None);
                    (fullscreen, w.maximized)
                };
                if is_fullscreen {
                    if let Some(window) = self.windows.get(&window_id) {
                        let mut w = window.borrow_mut();
                        w.exit_fullscreen();
                        w.pending_unfullscreen_restore = true;
                        if let Some(ref rwm) = self.rwm {
                            rwm.manage_dirty();
                        }
                    }
                    if let Some(output_id) = self.current_output {
                        self.arrange_output(output_id);
                    }
                } else if is_maximized {
                    self.unmaximize_window(window_id);
                } else {
                    self.hide_window(window_id);
                }
            }
            Action::SmartSnapHalf { side } => {
                if let Some(window_id) = self.focused_window {
                    self.smart_snap_half(window_id, side);
                }
            }
            Action::MaximizeFocused => {
                if let Some(window_id) = self.focused_window {
                    self.maximize_window(window_id);
                }
            }
            Action::ActivateMenuHovered => {
                if self.window_menu_mode == Some(WindowMenuMode::Pointer)
                    && self
                        .window_menu
                        .as_ref()
                        .and_then(|menu| menu.hovered)
                        .is_some()
                {
                    self.activate_menu_hovered();
                }
            }
            Action::WindowMenuCycle | Action::WindowMenuCycleApp => {
                if self.window_menu_mode == Some(WindowMenuMode::AltTab) {
                    if let Some(menu) = self.window_menu.as_mut() {
                        menu.select_next();
                    }
                }
            }
            Action::WindowMenuCommit => {
                if self.window_menu_mode == Some(WindowMenuMode::AltTab) {
                    if self
                        .window_menu
                        .as_ref()
                        .and_then(|menu| menu.hovered)
                        .is_some()
                    {
                        self.activate_menu_hovered();
                    } else {
                        self.close_window_menu();
                    }
                }
            }
            Action::ClearFocus => {
                self.clear_keyboard_focus();
            }
            Action::RestoreFocus => {
                self.restore_focus_from_stack();
            }
            Action::CustomFn { func, ref arg } => {
                let state = self.get_state();
                func(&state, arg);
            }
        }
    }

    /// Get current state for custom actions
    pub fn get_state(&self) -> crate::binding::State {
        crate::binding::State {}
    }

    /// Spawn a command
    pub fn spawn(&self, argv: &[String]) {
        if argv.is_empty() {
            return;
        }

        match unsafe { nix::unistd::fork() } {
            Ok(nix::unistd::ForkResult::Parent { .. }) => {
                // Parent continues
            }
            Ok(nix::unistd::ForkResult::Child) => {
                // Child process - create new session and exec
                let _ = nix::unistd::setsid();

                let mut cmd = Command::new(&argv[0]);
                cmd.args(&argv[1..]);
                cmd.stdin(Stdio::null());
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());

                if let Some(ref dir) = self.config.working_directory {
                    cmd.current_dir(dir);
                }

                for (key, value) in &self.config.env {
                    cmd.env(key, value);
                }

                // Make sure child inherits display environment
                // (WAYLAND_DISPLAY should already be in env)

                let err = cmd.exec();
                // exec() only returns on error
                eprintln!("Failed to exec {:?}: {:?}", argv, err);
                std::process::exit(1);
            }
            Err(_) => (),
        }
    }

    /// Spawn a shell command
    pub fn spawn_shell(&self, cmd: &str) {
        self.spawn(&["sh".to_string(), "-c".to_string(), cmd.to_string()]);
    }

    /// Focus next/previous output
    fn focus_output_iter(&mut self, direction: Direction) {
        let output_ids: Vec<OutputId> = self.outputs.keys().copied().collect();
        if output_ids.len() <= 1 {
            return;
        }

        let current_idx = self
            .current_output
            .and_then(|id| output_ids.iter().position(|&oid| oid == id))
            .unwrap_or(0);

        let next_idx = match direction {
            Direction::Forward => (current_idx + 1) % output_ids.len(),
            Direction::Reverse => {
                if current_idx == 0 {
                    output_ids.len() - 1
                } else {
                    current_idx - 1
                }
            }
        };

        self.current_output = Some(output_ids[next_idx]);
        self.set_default_layer_shell_output(output_ids[next_idx]);

        // Focus top window on new output
        if let Some(output) = self.outputs.get(&output_ids[next_idx]) {
            let output_ref = output.borrow();
            if let Some(window_id) = self.windows.iter().find_map(|(&id, w)| {
                if w.borrow().is_visible_on(&output_ref) {
                    Some(id)
                } else {
                    None
                }
            }) {
                drop(output_ref);
                self.focus(window_id);
            }
        }
    }

    /// Send focused window to next/previous output
    fn send_to_output(&mut self, direction: Direction) {
        let window_id = match self.focused_window {
            Some(id) => id,
            None => return,
        };

        let output_ids: Vec<OutputId> = self.outputs.keys().copied().collect();
        if output_ids.len() <= 1 {
            return;
        }

        let current_output_id = self
            .windows
            .get(&window_id)
            .and_then(|window| {
                window
                    .borrow()
                    .output
                    .as_ref()
                    .and_then(|output| output.upgrade())
            })
            .map(|output| output.borrow().id)
            .or(self.current_output)
            .unwrap_or(output_ids[0]);

        let current_idx = output_ids
            .iter()
            .position(|&oid| oid == current_output_id)
            .unwrap_or(0);

        let next_idx = match direction {
            Direction::Forward => (current_idx + 1) % output_ids.len(),
            Direction::Reverse => {
                if current_idx == 0 {
                    output_ids.len() - 1
                } else {
                    current_idx - 1
                }
            }
        };

        let current_output_id = output_ids[current_idx];
        let target_output_id = output_ids[next_idx];
        if target_output_id == current_output_id {
            return;
        }

        let (old_area, new_area, window_rect, snap_state, maximized, fullscreen, pre_snap) = {
            let Some(window) = self.windows.get(&window_id) else {
                return;
            };
            let w = window.borrow();
            let old_area = match self.outputs.get(&current_output_id) {
                Some(output) => output.borrow().usable_area(),
                None => return,
            };
            let new_area = match self.outputs.get(&target_output_id) {
                Some(output) => output.borrow().usable_area(),
                None => return,
            };
            (
                old_area,
                new_area,
                (w.x, w.y, w.width, w.height),
                w.snap_state,
                w.maximized,
                w.fullscreen.clone(),
                w.pre_snap,
            )
        };

        if matches!(fullscreen, super::window::FullscreenState::Output(_)) {
            self.set_window_output(window_id, target_output_id);
            if let (Some(window), Some(output)) = (
                self.windows.get(&window_id),
                self.outputs.get(&target_output_id),
            ) {
                let mut w = window.borrow_mut();
                if let Some(ref rwm_output) = output.borrow().rwm_output {
                    w.fullscreen_on(rwm_output);
                    w.fullscreen = super::window::FullscreenState::Output(Rc::downgrade(output));
                }
            }

            // Rearrange both outputs
            if let Some(current_id) = self.current_output {
                self.arrange_output(current_id);
            }
            self.arrange_output(target_output_id);
            return;
        }

        let (ox, oy, ow, oh) = old_area;
        let (nx, ny, nw, nh) = new_area;
        if ow <= 0 || oh <= 0 || nw <= 0 || nh <= 0 {
            self.set_window_output(window_id, target_output_id);
            return;
        }

        let map_rect = |x: i32, y: i32, w: i32, h: i32, resize: bool| {
            let rect_w = w.max(1);
            let rect_h = h.max(1);
            let ratio_w = rect_w as f32 / ow as f32;
            let ratio_h = rect_h as f32 / oh as f32;
            let mut new_w = if resize {
                (ratio_w * nw as f32).round() as i32
            } else {
                rect_w
            };
            let mut new_h = if resize {
                (ratio_h * nh as f32).round() as i32
            } else {
                rect_h
            };
            new_w = new_w.max(1).min(nw);
            new_h = new_h.max(1).min(nh);

            let rel_x = (x - ox) as f32 / ow as f32;
            let rel_y = (y - oy) as f32 / oh as f32;
            let mut new_x = nx + (rel_x * nw as f32).round() as i32;
            let mut new_y = ny + (rel_y * nh as f32).round() as i32;
            let max_x = nx + (nw - new_w).max(0);
            let max_y = ny + (nh - new_h).max(0);
            new_x = new_x.clamp(nx, max_x);
            new_y = new_y.clamp(ny, max_y);
            (new_x, new_y, new_w, new_h)
        };

        let (win_x, win_y, win_w, win_h) = window_rect;
        let force_resize = snap_state.is_some() || maximized;
        let needs_resize = force_resize || win_w.max(1) > nw || win_h.max(1) > nh;
        let (new_x, new_y, new_w, new_h) = map_rect(win_x, win_y, win_w, win_h, needs_resize);

        let mapped_pre_snap = if snap_state.is_some() {
            pre_snap.map(|saved| {
                let (px, py, pw, ph) = map_rect(saved.x, saved.y, saved.width, saved.height, true);
                super::window::SavedGeometry {
                    x: px,
                    y: py,
                    width: pw,
                    height: ph,
                }
            })
        } else {
            None
        };

        self.set_window_output(window_id, target_output_id);
        if let Some(window) = self.windows.get(&window_id) {
            let mut w = window.borrow_mut();
            if let Some(saved) = mapped_pre_snap {
                w.pre_snap = Some(saved);
            }
            w.set_position(new_x, new_y);
            if needs_resize {
                w.propose_dimensions(new_w, new_h);
            }
        }

        // Rearrange both outputs
        if let Some(current_id) = self.current_output {
            self.arrange_output(current_id);
        }
        self.arrange_output(target_output_id);
    }

    pub(crate) fn update_window_output_from_position(&mut self, window_id: WindowId) {
        let (current_output_id, rect_output_id) = {
            let Some(window) = self.windows.get(&window_id) else {
                return;
            };
            let w = window.borrow();
            if w.position_undefined || w.width <= 0 || w.height <= 0 {
                return;
            }
            let current_output_id = w
                .output
                .as_ref()
                .and_then(|output| output.upgrade())
                .map(|output| output.borrow().id);
            let rect_output_id = self.output_for_window_rect(&w);
            (current_output_id, rect_output_id)
        };

        let Some(new_output_id) = rect_output_id else {
            return;
        };

        if current_output_id == Some(new_output_id) {
            return;
        }

        self.set_window_output(window_id, new_output_id);
    }

    pub(crate) fn update_window_output_from_pointer(
        &mut self,
        seat_id: SeatId,
        pointer_x: i32,
        pointer_y: i32,
    ) {
        let Some(output_id) = self.output_at_point(pointer_x, pointer_y) else {
            return;
        };

        let Some(window_id) = self.focused_window else {
            return;
        };

        let Some(window) = self.windows.get(&window_id) else {
            return;
        };

        let w = window.borrow();
        let is_move = match &w.operator {
            super::window::Operator::Move { seat, .. } => {
                seat.as_ref()
                    .and_then(|op_seat| op_seat.upgrade().map(|seat| seat.borrow().id == seat_id))
                    == Some(true)
            }
            _ => false,
        };
        if !is_move {
            return;
        }

        let current_output_id = w
            .output
            .as_ref()
            .and_then(|output| output.upgrade())
            .map(|output| output.borrow().id);
        if current_output_id == Some(output_id) {
            return;
        }

        drop(w);
        self.set_window_output(window_id, output_id);
    }

    pub(crate) fn assign_output_for_window(&mut self, window_id: WindowId) {
        let output_id = self.preferred_output_for_window_id(window_id);
        if let Some(output_id) = output_id {
            self.set_window_output(window_id, output_id);
        }
    }

    fn output_for_window_rect(&self, window: &Window) -> Option<OutputId> {
        let wx1 = window.x;
        let wy1 = window.y;
        let wx2 = window.x + window.width;
        let wy2 = window.y + window.height;

        let mut best: Option<(OutputId, i64)> = None;
        for (id, output) in &self.outputs {
            let out = output.borrow();
            let ox1 = out.x;
            let oy1 = out.y;
            let ox2 = out.x + out.width;
            let oy2 = out.y + out.height;
            let overlap_w = (wx2.min(ox2) - wx1.max(ox1)).max(0) as i64;
            let overlap_h = (wy2.min(oy2) - wy1.max(oy1)).max(0) as i64;
            let area = overlap_w * overlap_h;
            if area <= 0 {
                continue;
            }
            if best.is_none_or(|(_, best_area)| area > best_area) {
                best = Some((*id, area));
            }
        }

        let best_id = best.map(|(id, _)| id);
        if best_id.is_some() {
            return best_id;
        }

        let center_x = window.x + (window.width / 2);
        let center_y = window.y + (window.height / 2);
        self.output_at_point(center_x, center_y)
    }

    fn output_at_point(&self, x: i32, y: i32) -> Option<OutputId> {
        self.outputs.iter().find_map(|(id, output)| {
            if output.borrow().contains_point(x, y) {
                Some(*id)
            } else {
                None
            }
        })
    }

    fn set_default_layer_shell_output(&self, output_id: OutputId) {
        let Some(output) = self.outputs.get(&output_id) else {
            return;
        };
        let output_ref = output.borrow();
        if let Some(ref layer_shell_output) = output_ref.layer_shell_output {
            layer_shell_output.set_default();
        }
    }

    fn set_window_output(&mut self, window_id: WindowId, output_id: OutputId) {
        if let (Some(window), Some(output)) =
            (self.windows.get(&window_id), self.outputs.get(&output_id))
        {
            window.borrow_mut().output = Some(Rc::downgrade(output));
            if self.focused_window == Some(window_id) {
                self.current_output = Some(output_id);
                self.last_focused_output = Some(output_id);
                self.set_default_layer_shell_output(output_id);
            }
        }
    }

    fn preferred_output_for_window_id(&self, window_id: WindowId) -> Option<OutputId> {
        let window = self.windows.get(&window_id)?;
        let w = window.borrow();
        if let Some(parent_id) = w.parent {
            if let Some(parent_output) = self.parent_output_if_visible(parent_id) {
                return Some(parent_output);
            }
        }
        self.current_output_for_new_window()
    }

    fn parent_output_if_visible(&self, parent_id: WindowId) -> Option<OutputId> {
        let parent = self.windows.get(&parent_id)?;
        let p = parent.borrow();
        if p.hidden {
            return None;
        }
        self.output_for_window_rect(&p)
    }

    fn current_output_for_new_window(&self) -> Option<OutputId> {
        let focused_output = self
            .focused_window
            .and_then(|window_id| self.output_for_window_id(window_id));

        focused_output
            .or(self.last_focused_output)
            .or(self.current_output)
    }

    fn clear_keyboard_focus(&mut self) {
        self.focused_window = None;
        if let Some(seat_id) = self.current_seat {
            if let Some(seat) = self.seats.get(&seat_id) {
                seat.borrow().clear_focus();
            }
        }
    }

    fn restore_focus_from_stack(&mut self) {
        if self.focused_window.is_some() {
            return;
        }

        let preferred_output = self.current_output.or(self.last_focused_output);
        let mut candidate = None;

        if let Some(output_id) = preferred_output {
            for &window_id in &self.focus_stack {
                let Some(window) = self.windows.get(&window_id) else {
                    continue;
                };
                let w = window.borrow();
                if w.hidden {
                    continue;
                }
                let matches_output = w
                    .output
                    .as_ref()
                    .and_then(|output| output.upgrade())
                    .map(|output| output.borrow().id)
                    == Some(output_id);
                if matches_output {
                    candidate = Some(window_id);
                    break;
                }
            }
        }

        if candidate.is_none() {
            for &window_id in &self.focus_stack {
                let Some(window) = self.windows.get(&window_id) else {
                    continue;
                };
                if !window.borrow().hidden {
                    candidate = Some(window_id);
                    break;
                }
            }
        }

        if let Some(window_id) = candidate {
            self.focus(window_id);
        } else {
            self.clear_keyboard_focus();
        }
    }

    fn output_for_window_id(&self, window_id: WindowId) -> Option<OutputId> {
        let window = self.windows.get(&window_id)?;
        let w = window.borrow();
        if w.position_undefined || w.width <= 0 || w.height <= 0 {
            return w
                .output
                .as_ref()
                .and_then(|output| output.upgrade())
                .map(|output| output.borrow().id);
        }
        self.output_for_window_rect(&w)
    }

    // Debug helpers removed.

    /// Start pointer move operation
    fn start_pointer_move(&mut self, seat_id: SeatId) {
        // First, focus the window under the pointer
        if let Some(seat) = self.seats.get(&seat_id) {
            let window_below = seat.borrow().window_below_pointer.clone();
            if let Some(weak) = window_below {
                if let Some(window) = weak.upgrade() {
                    let wid = window.borrow().id;
                    self.focus(wid);
                }
            }
        }

        // Now move the focused window
        if let Some(window_id) = self.focused_window {
            if let Some(window) = self.windows.get(&window_id) {
                if let Some(seat) = self.seats.get(&seat_id) {
                    let mut w = window.borrow_mut();
                    let (px, py) = {
                        let seat_ref = seat.borrow();
                        (seat_ref.pointer_x, seat_ref.pointer_y)
                    };
                    self.unmaximize_for_move(&mut w, px, py, true);
                    w.snap_state = None;
                    w.floating = true; // Make floating if not already
                    w.start_move(Rc::downgrade(seat));
                    seat.borrow().start_pointer_op();
                }
            }
        }
    }

    /// Start pointer resize operation
    fn start_pointer_resize(&mut self, seat_id: SeatId) {
        // First, focus the window under the pointer
        if let Some(seat) = self.seats.get(&seat_id) {
            let window_below = seat.borrow().window_below_pointer.clone();
            if let Some(weak) = window_below {
                if let Some(window) = weak.upgrade() {
                    let wid = window.borrow().id;
                    self.focus(wid);
                }
            }
        }

        // Now resize the focused window
        if let Some(window_id) = self.focused_window {
            if let Some(window) = self.windows.get(&window_id) {
                if let Some(seat) = self.seats.get(&seat_id) {
                    let edges = {
                        let mut w = window.borrow_mut();
                        w.clear_maximized_without_restore();
                        w.snap_state = None;
                        w.floating = true; // Make floating if not already

                        // Determine edges based on pointer position relative to window
                        let seat_ref = seat.borrow();
                        let px = seat_ref.pointer_x;
                        let py = seat_ref.pointer_y;
                        drop(seat_ref);

                        let edges = calculate_resize_edges(&w, px, py);
                        w.start_resize(Rc::downgrade(seat), edges);
                        edges
                    };
                    seat.borrow().start_pointer_op();
                    if edges != 0 {
                        self.update_cursor_for_seat(seat_id);
                    }
                }
            }
        }
    }

    /// Start move/resize based on pointer location within the window frame
    pub fn handle_window_interaction(&mut self, seat_id: SeatId, window_id: WindowId) {
        let seat = match self.seats.get(&seat_id) {
            Some(seat) => seat.clone(),
            None => return,
        };
        let window = match self.windows.get(&window_id) {
            Some(window) => window.clone(),
            None => return,
        };

        let (px, py) = {
            let seat_ref = seat.borrow();
            (seat_ref.pointer_x, seat_ref.pointer_y)
        };

        let (x, y, width, height, has_titlebar, swallow_top) = {
            let w = window.borrow();
            (
                w.x,
                w.y,
                w.width,
                w.height,
                w.decoration == Some(WindowDecoration::Ssd),
                w.swallow_top,
            )
        };

        let border_width = self.config.ui.border_width;
        let titlebar_height = super::titlebar::titlebar_height(&self.config.ui);
        let swallow_top = swallow_top.max(0);
        let frame_x = x - border_width;
        let frame_y = y - border_width - titlebar_height + swallow_top;
        let frame_width = width + border_width * 2;
        let frame_height = height + border_width * 2 + titlebar_height - swallow_top;
        let edges = calculate_resize_edges_near_border(
            frame_x,
            frame_y,
            frame_width,
            frame_height,
            border_width,
            px,
            py,
        );

        if edges != 0 {
            {
                let mut w = window.borrow_mut();
                w.clear_maximized_without_restore();
                w.floating = true;
                w.start_resize(Rc::downgrade(&seat), edges);
            }
            seat.borrow().start_pointer_op();
            self.update_cursor_for_seat(seat_id);
            return;
        }

        if has_titlebar {
            let titlebar_origin_x = x;
            let titlebar_origin_y = y - titlebar_height + swallow_top;
            let local_x = px - titlebar_origin_x;
            let local_y = py - titlebar_origin_y;

            if local_x >= 0 && local_x < width && local_y >= 0 && local_y < titlebar_height {
                let titlebar_height = super::titlebar::titlebar_height(&self.config.ui);
                let buttons = super::titlebar::button_rects(width, titlebar_height);

                if buttons.close.contains(local_x, local_y)
                    || buttons.hide.contains(local_x, local_y)
                    || buttons.maximize.contains(local_x, local_y)
                {
                    return;
                }

                let mut w = window.borrow_mut();
                self.unmaximize_for_move(&mut w, px, py, false);
                w.floating = true;
                w.start_move(Rc::downgrade(&seat));
                seat.borrow().start_pointer_op();
                return;
            }
        }

        if has_titlebar && point_in_titlebar(x, y + swallow_top, width, titlebar_height, px, py) {
            let mut w = window.borrow_mut();
            self.unmaximize_for_move(&mut w, px, py, false);
            w.floating = true;
            w.start_move(Rc::downgrade(&seat));
            seat.borrow().start_pointer_op();
        }
    }

    pub(crate) fn hide_window(&mut self, window_id: WindowId) {
        let was_focused = self.focused_window == Some(window_id);
        if let Some(window) = self.windows.get(&window_id) {
            window.borrow_mut().hide();
        }
        if !was_focused {
            return;
        }

        let output = self
            .current_output
            .and_then(|id| self.outputs.get(&id).cloned())
            .or_else(|| {
                self.windows
                    .get(&window_id)
                    .and_then(|w| w.borrow().output.as_ref().and_then(|o| o.upgrade()))
            });

        let Some(output) = output else {
            self.focused_window = None;
            if let Some(seat_id) = self.current_seat {
                if let Some(seat) = self.seats.get(&seat_id) {
                    seat.borrow().clear_focus();
                }
            }
            return;
        };

        let output_ref = output.borrow();
        let visible: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.borrow().is_visible_on(&output_ref))
            .map(|(&id, _)| id)
            .collect();

        if visible.is_empty() {
            self.focused_window = None;
            if let Some(seat_id) = self.current_seat {
                if let Some(seat) = self.seats.get(&seat_id) {
                    seat.borrow().clear_focus();
                }
            }
            return;
        }

        if let Some(next_id) = self
            .focus_stack
            .iter()
            .copied()
            .find(|id| visible.contains(id))
        {
            self.focus(next_id);
        } else {
            self.focus(visible[0]);
        }
    }

    pub(crate) fn maximize_window(&mut self, window_id: WindowId) {
        self.update_window_output_from_position(window_id);

        let output_id = self
            .windows
            .get(&window_id)
            .and_then(|window| {
                let w = window.borrow();
                self.output_for_window_rect(&w)
            })
            .or(self.current_output);

        let output = output_id.and_then(|oid| self.outputs.get(&oid).cloned());

        let Some(output) = output else {
            return;
        };

        let (ox, oy, ow, oh) = {
            let out = output.borrow();
            let (ox, oy, ow, oh) = out.usable_area();
            (ox, oy, ow, oh)
        };

        // Only account for borders/titlebar if window uses SSD
        let has_ssd = self
            .windows
            .get(&window_id)
            .map(|w| w.borrow().decoration == Some(WindowDecoration::Ssd))
            .unwrap_or(false);
        let (border_width, titlebar_height) = if has_ssd {
            (
                self.config.ui.border_width,
                super::titlebar::titlebar_height(&self.config.ui),
            )
        } else {
            (0, 0)
        };

        let swallow_top = self
            .windows
            .get(&window_id)
            .map(|w| w.borrow().swallow_top)
            .unwrap_or(0)
            .max(0);
        let content_w = (ow - border_width * 2).max(1);
        let content_h = (oh - border_width * 2 - titlebar_height + swallow_top).max(1);
        let content_x = ox + border_width;
        let content_y = oy + border_width + titlebar_height - swallow_top;
        if let Some(window) = self.windows.get(&window_id) {
            let mut w = window.borrow_mut();
            if !w.maximized && w.pre_snap.is_none() {
                w.pre_snap = Some(super::window::SavedGeometry {
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                });
            }
            w.snap_state = Some(super::window::SnapState::Maximized);
            w.floating = true;
            w.set_position(content_x, content_y);
            w.propose_dimensions(content_w, content_h);
            w.maximized = true;
            w.inform_maximized();
        }
    }

    fn unmaximize_for_move(&self, w: &mut Window, pointer_x: i32, pointer_y: i32, adjust_y: bool) {
        let was_maximized = w.maximized;
        let was_snapped = w.snap_state.is_some();
        if !was_maximized && !was_snapped {
            return;
        }

        let saved = w.pre_snap.take();
        w.maximized = false;
        w.snap_state = None;

        if let Some(saved) = saved {
            let swallow_top = w.swallow_top.max(0);
            let cur_w = w.width.max(1);
            let cur_h = w.height.max(1);
            let rel_x = (pointer_x - w.x) as f32 / cur_w as f32;
            let rel_y = (pointer_y - (w.y - swallow_top)) as f32 / cur_h as f32;
            w.propose_dimensions(saved.width, saved.height);
            let new_x = pointer_x - (rel_x * saved.width as f32).round() as i32;
            let new_y = if adjust_y {
                pointer_y - (rel_y * saved.height as f32).round() as i32
            } else {
                w.y
            };
            w.set_position(new_x, new_y);
        }

        if was_maximized {
            w.inform_unmaximized();
        }
    }

    pub(crate) fn unmaximize_window(&mut self, window_id: WindowId) {
        if let Some(window) = self.windows.get(&window_id) {
            let mut w = window.borrow_mut();
            w.maximized = false;
            if let Some(saved) = w.pre_snap.take() {
                w.set_position(saved.x, saved.y);
                w.propose_dimensions(saved.width, saved.height);
            }
            w.snap_state = None;
            w.inform_unmaximized();
        }
    }

    fn smart_snap_half(&mut self, window_id: WindowId, side: crate::binding::action::SnapSide) {
        use super::window::{FullscreenState, SnapState};

        self.update_window_output_from_position(window_id);

        let output_id = self
            .windows
            .get(&window_id)
            .and_then(|window| {
                let w = window.borrow();
                self.output_for_window_rect(&w)
            })
            .or(self.current_output);

        let output = output_id.and_then(|oid| self.outputs.get(&oid).cloned());

        let Some(output) = output else {
            return;
        };

        let (ox, oy, ow, oh) = {
            let out = output.borrow();
            let (ox, oy, ow, oh) = out.usable_area();
            (ox, oy, ow, oh)
        };

        // Only account for borders/titlebar if window uses SSD
        let has_ssd = self
            .windows
            .get(&window_id)
            .map(|w| w.borrow().decoration == Some(WindowDecoration::Ssd))
            .unwrap_or(false);
        let (border_width, titlebar_height) = if has_ssd {
            (
                self.config.ui.border_width,
                super::titlebar::titlebar_height(&self.config.ui),
            )
        } else {
            (0, 0)
        };

        let swallow_top = self
            .windows
            .get(&window_id)
            .map(|w| w.borrow().swallow_top)
            .unwrap_or(0)
            .max(0);

        let (side_width, side_x) = {
            let left_width = ow / 2;
            match side {
                crate::binding::action::SnapSide::Left => (left_width, ox),
                crate::binding::action::SnapSide::Right => (ow - left_width, ox + left_width),
            }
        };

        let content_w = (side_width - border_width * 2).max(1);
        let content_h = (oh - border_width * 2 - titlebar_height + swallow_top).max(1);
        let content_x = side_x + border_width;
        let content_y = oy + border_width + titlebar_height - swallow_top;
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };

        let mut w = window.borrow_mut();
        let target_snap = match side {
            crate::binding::action::SnapSide::Left => SnapState::Left,
            crate::binding::action::SnapSide::Right => SnapState::Right,
        };

        if w.snap_state == Some(target_snap) {
            return;
        }

        if w.snap_state == Some(target_snap.opposite()) {
            if let Some(saved) = w.pre_snap.take() {
                w.set_position(saved.x, saved.y);
                w.propose_dimensions(saved.width, saved.height);
            }
            w.snap_state = None;
            return;
        }

        if w.snap_state == Some(SnapState::Maximized) {
            if let Some(saved) = w.pre_snap {
                w.set_position(saved.x, saved.y);
                w.propose_dimensions(saved.width, saved.height);
            }
            w.maximized = false;
            w.inform_unmaximized();
            w.snap_state = None;
        }

        if w.pre_snap.is_none() {
            let saved = if !matches!(w.fullscreen, FullscreenState::None) {
                w.pre_fullscreen
            } else {
                None
            }
            .unwrap_or(super::window::SavedGeometry {
                x: w.x,
                y: w.y,
                width: w.width,
                height: w.height,
            });
            w.pre_snap = Some(saved);
        }

        if !matches!(w.fullscreen, FullscreenState::None) {
            w.exit_fullscreen();
            w.pending_unfullscreen_restore = false;
            if let Some(ref rwm) = self.rwm {
                rwm.manage_dirty();
            }
        }

        w.clear_maximized_without_restore();
        w.floating = true;
        w.set_position(content_x, content_y);
        w.propose_dimensions(content_w, content_h);
        w.snap_state = Some(target_snap);
    }

    /// Toggle fullscreen for focused window
    fn toggle_fullscreen(&mut self, _in_window: bool) {
        let window_id = match self.focused_window {
            Some(id) => id,
            None => return,
        };

        if let Some(window) = self.windows.get(&window_id) {
            let mut w = window.borrow_mut();
            match &w.fullscreen {
                super::window::FullscreenState::None => {
                    // Enter fullscreen
                    if w.pre_fullscreen.is_none() {
                        w.pre_fullscreen = Some(super::window::SavedGeometry {
                            x: w.x,
                            y: w.y,
                            width: w.width,
                            height: w.height,
                        });
                    }
                    w.pending_unfullscreen_restore = false;
                    if let Some(output_id) = self.current_output {
                        if let Some(output) = self.outputs.get(&output_id) {
                            let output_ref = output.borrow();
                            if let Some(ref rwm_output) = output_ref.rwm_output {
                                w.fullscreen_on(rwm_output);
                                w.fullscreen =
                                    super::window::FullscreenState::Output(Rc::downgrade(output));
                            }
                        }
                    }
                }
                _ => {
                    // Exit fullscreen
                    w.exit_fullscreen();
                    w.pending_unfullscreen_restore = true;
                    if let Some(ref rwm) = self.rwm {
                        rwm.manage_dirty();
                    }
                }
            }
        }

        if let Some(output_id) = self.current_output {
            self.arrange_output(output_id);
        }
    }

    /// Handle manage_start event - process pending window events and arrange windows
    pub fn handle_manage_start(&mut self) {
        // Apply deferred fullscreen restores from the previous manage sequence.
        let restore_ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter_map(|(&id, w)| {
                let w = w.borrow();
                if w.pending_unfullscreen_restore
                    && matches!(w.fullscreen, super::window::FullscreenState::None)
                    && w.pre_fullscreen.is_some()
                {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();
        for window_id in restore_ids {
            if let Some(window) = self.windows.get(&window_id) {
                let mut w = window.borrow_mut();
                if let Some(saved) = w.pre_fullscreen {
                    w.set_position(saved.x, saved.y);
                    w.propose_dimensions(saved.width, saved.height);
                }
                w.pending_unfullscreen_restore = false;
            }
        }

        // Process seat actions
        let seat_ids: Vec<SeatId> = self.seats.keys().copied().collect();
        for seat_id in seat_ids {
            let actions = if let Some(seat) = self.seats.get(&seat_id) {
                seat.borrow_mut().drain_actions()
            } else {
                continue;
            };

            for action in actions {
                self.execute_action(action, seat_id);
            }
        }

        // Process window events
        let window_ids: Vec<WindowId> = self.windows.keys().copied().collect();
        for window_id in window_ids {
            let events = if let Some(window) = self.windows.get(&window_id) {
                window.borrow_mut().handle_events()
            } else {
                continue;
            };

            for event in events {
                self.handle_window_event(window_id, event);
            }
        }

        // Keep window output association in sync with window positions.
        let window_ids: Vec<WindowId> = self.windows.keys().copied().collect();
        for window_id in window_ids {
            let output_id = {
                let Some(window) = self.windows.get(&window_id) else {
                    continue;
                };
                let w = window.borrow();
                if w.position_undefined || w.width <= 0 || w.height <= 0 {
                    continue;
                }
                self.output_for_window_rect(&w)
            };

            let Some(output_id) = output_id else {
                continue;
            };

            let current_output_id = self
                .windows
                .get(&window_id)
                .and_then(|window| {
                    window
                        .borrow()
                        .output
                        .as_ref()
                        .and_then(|output| output.upgrade())
                })
                .map(|output| output.borrow().id);

            if current_output_id == Some(output_id) {
                continue;
            }

            self.set_window_output(window_id, output_id);
        }

        // Arrange all outputs
        let output_ids: Vec<OutputId> = self.outputs.keys().copied().collect();
        for output_id in output_ids {
            self.arrange_output(output_id);
        }
    }

    /// Handle a window event
    fn handle_window_event(&mut self, window_id: WindowId, event: super::window::WindowEvent) {
        use super::window::WindowEvent;

        match event {
            WindowEvent::Init => {
                // Apply rules
                if let Some(window) = self.windows.get(&window_id) {
                    let mut w = window.borrow_mut();
                    self.apply_rules_to_window(&mut w);

                    // Apply decoration preference (if any)
                    if let Some(decoration) = w.decoration {
                        w.set_decoration(decoration);
                    }

                    // We must call propose_dimensions for windows to be displayed.
                    // The protocol says (0,0) means "let window decide" but that
                    // gives us the window's minimum size (often tiny).
                    let (default_width, default_height) = w
                        .output
                        .as_ref()
                        .and_then(|output| output.upgrade())
                        .map(|output| {
                            let (_, _, width, height) = output.borrow().usable_area();
                            if width > 0 && height > 0 {
                                (width / 2, height / 2)
                            } else {
                                (800, 600)
                            }
                        })
                        .unwrap_or((800, 600));
                    w.propose_dimensions(default_width, default_height);
                }

                // Focus the new window
                self.focus(window_id);
            }
            WindowEvent::Close => {
                if let Some(window) = self.windows.get(&window_id) {
                    window.borrow().close();
                }
            }
            WindowEvent::Fullscreen(output) => {
                if let Some(window) = self.windows.get(&window_id) {
                    let mut w = window.borrow_mut();
                    // Handle fullscreen request
                    if w.pre_fullscreen.is_none() {
                        w.pre_fullscreen = Some(super::window::SavedGeometry {
                            x: w.x,
                            y: w.y,
                            width: w.width,
                            height: w.height,
                        });
                    }
                    w.pending_unfullscreen_restore = false;
                    if let Some(output) = output.and_then(|o| o.upgrade()) {
                        if let Some(ref rwm_output) = output.borrow().rwm_output {
                            w.fullscreen_on(rwm_output);
                            w.fullscreen =
                                super::window::FullscreenState::Output(Rc::downgrade(&output));
                        }
                    }
                }
            }
            WindowEvent::Unfullscreen => {
                if let Some(window) = self.windows.get(&window_id) {
                    let mut w = window.borrow_mut();
                    w.exit_fullscreen();
                    w.pending_unfullscreen_restore = true;
                    if let Some(ref rwm) = self.rwm {
                        rwm.manage_dirty();
                    }
                }
            }
            WindowEvent::Maximize => {
                self.maximize_window(window_id);
            }
            WindowEvent::Unmaximize => {
                self.unmaximize_window(window_id);
            }
            WindowEvent::Minimize => {
                self.hide_window(window_id);
            }
            WindowEvent::Move(seat) => {
                if let Some(seat) = seat.upgrade() {
                    self.start_pointer_move(seat.borrow().id);
                }
            }
            WindowEvent::Resize(seat, edges) => {
                if let Some(window) = self.windows.get(&window_id) {
                    if let Some(seat) = seat.upgrade() {
                        let mut w = window.borrow_mut();
                        w.clear_maximized_without_restore();
                        w.start_resize(Rc::downgrade(&seat), edges);
                        seat.borrow().start_pointer_op();
                    }
                }
            }
        }
    }

    /// Handle render_start event - position windows and set borders
    pub fn handle_render_start(&mut self) {
        self.apply_initial_positions();
        let unminimize_all = !self.startup_unminimize_done;
        if unminimize_all {
            for window in self.windows.values() {
                window.borrow_mut().show();
            }
            self.startup_unminimize_done = true;
        }

        // Process each window
        for (window_id, window) in &self.windows {
            let mut w = window.borrow_mut();

            // Check if window should be visible
            let mut visible = self
                .outputs
                .values()
                .any(|output| w.is_visible_on(&output.borrow()));
            if unminimize_all {
                visible = true;
            }

            let swallow_top = w.swallow_top;
            if swallow_top > 0 && w.width > 0 && w.height > 0 {
                let clip_w = w.width.max(1);
                let clip_h = (w.height - swallow_top).max(1);
                w.set_content_clip_box(0, swallow_top, clip_w, clip_h);
            } else {
                w.set_content_clip_box(0, 0, 0, 0);
            }

            if visible {
                w.show();

                // Disable compositor borders; custom decoration handles borders.
                let is_focused = self.focused_window == Some(*window_id);
                let edges = Edges::all();
                w.set_borders(edges, 0, 0, 0, 0, 0);

                // Raise focused window
                if is_focused {
                    w.place_top();
                }
            } else {
                w.hide();
            }
        }
    }

    fn apply_initial_positions(&mut self) {
        let mut output_windows: HashMap<OutputId, Vec<WindowId>> = HashMap::new();

        for (&window_id, window) in &self.windows {
            let w = window.borrow();
            if !w.position_undefined {
                continue;
            }

            let output_id = w
                .output
                .as_ref()
                .and_then(|output| output.upgrade())
                .map(|output| output.borrow().id)
                .or_else(|| self.preferred_output_for_window_id(window_id))
                .or(self.current_output);

            let output_id = match output_id {
                Some(output_id) => output_id,
                None => continue,
            };

            let output = match self.outputs.get(&output_id) {
                Some(output) => output.clone(),
                None => continue,
            };

            if !w.is_visible_on(&output.borrow()) {
                continue;
            }

            output_windows.entry(output_id).or_default().push(window_id);
        }

        for (output_id, mut window_ids) in output_windows {
            window_ids.sort_unstable();

            let output = match self.outputs.get(&output_id) {
                Some(output) => output.clone(),
                None => continue,
            };

            let (area_x, area_y, area_w, area_h) = output.borrow().usable_area();
            if area_w <= 0 || area_h <= 0 {
                continue;
            }

            let target_w = (area_w / 2).max(1);
            let target_h = (area_h / 2).max(1);
            let pad_x = 10 + self.config.ui.border_width;
            let pad_y = 10
                + self.config.ui.border_width
                + super::titlebar::titlebar_height(&self.config.ui);
            let start_x = area_x + pad_x;
            let start_y = area_y + pad_y;
            let mut end_x = area_x + area_w - target_w - pad_x;
            let mut end_y = area_y + area_h - target_h - pad_y;

            if end_x < start_x {
                end_x = start_x;
            }
            if end_y < start_y {
                end_y = start_y;
            }

            let denom = (window_ids.len().saturating_sub(1)) as i32;
            for (idx, window_id) in window_ids.iter().enumerate() {
                let x = if denom == 0 {
                    start_x
                } else {
                    start_x + (end_x - start_x) * idx as i32 / denom
                };
                let y = if denom == 0 {
                    start_y
                } else {
                    start_y + (end_y - start_y) * idx as i32 / denom
                };

                if let Some(window) = self.windows.get(window_id) {
                    window.borrow_mut().set_position(x, y);
                }
            }
        }
    }

    // Debug helpers removed.

    /// Finish manage sequence
    pub fn finish_manage(&self) {
        if let Some(ref rwm) = self.rwm {
            rwm.manage_finish();
            let needs_restore = self
                .windows
                .values()
                .any(|w| w.borrow().pending_unfullscreen_restore);
            if needs_restore {
                rwm.manage_dirty();
            }
        }
    }

    /// Finish render sequence
    pub fn finish_render(&self) {
        if let Some(ref rwm) = self.rwm {
            rwm.render_finish();
        }
    }

    /// Build menu items for windows (including hidden).
    pub fn collect_menu_items(&self, _output_id: OutputId) -> Vec<MenuItem> {
        let focused = self.focused_window;

        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for window_id in &self.focus_stack {
            if let Some(window) = self.windows.get(window_id) {
                items.push(menu_item_from_window(*window_id, focused, &window.borrow()));
                seen.insert(*window_id);
            }
        }

        for (&window_id, window) in &self.windows {
            if seen.contains(&window_id) {
                continue;
            }
            items.push(menu_item_from_window(window_id, focused, &window.borrow()));
        }

        items
    }

    /// Build menu items for windows sharing the given app id (including hidden).
    pub fn collect_menu_items_for_app(&self, _output_id: OutputId, app_id: &str) -> Vec<MenuItem> {
        let focused = self.focused_window;

        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for window_id in &self.focus_stack {
            if let Some(window) = self.windows.get(window_id) {
                let w = window.borrow();
                if w.app_id.as_deref() == Some(app_id) {
                    items.push(menu_item_from_window(*window_id, focused, &w));
                    seen.insert(*window_id);
                }
            }
        }

        for (&window_id, window) in &self.windows {
            if seen.contains(&window_id) {
                continue;
            }
            let w = window.borrow();
            if w.app_id.as_deref() == Some(app_id) {
                items.push(menu_item_from_window(window_id, focused, &w));
            }
        }

        items
    }

    /// Update menu hover based on surface-local pointer position.
    pub fn update_menu_hover(&mut self, x: i32, y: i32) -> bool {
        if let Some(menu) = self.window_menu.as_mut() {
            return menu.update_hover(x, y);
        }
        false
    }

    /// Activate the currently hovered menu item.
    pub fn activate_menu_hovered(&mut self) {
        let Some(menu) = self.window_menu.as_ref() else {
            return;
        };
        let Some(idx) = menu.hovered else {
            return;
        };
        let window_id = menu.items.get(idx).map(|item| item.window_id);
        self.clear_alt_tab_state();
        self.window_menu = None;
        self.window_menu_mode = None;
        self.window_menu_shield = None;

        let Some(window_id) = window_id else {
            return;
        };

        if let Some(window) = self.windows.get(&window_id) {
            {
                let mut w = window.borrow_mut();
                if w.hidden {
                    w.show();
                }
                w.place_top();
            }
            self.focus(window_id);
        }
    }

    /// Close the window menu if open.
    pub fn close_window_menu(&mut self) {
        if self.window_menu_mode == Some(WindowMenuMode::AltTab) {
            self.restore_alt_tab_state();
        } else {
            self.clear_alt_tab_state();
        }
        self.window_menu = None;
        self.window_menu_mode = None;
        self.window_menu_shield = None;
    }

    pub fn begin_alt_tab(&mut self) {
        if self.window_menu_alt_tab_stack.is_some() {
            return;
        }
        self.window_menu_alt_tab_stack = Some(self.focus_stack.clone());
        self.window_menu_alt_tab_focused = self.focused_window;
        self.window_menu_alt_tab_preview = None;
        self.window_menu_alt_tab_preview_was_hidden = false;
    }

    pub fn preview_alt_tab_window(&mut self, window_id: WindowId) {
        if self.window_menu_mode != Some(WindowMenuMode::AltTab) {
            return;
        }
        if self.window_menu_alt_tab_stack.is_none() {
            self.begin_alt_tab();
        }
        if self.window_menu_alt_tab_preview == Some(window_id) {
            return;
        }

        if let Some(prev_id) = self.window_menu_alt_tab_preview.take() {
            if self.window_menu_alt_tab_preview_was_hidden {
                if let Some(prev_window) = self.windows.get(&prev_id) {
                    prev_window.borrow_mut().hide();
                }
            }
        }
        self.window_menu_alt_tab_preview_was_hidden = false;

        if let Some(ref order) = self.window_menu_alt_tab_stack {
            self.restore_stack_order(order);
        }

        if let Some(window) = self.windows.get(&window_id) {
            let mut was_hidden = false;
            {
                let mut w = window.borrow_mut();
                if w.hidden {
                    was_hidden = true;
                    w.show();
                }
                w.place_top();
            }
            self.focus_preview(window_id);
            self.window_menu_alt_tab_preview = Some(window_id);
            self.window_menu_alt_tab_preview_was_hidden = was_hidden;
        }
    }

    fn restore_alt_tab_state(&mut self) {
        if let Some(prev_id) = self.window_menu_alt_tab_preview.take() {
            if self.window_menu_alt_tab_preview_was_hidden {
                if let Some(prev_window) = self.windows.get(&prev_id) {
                    prev_window.borrow_mut().hide();
                }
            }
        }
        self.window_menu_alt_tab_preview_was_hidden = false;

        if let Some(ref order) = self.window_menu_alt_tab_stack {
            self.restore_stack_order(order);
        }

        if let Some(window_id) = self.window_menu_alt_tab_focused {
            if self.windows.contains_key(&window_id) {
                self.focus_preview(window_id);
            }
        }

        self.clear_alt_tab_state();
    }

    fn restore_stack_order(&self, order: &[WindowId]) {
        for window_id in order.iter().rev() {
            if let Some(window) = self.windows.get(window_id) {
                window.borrow().place_top();
            }
        }
    }

    fn clear_alt_tab_state(&mut self) {
        self.window_menu_alt_tab_stack = None;
        self.window_menu_alt_tab_focused = None;
        self.window_menu_alt_tab_preview = None;
        self.window_menu_alt_tab_preview_was_hidden = false;
    }

    /// Update the cursor shape based on resize state or border hover.
    pub fn update_cursor_for_seat(&mut self, seat_id: SeatId) {
        if self.window_menu_mode == Some(WindowMenuMode::AltTab) {
            return;
        }
        let seat = match self.seats.get(&seat_id) {
            Some(seat) => seat.clone(),
            None => return,
        };

        let mut edges = 0u32;

        if let Some(window_id) = self.focused_window {
            if let Some(window) = self.windows.get(&window_id) {
                if let super::window::Operator::Resize {
                    edges: op_edges,
                    seat: Some(op_seat),
                    ..
                } = &window.borrow().operator
                {
                    if let Some(op_seat) = op_seat.upgrade() {
                        if op_seat.borrow().id == seat_id {
                            edges = *op_edges;
                        }
                    }
                }
            }
        }

        if edges == 0 {
            let window_below = seat.borrow().window_below_pointer.clone();
            if let Some(weak) = window_below {
                if let Some(window) = weak.upgrade() {
                    let w = window.borrow();
                    let (px, py) = {
                        let seat_ref = seat.borrow();
                        (seat_ref.pointer_x, seat_ref.pointer_y)
                    };
                    let border_width = self.config.ui.border_width;
                    let titlebar_height = super::titlebar::titlebar_height(&self.config.ui);
                    let frame_x = w.x - border_width;
                    let frame_y = w.y - border_width - titlebar_height;
                    let frame_width = w.width + border_width * 2;
                    let frame_height = w.height + border_width * 2 + titlebar_height;
                    edges = calculate_resize_edges_near_border(
                        frame_x,
                        frame_y,
                        frame_width,
                        frame_height,
                        border_width,
                        px,
                        py,
                    );
                }
            }
        }

        let shape = cursor_shape_for_edges(edges);
        seat.borrow_mut().set_cursor_shape(shape);
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Calculate resize edges based on pointer position
fn calculate_resize_edges(window: &Window, px: i32, py: i32) -> u32 {
    let cx = window.x + window.width / 2;
    let cy = window.y + window.height / 2;

    let mut edges = 0u32;

    if px < cx {
        edges |= 4; // Left
    } else {
        edges |= 8; // Right
    }

    if py < cy {
        edges |= 1; // Top
    } else {
        edges |= 2; // Bottom
    }

    edges
}

fn calculate_resize_edges_near_border(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    border: i32,
    px: i32,
    py: i32,
) -> u32 {
    if width <= 0 || height <= 0 || border <= 0 {
        return 0;
    }

    let left_edge = x;
    let right_edge = x + width;
    let top_edge = y;
    let bottom_edge = y + height;

    let within_vert = py >= top_edge - border && py <= bottom_edge + border;
    let within_horiz = px >= left_edge - border && px <= right_edge + border;

    let dist_left = (px - left_edge).abs();
    let dist_right = (px - right_edge).abs();
    let dist_top = (py - top_edge).abs();
    let dist_bottom = (py - bottom_edge).abs();

    let mut edges = 0u32;

    if within_vert {
        let mut horiz = 0u32;
        if dist_left <= border {
            horiz = 4;
        }
        if dist_right <= border && (horiz == 0 || dist_right < dist_left) {
            horiz = 8;
        }
        edges |= horiz;
    }

    if within_horiz {
        let mut vert = 0u32;
        if dist_top <= border {
            vert = 1;
        }
        if dist_bottom <= border && (vert == 0 || dist_bottom < dist_top) {
            vert = 2;
        }
        edges |= vert;
    }

    edges
}

fn cursor_shape_for_edges(edges: u32) -> Option<CursorShape> {
    let horiz = edges & (4 | 8);
    let vert = edges & (1 | 2);

    if horiz == 4 && vert == 1 {
        return Some(CursorShape::NwResize);
    }
    if horiz == 8 && vert == 1 {
        return Some(CursorShape::NeResize);
    }
    if horiz == 4 && vert == 2 {
        return Some(CursorShape::SwResize);
    }
    if horiz == 8 && vert == 2 {
        return Some(CursorShape::SeResize);
    }
    if horiz == 4 && vert == 0 {
        return Some(CursorShape::WResize);
    }
    if horiz == 8 && vert == 0 {
        return Some(CursorShape::EResize);
    }
    if horiz == 0 && vert == 1 {
        return Some(CursorShape::NResize);
    }
    if horiz == 0 && vert == 2 {
        return Some(CursorShape::SResize);
    }
    if horiz == 12 && vert == 0 {
        return Some(CursorShape::EwResize);
    }
    if horiz == 0 && vert == 3 {
        return Some(CursorShape::NsResize);
    }
    if horiz == 12 && vert == 3 {
        return Some(CursorShape::AllResize);
    }

    None
}

fn point_in_titlebar(x: i32, y: i32, width: i32, titlebar_height: i32, px: i32, py: i32) -> bool {
    if width <= 0 || titlebar_height <= 0 {
        return false;
    }

    px >= x && px <= x + width && py >= y - titlebar_height && py <= y
}

fn menu_item_from_window(
    window_id: WindowId,
    focused: Option<WindowId>,
    window: &Window,
) -> MenuItem {
    let title = window
        .title
        .as_ref()
        .filter(|t| !t.is_empty())
        .cloned()
        .or_else(|| window.app_id.clone())
        .unwrap_or_else(|| format!("Window {}", window_id));

    MenuItem {
        window_id,
        title,
        hidden: window.hidden,
        active: focused == Some(window_id),
    }
}
