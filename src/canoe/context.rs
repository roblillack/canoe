//! Core context - central state management

use crate::binding::{Action, Direction, PointerBinding, XkbBinding};
use crate::config::{load_config, Config, WindowDecoration};
use crate::protocol::river_window_management_v1::client::river_window_v1::{Capabilities, Edges};
use crate::protocol::*;
use crate::rule;
use resvg::tiny_skia;
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
    next_minimize_seq: u64,

    // Configuration
    pub config: Config,
    /// Whether the config file is ignored (`--no-config`). Remembered so that
    /// reloads (SIGHUP) keep honoring it instead of suddenly reading the file.
    skip_config: bool,
    /// Set by [`Context::reload_config`] to re-apply window rules on the next
    /// `manage_start`. Window-management requests (e.g. `set_capabilities`) are
    /// only valid inside a manage sequence, so the actual work is deferred there
    /// rather than run directly in the SIGHUP handler.
    pending_reapply_rules: bool,
    /// Set by [`Context::reload_config`] to rebuild and re-register each seat's
    /// XKB key bindings on the next `manage_start`. Like rule re-application this
    /// is deferred because `river_xkb_binding_v1.enable`/`disable` are only valid
    /// inside a manage sequence, and recreating the binding objects also needs
    /// the Wayland queue handle that only the dispatch layer has.
    pending_rebind_xkb: bool,

    // Runtime state
    pub running: bool,
    pub session_locked: bool,
    startup_unminimize_done: bool,

    /// Output with active icon keyboard focus
    pub icon_focus_output: Option<OutputId>,

    /// Cache for desktop icon images, keyed by (app_id, size_px).
    /// `None` value means "looked up but not found" — avoids repeated disk lookups.
    icon_cache: HashMap<(String, i32), Option<Rc<tiny_skia::Pixmap>>>,

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
    pub fn new(skip_config: bool) -> Self {
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
            next_minimize_seq: 0,

            config: load_config(skip_config),
            skip_config,
            pending_reapply_rules: false,
            pending_rebind_xkb: false,

            running: true,
            session_locked: false,
            startup_unminimize_done: false,

            icon_focus_output: None,

            icon_cache: HashMap::new(),

            window_menu: None,
            window_menu_mode: None,
            window_menu_shield: None,
            window_menu_alt_tab_stack: None,
            window_menu_alt_tab_focused: None,
            window_menu_alt_tab_preview: None,
            window_menu_alt_tab_preview_was_hidden: false,
        }
    }

    /// Re-read the configuration and apply it to the running session.
    ///
    /// Triggered by SIGHUP. The file is re-read honoring `--no-config`, then
    /// the parts that affect already-managed objects are refreshed: every
    /// titlebar is forced to repaint (the shadow and desktop caches already key
    /// on their inputs, so they repaint on their own) and the desktop is marked
    /// dirty. Window rules are re-applied via `set_capabilities` and friends,
    /// which are only valid inside a manage sequence, so that work is deferred
    /// to the next `manage_start` (see [`Context::pending_reapply_rules`]). The
    /// `manage_dirty` request kicks off that sequence; it is explicitly allowed
    /// outside a sequence for exactly this kind of out-of-band state change.
    ///
    /// Note: per-input-device settings (`repeat_rate`, `scroll_factor`) and
    /// keyboard/pointer bindings are bound when a seat/device first appears,
    /// so changes to those only take effect for devices connected afterwards.
    pub fn reload_config(&mut self) {
        self.config = load_config(self.skip_config);

        // Repainting only touches local buffers, so it is safe outside a manage
        // sequence. Force titlebars dirty since color changes are not otherwise
        // tracked as a repaint trigger.
        for window in self.windows.values() {
            if let Some(titlebar) = window.borrow_mut().titlebar.as_mut() {
                titlebar.dirty = true;
            }
        }
        self.mark_all_desktops_dirty();

        // Re-applying rules and re-registering key bindings both send requests
        // that may only be made during a manage sequence; defer both to the next
        // manage_start. Rebinding every reload also lets `main_modifier` changes
        // take effect, not just `[hotkeys]` edits.
        self.pending_reapply_rules = true;
        self.pending_rebind_xkb = true;
        if let Some(ref rwm) = self.rwm {
            rwm.manage_dirty();
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
            if let Some(&next_id) = self.focus_stack.first() {
                self.focus(next_id);
            } else {
                self.clear_keyboard_focus();
            }
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
        use crate::binding::action::default_pointer_bindings;

        self.populate_xkb_bindings(seat);

        // Add pointer bindings
        for (mode, button, modifiers, action) in default_pointer_bindings(self.config.main_modifier)
        {
            seat.add_pointer_binding(PointerBinding::new(mode, button, modifiers, action));
        }

        seat.initialize_bindings();
    }

    /// Append the built-in XKB bindings plus any user-configured `[hotkeys]` to a
    /// seat. Existing bindings are left in place, so callers rebuilding the list
    /// (see [`Context::rebuild_xkb_bindings`]) must clear it first.
    fn populate_xkb_bindings(&self, seat: &mut Seat) {
        use crate::binding::action::default_xkb_bindings;
        use crate::binding::BindingEvent;
        use crate::config::Mode;

        // User-configured hotkeys fire in the default mode on key press and take
        // precedence over any built-in binding sharing the same key + modifiers,
        // so collect their signatures and skip the colliding defaults rather than
        // registering two bindings for one chord.
        let user_chords: std::collections::HashSet<(u32, u32)> = self
            .config
            .hotkeys
            .iter()
            .map(|hotkey| (hotkey.keysym, hotkey.modifiers))
            .collect();

        for (mode, keysym, modifiers, action, event) in
            default_xkb_bindings(self.config.main_modifier)
        {
            if mode == Mode::Default
                && event == BindingEvent::Pressed
                && user_chords.contains(&(keysym, modifiers))
            {
                continue;
            }
            seat.add_xkb_binding(
                XkbBinding::new(mode, keysym, modifiers, action).with_event(event),
            );
        }

        // Add user-configured hotkeys (spawn a command on key press)
        for hotkey in &self.config.hotkeys {
            seat.add_xkb_binding(
                XkbBinding::new(
                    Mode::Default,
                    hotkey.keysym,
                    hotkey.modifiers,
                    Action::Spawn {
                        argv: hotkey.argv.clone(),
                    },
                )
                .with_event(BindingEvent::Pressed),
            );
        }
    }

    /// Rebuild a seat's in-memory XKB binding list from the current config and
    /// mark each enabled for the seat's current mode.
    ///
    /// This only refreshes the data; the caller must destroy the old protocol
    /// binding objects beforehand and create new ones afterward (that needs the
    /// Wayland queue handle, which lives in the dispatch layer). Pointer bindings
    /// are left untouched.
    pub fn rebuild_xkb_bindings(&self, seat: &mut Seat) {
        seat.xkb_bindings.clear();
        self.populate_xkb_bindings(seat);
        let mode = seat.mode;
        for (binding, _) in &mut seat.xkb_bindings {
            binding.enabled = binding.mode == mode;
        }
    }

    /// Take (and clear) the pending-XKB-rebind flag set by a config reload.
    pub fn take_pending_rebind_xkb(&mut self) -> bool {
        std::mem::take(&mut self.pending_rebind_xkb)
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

        // Decoration hints (river_window_v1.decoration_hint):
        //   0 only_supports_csd, 1 prefers_csd, 2 prefers_ssd, 3 no_preference
        //
        // We default to SSD and only fall back to CSD when the client
        // *explicitly* asked for client-side decorations (prefers_csd). A value
        // of "only_supports_csd" is what River reports for a client that never
        // created an xdg-decoration object at all -- i.e. it does not negotiate
        // decorations. IMHO the naming of this value is wrong, as quite a few
        // applications I tested assume SSD and get the 0 decoration_hint:
        // MATE Terminal, Gimp, Inkscape, ...
        let prefers_csd = matches!(window.decoration_hint, Some(1));

        let decoration = if applied.force_ssd {
            WindowDecoration::Ssd
        } else if prefers_csd {
            WindowDecoration::Csd
        } else {
            WindowDecoration::Ssd
        };
        window.decoration = Some(decoration);
        window.set_swallow_top(applied.swallow_top.unwrap_or(0));

        // Inform the client which window-management capabilities apply.
        // Regular windows: full set.
        // Fixed-size toplevels: no maximize/fullscreen, but minimize stays.
        // Parented dialogs: close only.
        let caps = if window.has_minimize_button() && window.has_maximize_button() {
            Capabilities::WindowMenu
                | Capabilities::Maximize
                | Capabilities::Fullscreen
                | Capabilities::Minimize
        } else if window.has_minimize_button() {
            Capabilities::WindowMenu | Capabilities::Minimize
        } else {
            Capabilities::WindowMenu
        };
        window.set_capabilities(caps);
    }

    /// Focus a window
    pub fn focus(&mut self, window_id: WindowId) {
        // Unfullscreen any fullscreen window when switching to a different window.
        // We cannot rely on self.focused_window here because focus_preview (used
        // during alt-tab) may have already changed it.
        let fullscreen_ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter_map(|(&id, w)| {
                if id != window_id
                    && !matches!(w.borrow().fullscreen, super::window::FullscreenState::None)
                {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();
        if !fullscreen_ids.is_empty() {
            for fs_id in &fullscreen_ids {
                if let Some(window) = self.windows.get(fs_id) {
                    let mut w = window.borrow_mut();
                    w.exit_fullscreen();
                    w.pending_unfullscreen_restore = true;
                }
            }
            if let Some(ref rwm) = self.rwm {
                rwm.manage_dirty();
            }
        }

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

    /// Mark a window as focused for rendering during an alt-tab preview without
    /// delivering real keyboard focus to it. Keyboard focus is cleared on the
    /// seat so that keys typed while the switcher menu is open are dropped by
    /// the compositor instead of being sent to the previewed window.
    fn focus_preview_visual(&mut self, window_id: WindowId) {
        self.focused_window = Some(window_id);

        if let Some(seat_id) = self.current_seat {
            if let Some(seat) = self.seats.get(&seat_id) {
                seat.borrow().clear_focus();
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
            Action::SpawnTerminal => {
                self.spawn(&self.config.terminal_cmd);
            }
            Action::SpawnLock => {
                self.spawn(&self.config.lock_cmd);
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
            Action::WindowMenuCancel => {
                if self.window_menu_mode == Some(WindowMenuMode::AltTab) {
                    self.close_window_menu();
                }
            }
            Action::ClearFocus => {
                self.clear_keyboard_focus();
            }
            Action::RestoreFocus => {
                self.restore_focus_from_stack();
            }
            Action::IconSelectNext => {
                self.icon_navigate(1, 0);
            }
            Action::IconSelectPrev => {
                self.icon_navigate(-1, 0);
            }
            Action::IconSelectUp => {
                self.icon_navigate(0, -1);
            }
            Action::IconSelectDown => {
                self.icon_navigate(0, 1);
            }
            Action::IconActivate => {
                self.icon_activate(seat_id);
            }
            Action::IconCancel => {
                self.exit_icon_focus(seat_id);
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
        // First, focus the window under the pointer; bail out if there is none
        // (e.g. clicking on the desktop) so we don't move an unrelated window.
        if let Some(seat) = self.seats.get(&seat_id) {
            let window_below = seat.borrow().window_below_pointer.clone();
            let Some(window) = window_below.and_then(|w| w.upgrade()) else {
                return;
            };
            let wid = window.borrow().id;
            self.focus(wid);
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
        // First, focus the window under the pointer; bail out if there is none
        // (e.g. clicking on the desktop) so we don't resize an unrelated window.
        if let Some(seat) = self.seats.get(&seat_id) {
            let window_below = seat.borrow().window_below_pointer.clone();
            let Some(window) = window_below.and_then(|w| w.upgrade()) else {
                return;
            };
            let wid = window.borrow().id;
            self.focus(wid);
        }

        // Now resize the focused window
        if let Some(window_id) = self.focused_window {
            if let Some(window) = self.windows.get(&window_id) {
                // Fixed-size toplevels and dialogs are not resizable; the
                // modifier+drag gesture must not start a resize on them (the
                // window stays focused from the step above).
                if !window.borrow().is_resizable() {
                    return;
                }
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

        let (x, y, width, height, has_titlebar, swallow_top, is_resizable, show_min, show_max) = {
            let w = window.borrow();
            (
                w.x,
                w.y,
                w.width,
                w.height,
                w.decoration == Some(WindowDecoration::Ssd),
                w.swallow_top,
                w.is_resizable(),
                w.has_minimize_button(),
                w.has_maximize_button(),
            )
        };

        let border_width = self.config.ui.border_width;
        let titlebar_height = super::titlebar::titlebar_height(&self.config.ui);
        let swallow_top = swallow_top.max(0);
        let frame_x = x - border_width;
        let frame_y = y - border_width - titlebar_height + swallow_top;
        let frame_width = width + border_width * 2;
        let frame_height = height + border_width * 2 + titlebar_height - swallow_top;
        let edges = if !is_resizable {
            0
        } else {
            calculate_resize_edges_near_border(
                frame_x,
                frame_y,
                frame_width,
                frame_height,
                border_width,
                px,
                py,
            )
        };

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
                let buttons =
                    super::titlebar::button_rects(width, titlebar_height, show_min, show_max);

                let on_button = buttons.close.contains(local_x, local_y)
                    || buttons
                        .hide
                        .map(|r| r.contains(local_x, local_y))
                        .unwrap_or(false)
                    || buttons
                        .maximize
                        .map(|r| r.contains(local_x, local_y))
                        .unwrap_or(false);
                if on_button {
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
            let mut w = window.borrow_mut();
            w.minimize_seq = self.next_minimize_seq;
            self.next_minimize_seq += 1;
            w.hide();
        }
        self.mark_all_desktops_dirty();
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
        // Re-apply window rules deferred from a config reload (SIGHUP). This is
        // the earliest point in a manage sequence, so the window-management
        // requests these issue (set_capabilities, swallow_top, ...) are valid.
        if self.pending_reapply_rules {
            self.pending_reapply_rules = false;
            let windows: Vec<_> = self.windows.values().cloned().collect();
            for window in &windows {
                let mut w = window.borrow_mut();
                self.apply_rules_to_window(&mut w);
            }
        }

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

                    // We must propose dimensions for the window to be displayed.
                    // Propose (0, 0), which tells River to let the window pick
                    // its own preferred size; River reports that size back via a
                    // dimensions event. This honors the size the application
                    // asked for instead of imposing one. Fixed-size windows
                    // (min == max) and dialogs naturally end up at their own
                    // requested size through the same path.
                    w.propose_preferred_dimensions();
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
                // The xdg-shell `set_fullscreen` request allows the client to
                // pass a NULL output, meaning "let the compositor pick". River
                // forwards that as `Option<Weak<Output>> = None`, so we must
                // resolve a fallback target here instead of silently dropping
                // the request.
                let target = output
                    .and_then(|o| o.upgrade())
                    .or_else(|| {
                        self.windows
                            .get(&window_id)
                            .and_then(|w| w.borrow().output.as_ref().and_then(|o| o.upgrade()))
                    })
                    .or_else(|| {
                        self.current_output
                            .and_then(|id| self.outputs.get(&id).cloned())
                    })
                    .or_else(|| self.outputs.values().next().cloned());

                if let Some(window) = self.windows.get(&window_id) {
                    let mut w = window.borrow_mut();
                    if w.pre_fullscreen.is_none() {
                        // A client that requests fullscreen immediately on
                        // creation has w.width/height still at 0 (the windowed
                        // configure cycle never completed). Saving (0, 0) here
                        // would restore the window to nothing on unfullscreen,
                        // so fall back to half the target output's usable area
                        // and centre it within that area instead of pinning to
                        // the top-left corner.
                        let usable = target.as_ref().map(|out| out.borrow().usable_area());
                        let (saved_w, saved_h) = if w.width > 0 && w.height > 0 {
                            (w.width, w.height)
                        } else {
                            match usable {
                                Some((_, _, ow, oh)) if ow > 0 && oh > 0 => (ow / 2, oh / 2),
                                _ => (800, 600),
                            }
                        };
                        let (saved_x, saved_y) = if !w.position_undefined {
                            (w.x, w.y)
                        } else if let Some((ux, uy, ow, oh)) = usable {
                            (
                                ux + ((ow - saved_w) / 2).max(0),
                                uy + ((oh - saved_h) / 2).max(0),
                            )
                        } else {
                            (w.x, w.y)
                        };
                        w.pre_fullscreen = Some(super::window::SavedGeometry {
                            x: saved_x,
                            y: saved_y,
                            width: saved_w,
                            height: saved_h,
                        });
                    }
                    w.pending_unfullscreen_restore = false;
                    if let Some(output) = target {
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
                        if !w.is_resizable() {
                            return;
                        }
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
        for window in self.windows.values() {
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
                let edges = Edges::all();
                w.set_borders(edges, 0, 0, 0, 0, 0);
            } else {
                w.hide();
            }
        }

        // Raise the focused window's root ancestor to the top. For a focused
        // dialog this lifts the whole parent chain so the chains-above-parents
        // pass below ends up stacking the focused child at the very top.
        if let Some(focused_id) = self.focused_window {
            let root_id = self.root_ancestor(focused_id);
            if let Some(root) = self.windows.get(&root_id) {
                let root = root.borrow();
                if !root.hidden {
                    root.place_top();
                }
            }
        }

        // Dialogs render directly above their parent (river-window-management-v1
        // recommends this for parented windows). Process by parent-chain depth
        // so nested dialogs end up stacked above their immediate parent without
        // disturbing the order established by deeper iterations.
        let mut chains: Vec<(WindowId, WindowId, usize)> = self
            .windows
            .iter()
            .filter_map(|(&id, w)| {
                w.borrow()
                    .parent
                    .map(|p| (id, p, self.parent_chain_depth(id)))
            })
            .collect();
        chains.sort_by_key(|&(_, _, depth)| depth);
        for (child_id, parent_id, _) in chains {
            let (Some(child), Some(parent)) =
                (self.windows.get(&child_id), self.windows.get(&parent_id))
            else {
                continue;
            };
            let c = child.borrow();
            if c.hidden {
                continue;
            }
            c.place_above(&parent.borrow());
        }
    }

    /// Number of ancestors a window has via its parent chain (0 if no parent).
    /// Cycles are not possible per the river protocol but we cap iteration
    /// defensively at the total window count.
    fn parent_chain_depth(&self, window_id: WindowId) -> usize {
        let mut depth = 0usize;
        let mut current = window_id;
        let limit = self.windows.len();
        while depth <= limit {
            let Some(window) = self.windows.get(&current) else {
                break;
            };
            let Some(parent) = window.borrow().parent else {
                break;
            };
            depth += 1;
            current = parent;
        }
        depth
    }

    /// The topmost ancestor reachable via the parent chain, or the window
    /// itself if it has no (known) parent.
    fn root_ancestor(&self, window_id: WindowId) -> WindowId {
        let mut current = window_id;
        let limit = self.windows.len();
        for _ in 0..=limit {
            let parent = self
                .windows
                .get(&current)
                .and_then(|w| w.borrow().parent)
                .filter(|p| self.windows.contains_key(p));
            match parent {
                Some(p) => current = p,
                None => break,
            }
        }
        current
    }

    fn apply_initial_positions(&mut self) {
        // Dialogs (windows with a parent) get centred over their parent
        // rather than being cascaded along with regular toplevels.
        let titlebar_h = super::titlebar::titlebar_height(&self.config.ui);
        let border_width = self.config.ui.border_width;
        let dialog_positions: Vec<(WindowId, i32, i32)> = self
            .windows
            .iter()
            .filter_map(|(&id, window)| {
                let w = window.borrow();
                if !w.position_undefined || w.width <= 0 || w.height <= 0 {
                    return None;
                }
                let parent = self.windows.get(&w.parent?)?.borrow();
                if parent.position_undefined || parent.width <= 0 || parent.height <= 0 {
                    return None;
                }
                let mut cx = parent.x + (parent.width - w.width) / 2;
                let mut cy = parent.y + (parent.height - w.height) / 2;
                if let Some(output) = w
                    .output
                    .as_ref()
                    .and_then(|o| o.upgrade())
                    .or_else(|| parent.output.as_ref().and_then(|o| o.upgrade()))
                {
                    let (ax, ay, aw, ah) = output.borrow().usable_area();
                    if aw > 0 && ah > 0 {
                        let min_x = ax + border_width;
                        let min_y = ay + border_width + titlebar_h;
                        let max_x = (ax + aw - border_width - w.width).max(min_x);
                        let max_y = (ay + ah - border_width - w.height).max(min_y);
                        cx = cx.clamp(min_x, max_x);
                        cy = cy.clamp(min_y, max_y);
                    }
                }
                Some((id, cx, cy))
            })
            .collect();
        for (id, x, y) in dialog_positions {
            if let Some(window) = self.windows.get(&id) {
                window.borrow_mut().set_position(x, y);
            }
        }

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

    /// Mark all desktop surfaces as needing re-render.
    pub fn mark_all_desktops_dirty(&self) {
        for output in self.outputs.values() {
            if let Some(desktop) = output.borrow_mut().desktop_surface.as_mut() {
                desktop.dirty = true;
            }
        }
    }

    /// Look up a cached desktop icon image, loading from disk on first access.
    fn get_icon(&mut self, app_id: &str, size_px: i32) -> Option<Rc<tiny_skia::Pixmap>> {
        let key = (app_id.to_string(), size_px);
        self.icon_cache
            .entry(key)
            .or_insert_with(|| super::desktop::load_icon_for_app(app_id, size_px).map(Rc::new))
            .clone()
    }

    /// Collect minimized windows for an output, sorted by minimize order.
    pub fn collect_minimized_icons(
        &mut self,
        output_id: OutputId,
    ) -> Vec<super::desktop::DesktopIcon> {
        let output = match self.outputs.get(&output_id) {
            Some(o) => o,
            None => return Vec::new(),
        };
        let output_ref = output.borrow();

        let scale = output_ref.scale.max(1);
        let icon_size_px = super::desktop::ICON_SIZE * scale;

        // Collect window data into a temp vec to avoid holding borrow of self.windows
        // while calling self.get_icon.
        let mut entries: Vec<(u64, WindowId, String, Option<String>)> = self
            .windows
            .iter()
            .filter_map(|(&window_id, window)| {
                let w = window.borrow();
                if !w.hidden {
                    return None;
                }
                let matches = w
                    .output
                    .as_ref()
                    .and_then(|o| o.upgrade())
                    .map(|o| o.borrow().id == output_ref.id)
                    .unwrap_or(false);
                if !matches {
                    return None;
                }
                let title = w
                    .title
                    .as_ref()
                    .filter(|t| !t.is_empty())
                    .cloned()
                    .or_else(|| w.app_id.clone())
                    .unwrap_or_else(|| format!("Window {}", window_id));
                let app_id = w.app_id.clone();
                Some((w.minimize_seq, window_id, title, app_id))
            })
            .collect();
        drop(output_ref);

        entries.sort_by_key(|(seq, _, _, _)| *seq);

        entries
            .into_iter()
            .map(|(_, window_id, title, app_id)| {
                let icon = app_id
                    .as_deref()
                    .and_then(|id| self.get_icon(id, icon_size_px));
                super::desktop::DesktopIcon {
                    window_id,
                    title,
                    app_id,
                    icon,
                }
            })
            .collect()
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
            let was_hidden;
            {
                let mut w = window.borrow_mut();
                was_hidden = w.hidden;
                if w.hidden {
                    w.show();
                }
                w.place_top();
            }
            if was_hidden {
                self.mark_all_desktops_dirty();
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

        let mut visibility_changed = false;
        if let Some(prev_id) = self.window_menu_alt_tab_preview.take() {
            if self.window_menu_alt_tab_preview_was_hidden {
                if let Some(prev_window) = self.windows.get(&prev_id) {
                    prev_window.borrow_mut().hide();
                    visibility_changed = true;
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
                    visibility_changed = true;
                }
                w.place_top();
            }
            self.focus_preview_visual(window_id);
            self.window_menu_alt_tab_preview = Some(window_id);
            self.window_menu_alt_tab_preview_was_hidden = was_hidden;
        }
        if visibility_changed {
            self.mark_all_desktops_dirty();
        }
    }

    fn restore_alt_tab_state(&mut self) {
        let mut visibility_changed = false;
        if let Some(prev_id) = self.window_menu_alt_tab_preview.take() {
            if self.window_menu_alt_tab_preview_was_hidden {
                if let Some(prev_window) = self.windows.get(&prev_id) {
                    prev_window.borrow_mut().hide();
                    visibility_changed = true;
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
        if visibility_changed {
            self.mark_all_desktops_dirty();
        }
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

    /// Enter desktop icon keyboard focus mode.
    ///
    /// Sets local selection state synchronously and queues the protocol-bound
    /// work (clear keyboard focus, switch seat mode) as actions so they run
    /// inside the next manage sequence. Callers must follow up with
    /// `manage_dirty` to trigger that sequence.
    pub fn enter_icon_focus(&mut self, output_id: OutputId, window_id: WindowId, seat_id: SeatId) {
        // Set the selected icon on the desktop surface
        if let Some(output) = self.outputs.get(&output_id) {
            if let Some(desktop) = output.borrow_mut().desktop_surface.as_mut() {
                desktop.selected_icon = Some(window_id);
                desktop.dirty = true;
            }
        }
        self.icon_focus_output = Some(output_id);
        if let Some(seat) = self.seats.get(&seat_id) {
            let mut seat_ref = seat.borrow_mut();
            seat_ref.queue_action(Action::ClearFocus);
            seat_ref.queue_action(Action::SwitchMode {
                mode: crate::config::Mode::DesktopIcons,
            });
        }
    }

    /// Exit desktop icon keyboard focus mode.
    pub fn exit_icon_focus(&mut self, seat_id: SeatId) {
        if let Some(output_id) = self.icon_focus_output.take() {
            if let Some(output) = self.outputs.get(&output_id) {
                if let Some(desktop) = output.borrow_mut().desktop_surface.as_mut() {
                    desktop.selected_icon = None;
                    desktop.dirty = true;
                }
            }
        }
        if let Some(seat) = self.seats.get(&seat_id) {
            seat.borrow_mut().switch_mode(crate::config::Mode::Default);
        }
    }

    /// Navigate icon selection by (dx, dy) in the icon grid.
    pub fn icon_navigate(&mut self, dx: i32, dy: i32) {
        let Some(output_id) = self.icon_focus_output else {
            return;
        };
        let Some(output) = self.outputs.get(&output_id) else {
            return;
        };
        let mut out = output.borrow_mut();
        let Some(desktop) = out.desktop_surface.as_mut() else {
            return;
        };
        let count = desktop.icon_count() as i32;
        if count == 0 {
            return;
        }
        let cols = desktop.icon_cols.max(1);
        let current_idx = desktop.selected_icon_index().unwrap_or(0) as i32;
        let col = current_idx % cols;
        let row = current_idx / cols;

        let new_idx = if dy != 0 {
            // Move up/down by row
            let new_row = row + dy;
            let candidate = new_row * cols + col;
            if candidate < 0 || candidate >= count {
                // Stay at current position if out of bounds
                current_idx
            } else {
                candidate
            }
        } else {
            // Move left/right linearly with wrapping
            let candidate = current_idx + dx;
            ((candidate % count) + count) % count
        };

        if let Some(window_id) = desktop.icon_window_at_index(new_idx as usize) {
            desktop.selected_icon = Some(window_id);
            desktop.dirty = true;
        }
    }

    /// Activate (restore) the selected desktop icon window.
    pub fn icon_activate(&mut self, seat_id: SeatId) {
        let Some(output_id) = self.icon_focus_output else {
            return;
        };
        let selected = self.outputs.get(&output_id).and_then(|o| {
            o.borrow()
                .desktop_surface
                .as_ref()
                .and_then(|d| d.selected_icon)
        });
        let Some(window_id) = selected else {
            self.exit_icon_focus(seat_id);
            return;
        };
        // Show and focus the window
        if let Some(window) = self.windows.get(&window_id) {
            {
                let mut w = window.borrow_mut();
                w.show();
                w.place_top();
            }
        }
        self.mark_all_desktops_dirty();
        self.exit_icon_focus(seat_id);
        self.focus(window_id);
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
                    if w.is_resizable() {
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
        }

        if edges != 0 {
            let shape = cursor_shape_for_edges(edges);
            seat.borrow_mut().set_cursor_shape(shape);
            return;
        }

        // For WM-owned surfaces, force a sensible cursor so the previous
        // application's cursor doesn't leak through. Over desktop icons we
        // show the pointer (hand); everywhere else on our surfaces, the
        // default arrow.
        let (target, surface_x, surface_y) = {
            let seat_ref = seat.borrow();
            (
                seat_ref.pointer_target,
                seat_ref.last_surface_x,
                seat_ref.last_surface_y,
            )
        };

        let shape = match target {
            super::PointerTarget::Desktop(output_id) => {
                let on_icon = self
                    .outputs
                    .get(&output_id)
                    .and_then(|o| {
                        let o = o.borrow();
                        let scale = o.scale;
                        o.desktop_surface
                            .as_ref()
                            .and_then(|d| d.icon_at(surface_x, surface_y, scale))
                    })
                    .is_some();
                if on_icon {
                    Some(CursorShape::Pointer)
                } else {
                    Some(CursorShape::Default)
                }
            }
            super::PointerTarget::Menu | super::PointerTarget::Titlebar(_) => {
                Some(CursorShape::Default)
            }
            // MenuShield hides the cursor via wl_pointer.set_cursor directly,
            // so don't touch it here.
            super::PointerTarget::MenuShield(_) | super::PointerTarget::None => None,
        };

        seat.borrow_mut().set_cursor_shape(shape);
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new(false)
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
