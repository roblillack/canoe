//! Core context - central state management

use crate::binding::{Action, Direction, PointerBinding, XkbBinding};
use crate::config::{load_config, Config, WindowDecoration};
use crate::rule;
use crate::wm::{
    Capabilities, CursorShape, Edges, OutputHandle, SeatHandle, WindowHandle, WindowManager,
};
use resvg::tiny_skia;

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
    /// The compositor
    pub rwm: Option<WindowManager>,

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
    /// Output last announced to River as the default for layer-shell
    /// surfaces. See [`Context::sync_layer_shell_default_output`].
    layer_shell_default_output: Option<OutputId>,

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
    /// Set when the session is unlocked. The compositor drops keyboard focus
    /// on unlock, so the next manage sequence has to send it again.
    pending_unlock_focus: bool,
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

    /// The open screen-sharing picker, if any
    pub share_picker: Option<super::share_picker::SharePicker>,
    /// Shared windows to make repaint, see [`super::share_picker::Nudge`]
    pub share_nudges: Vec<super::share_picker::Nudge>,
    /// A manage sequence was requested to run due nudges
    pub share_nudge_requested: bool,
    /// The last thing picked for sharing, see
    /// [`super::share_picker::REPEAT_CHOICE_WINDOW`]
    pub share_last_choice: Option<super::share_picker::LastChoice>,

    /// Outputs that moved since the last manage sequence: (id, dx, dy).
    pub pending_output_shifts: Vec<(OutputId, i32, i32)>,
    /// Outputs whose size changed since the last manage sequence:
    /// (id, old width, old height).
    pub pending_output_resizes: Vec<(OutputId, i32, i32)>,
    /// Outputs whose usable area changed since the last manage sequence,
    /// e.g. because a bar was started, stopped or resized.
    pub pending_usable_area_changes: Vec<OutputId>,
    /// Windows anchored to a removed output: (window_id, dead usable area).
    pending_output_orphans: Vec<(WindowId, (i32, i32, i32, i32))>,
    /// Windows fullscreen on a removed output they are not anchored to.
    pending_fullscreen_exits: Vec<WindowId>,
}

impl Context {
    /// Create a new context with default configuration
    pub fn new(skip_config: bool) -> Self {
        Self {
            rwm: None,

            windows: HashMap::new(),
            outputs: HashMap::new(),
            seats: HashMap::new(),

            focus_stack: Vec::new(),
            focused_window: None,
            last_focused_output: None,

            current_output: None,
            current_seat: None,
            layer_shell_default_output: None,

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
            pending_unlock_focus: false,
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

            share_picker: None,
            share_nudges: Vec::new(),
            share_nudge_requested: false,
            share_last_choice: None,

            pending_output_shifts: Vec::new(),
            pending_output_resizes: Vec::new(),
            pending_usable_area_changes: Vec::new(),
            pending_output_orphans: Vec::new(),
            pending_fullscreen_exits: Vec::new(),
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

        // Not manage-sequence-bound; apply immediately.
        let theme = self.config.xcursor_theme_resolved();
        for seat in self.seats.values() {
            seat.borrow().set_xcursor_theme(&theme);
        }

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
    pub fn create_window(&mut self, rwm_window: WindowHandle) -> Rc<RefCell<Window>> {
        let id = self.next_window_id;
        self.next_window_id += 1;

        let mut window = Window::new(id);
        window.rwm_node = Some(rwm_window.node());
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
        let was_focused = self.focused_window == Some(window_id);

        self.focus_stack.retain(|&id| id != window_id);
        self.windows.remove(&window_id);

        if let Some(picker) = self.share_picker.as_mut() {
            picker.remove_target(super::share_picker::ShareTarget::Window(window_id));
        }
        self.share_nudges.retain(|n| n.window != window_id);
        self.forget_share_choice(super::share_picker::ShareTarget::Window(window_id));

        if was_focused {
            self.focused_window = None;
            self.restore_focus_from_stack();
        }
    }

    /// Create a new output and add to context
    pub fn create_output(&mut self, rwm_output: OutputHandle) -> Rc<RefCell<Output>> {
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
        let dead_rect = self.outputs.get(&output_id).map(|output| {
            let output = output.borrow();
            (output.x, output.y, output.width, output.height)
        });

        if let Some(output) = self.outputs.get(&output_id) {
            let dead_area = output.borrow().usable_area();
            for (&window_id, window) in &self.windows {
                let anchored = window
                    .borrow()
                    .output
                    .as_ref()
                    .and_then(|o| o.upgrade())
                    .map(|o| o.borrow().id == output_id)
                    .unwrap_or(false);
                if anchored {
                    self.pending_output_orphans.push((window_id, dead_area));
                    window.borrow_mut().output = None;
                } else if window.borrow().is_fullscreen_on(output_id) {
                    // Fullscreen here at a client's request while anchored
                    // elsewhere: only the fullscreen state needs undoing.
                    self.pending_fullscreen_exits.push(window_id);
                }
            }
        }

        // Update current output if necessary
        // Update current output if necessary. Windows orphaned by the removal
        // move to the current output, so prefer the removed one's neighbour.
        if self.current_output == Some(output_id) {
            self.current_output = dead_rect.and_then(|rect| self.nearest_output(rect, output_id));
        }
        // New windows open on the last focused output while nothing has
        // focus, so it must not keep naming the removed one.
        if self.last_focused_output == Some(output_id) {
            self.last_focused_output =
                dead_rect.and_then(|rect| self.nearest_output(rect, output_id));
        }
        // River forgets the default when its output goes away; the next
        // manage sequence announces the new one.
        if self.layer_shell_default_output == Some(output_id) {
            self.layer_shell_default_output = None;
        }

        self.outputs.remove(&output_id);

        if let Some(picker) = self.share_picker.as_mut() {
            picker.remove_target(super::share_picker::ShareTarget::Output(output_id));
            picker.surfaces.retain(|s| s.output_id != output_id);
        }
        self.forget_share_choice(super::share_picker::ShareTarget::Output(output_id));
    }

    /// Create a new seat and add to context
    pub fn create_seat(&mut self, rwm_seat: SeatHandle) -> Rc<RefCell<Seat>> {
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
        for binding in &mut seat.xkb_bindings {
            binding.enabled = binding.mode == mode;
        }
    }

    /// Take (and clear) the pending-XKB-rebind flag set by a config reload.
    pub fn take_pending_rebind_xkb(&mut self) -> bool {
        std::mem::take(&mut self.pending_rebind_xkb)
    }

    /// Remove a seat from context
    #[allow(dead_code)] // the compositor has a single, permanent seat
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
            // Cycling opens, re-renders and previews the switcher, all of which
            // happens in the dispatch layer as soon as the key is pressed.
            Action::WindowMenuCycle { .. } | Action::WindowMenuCycleApp => {}
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
            // The picker answers over IPC and redraws its overlay, both of
            // which happen in the dispatch layer as soon as the key is pressed.
            Action::SharePickerConfirm
            | Action::SharePickerCancel
            | Action::SharePickerNext
            | Action::SharePickerPrev => {}
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

                // Protocol note: propagate the theme to spawned clients so
                // client-rendered cursors match compositor-rendered ones.
                // Unset parts stay unset, leaving clients their own defaults.
                let theme = self.config.xcursor_theme_resolved();
                if let Some(name) = &theme.name {
                    cmd.env("XCURSOR_THEME", name);
                }
                if let Some(size) = theme.size {
                    cmd.env("XCURSOR_SIZE", size.to_string());
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

        let (old_area, new_area, window_rect, fullscreen) = {
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
                super::window::SavedGeometry {
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                },
                w.fullscreen.clone(),
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

        if old_area.2 <= 0 || old_area.3 <= 0 || new_area.2 <= 0 || new_area.3 <= 0 {
            self.set_window_output(window_id, target_output_id);
            return;
        }

        self.relocate_window(window_id, window_rect, old_area, target_output_id, false);

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

    /// Tell River which output new layer surfaces without an explicit output
    /// (e.g. fuzzel) should appear on, if `current_output` changed since the
    /// last announcement. `set_default` is only valid inside a manage
    /// sequence, so this must only be called from `handle_manage_start`.
    fn sync_layer_shell_default_output(&mut self) {
        let Some(output_id) = self.current_output else {
            return;
        };
        if self.layer_shell_default_output == Some(output_id) {
            return;
        }
        let Some(output) = self.outputs.get(&output_id) else {
            return;
        };
        if let Some(ref rwm_output) = output.borrow().rwm_output {
            rwm_output.set_default_layer_output();
            self.layer_shell_default_output = Some(output_id);
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

    /// Record that the session was unlocked. Focus is sent again in the next
    /// manage sequence, which the compositor starts right after the event.
    pub fn handle_session_unlocked(&mut self) {
        self.session_locked = false;
        self.pending_unlock_focus = true;
    }

    /// River clears every seat's keyboard focus when the session is unlocked
    /// and leaves it to the window manager to pick a new target. Send focus to
    /// the window that had it before the lock, or to the next visible window
    /// if that one went away or was minimized in the meantime.
    fn restore_focus_after_unlock(&mut self) {
        // Unlocking switches the seat back to the default binding mode, so a
        // desktop icon selection from before the lock can't be driven anymore.
        if self.icon_focus_output.is_some() {
            if let Some(seat_id) = self.current_seat {
                self.exit_icon_focus(seat_id);
            }
        }

        let previous = self.focused_window.filter(|id| {
            self.windows
                .get(id)
                .is_some_and(|window| !window.borrow().hidden)
        });

        match previous {
            Some(window_id) => self.focus(window_id),
            None => {
                self.focused_window = None;
                self.restore_focus_from_stack();
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

        let area = output.borrow().usable_area();
        let (content_x, content_y, content_w, content_h) =
            self.snapped_geometry(window_id, super::window::SnapState::Maximized, area);
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

    /// Content geometry (x, y, width, height) of a window snapped to `snap`
    /// within the usable `area`, leaving room for server-side decorations.
    fn snapped_geometry(
        &self,
        window_id: WindowId,
        snap: super::window::SnapState,
        area: (i32, i32, i32, i32),
    ) -> (i32, i32, i32, i32) {
        use super::window::SnapState;

        let (ox, oy, ow, oh) = area;
        let (left, top, right, bottom) = self
            .windows
            .get(&window_id)
            .map(|w| self.frame_insets(&w.borrow()))
            .unwrap_or_default();

        let left_width = ow / 2;
        let (side_x, side_width) = match snap {
            SnapState::Maximized => (ox, ow),
            SnapState::Left => (ox, left_width),
            SnapState::Right => (ox + left_width, ow - left_width),
        };

        (
            side_x + left,
            oy + top,
            (side_width - left - right).max(1),
            (oh - top - bottom).max(1),
        )
    }

    /// How far server-side decorations extend past a window's content on each
    /// side: (left, top, right, bottom). Zero for client-side decorations.
    fn frame_insets(&self, window: &Window) -> (i32, i32, i32, i32) {
        if window.decoration != Some(WindowDecoration::Ssd) {
            return (0, 0, 0, 0);
        }
        let border = self.config.ui.border_width;
        let titlebar = super::titlebar::titlebar_height(&self.config.ui);
        (
            border,
            border + titlebar - window.swallow_top.max(0),
            border,
            border,
        )
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

        let target_snap = match side {
            crate::binding::action::SnapSide::Left => SnapState::Left,
            crate::binding::action::SnapSide::Right => SnapState::Right,
        };
        let area = output.borrow().usable_area();
        let (content_x, content_y, content_w, content_h) =
            self.snapped_geometry(window_id, target_snap, area);
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };

        let mut w = window.borrow_mut();

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

    /// Remember that the user picked `target` for sharing, and make it
    /// repaint if it's a window.
    pub fn record_share_choice(&mut self, target: super::share_picker::ShareTarget) {
        self.share_last_choice = Some(super::share_picker::LastChoice {
            target,
            at: std::time::Instant::now(),
        });
        if let super::share_picker::ShareTarget::Window(window) = target {
            self.schedule_share_nudge(window);
        }
    }

    /// The candidate to answer a new request with without asking: the one the
    /// user picked moments ago, if it's offered again. See
    /// [`super::share_picker::REPEAT_CHOICE_WINDOW`]. Each pick is repeated at
    /// most once.
    pub fn repeat_share_choice(
        &mut self,
        candidates: &[super::share_picker::ShareCandidate],
    ) -> Option<super::share_picker::ShareCandidate> {
        let last = self.share_last_choice.take()?;
        if last.at.elapsed() > super::share_picker::REPEAT_CHOICE_WINDOW {
            return None;
        }
        let candidate = candidates.iter().find(|c| c.target == last.target)?.clone();
        if let super::share_picker::ShareTarget::Window(window) = candidate.target {
            self.schedule_share_nudge(window);
        }
        Some(candidate)
    }

    /// Don't repeat a choice of `target`, which went away.
    fn forget_share_choice(&mut self, target: super::share_picker::ShareTarget) {
        if self.share_last_choice.is_some_and(|c| c.target == target) {
            self.share_last_choice = None;
        }
    }

    /// Make a window that was just picked for sharing repaint, see
    /// [`super::share_picker::Nudge`].
    pub fn schedule_share_nudge(&mut self, window: WindowId) {
        use super::share_picker::{Nudge, NudgeStep, NUDGE_DELAYS};
        let now = std::time::Instant::now();
        self.share_nudges.retain(|n| n.window != window);
        for delay in NUDGE_DELAYS {
            self.share_nudges.push(Nudge {
                window,
                due: now + delay,
                step: NudgeStep::Start,
            });
        }
    }

    /// When the next nudge is due, unless a manage sequence to run it was
    /// already requested.
    pub fn next_share_nudge(&self) -> Option<std::time::Instant> {
        if self.share_nudge_requested {
            return None;
        }
        self.share_nudges.iter().map(|n| n.due).min()
    }

    /// Run the nudges that are due. Must be called in a manage sequence.
    fn run_share_nudges(&mut self) {
        use super::share_picker::{Nudge, NudgeStep, NUDGE_LENGTH};
        self.share_nudge_requested = false;
        let now = std::time::Instant::now();
        let (due, rest): (Vec<Nudge>, Vec<Nudge>) =
            self.share_nudges.drain(..).partition(|n| n.due <= now);
        self.share_nudges = rest;
        for nudge in due {
            let Some(window) = self.windows.get(&nudge.window) else {
                continue;
            };
            let w = window.borrow();
            let Some(rwm_window) = w.rwm_window.as_ref() else {
                continue;
            };
            match nudge.step {
                NudgeStep::Start => {
                    // Leave a window the user is resizing alone, and don't
                    // overlap with a nudge still in progress.
                    let busy = matches!(w.operator, super::window::Operator::Resize { .. })
                        || self
                            .share_nudges
                            .iter()
                            .any(|n| n.window == nudge.window && n.step == NudgeStep::End);
                    if busy {
                        continue;
                    }
                    rwm_window.inform_resize_start();
                    rwm_window.set_tiled(Edges::Top | Edges::Bottom | Edges::Left | Edges::Right);
                    self.share_nudges.push(Nudge {
                        window: nudge.window,
                        due: now + NUDGE_LENGTH,
                        step: NudgeStep::End,
                    });
                }
                NudgeStep::End => {
                    rwm_window.inform_resize_end();
                    // Canoe never marks windows tiled.
                    rwm_window.set_tiled(Edges::empty());
                }
            }
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

        let shifts = std::mem::take(&mut self.pending_output_shifts);
        for (output_id, dx, dy) in shifts {
            self.apply_output_shift(output_id, dx, dy);
        }
        let resizes = std::mem::take(&mut self.pending_output_resizes);
        let mut area_changes = std::mem::take(&mut self.pending_usable_area_changes);
        for &(output_id, old_width, old_height) in &resizes {
            self.refit_output_windows(output_id, old_width, old_height);
        }
        // Refitting a resized output already snapped its windows again.
        area_changes.retain(|id| !resizes.iter().any(|&(resized, _, _)| resized == *id));
        area_changes.sort_unstable();
        area_changes.dedup();
        for output_id in area_changes {
            self.resnap_output_windows(output_id);
        }
        let orphans = std::mem::take(&mut self.pending_output_orphans);
        if !orphans.is_empty() {
            for (window_id, dead_area) in orphans {
                self.migrate_orphaned_window(window_id, dead_area);
            }
            self.mark_all_desktops_dirty(); // icons follow their window
        }
        for window_id in std::mem::take(&mut self.pending_fullscreen_exits) {
            self.restore_from_stale_fullscreen(window_id);
        }
        self.fit_oversized_new_windows();

        self.run_share_nudges();

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

        if std::mem::take(&mut self.pending_unlock_focus) {
            self.restore_focus_after_unlock();
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

        self.sync_layer_shell_default_output();
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

                // Ask the window to pick a size that fits where it will be
                // placed. Apps that ignore this hint get shrunk once they're
                // placed, see place_new_window.
                if let Some(window) = self.windows.get(&window_id) {
                    let bounds = {
                        let w = window.borrow();
                        self.placement_area(&w).map(|(_, _, area_w, area_h)| {
                            let (left, top, right, bottom) = self.frame_insets(&w);
                            (area_w - left - right, area_h - top - bottom)
                        })
                    };
                    if let Some((max_width, max_height)) = bounds {
                        window
                            .borrow()
                            .set_dimension_bounds(max_width.max(1), max_height.max(1));
                    }
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

    /// Visible windows in stacking order, topmost first.
    ///
    /// Mirrors what `handle_render_start` asks River for: fullscreen windows
    /// cover everything, the most recently focused window's parent chain is
    /// raised, and dialogs sit directly above their parents.
    pub fn stacking_order(&self) -> Vec<WindowId> {
        let focus_index = |id: WindowId| {
            self.focus_stack
                .iter()
                .position(|&f| f == id)
                .unwrap_or(usize::MAX)
        };
        let mut ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| {
                let w = w.borrow();
                !w.hidden && w.width > 0 && w.height > 0
            })
            .map(|(&id, _)| id)
            .collect();
        ids.sort_by_key(|&id| {
            let fullscreen = self.windows.get(&id).is_some_and(|w| {
                !matches!(w.borrow().fullscreen, super::window::FullscreenState::None)
            });
            (
                !fullscreen,
                focus_index(self.root_ancestor(id)),
                std::cmp::Reverse(self.parent_chain_depth(id)),
                focus_index(id),
                id,
            )
        });
        ids
    }

    /// Match the portal's offer (see [`super::share_picker::resolve_candidates`])
    /// against the outputs and windows we manage.
    pub fn share_candidates(&self, lines: &[String]) -> Vec<super::share_picker::ShareCandidate> {
        // Outputs left to right, top to bottom.
        let mut outputs: Vec<(i32, i32, OutputId, String)> = self
            .outputs
            .iter()
            .filter_map(|(&id, o)| {
                let o = o.borrow();
                o.name.clone().map(|name| (o.x, o.y, id, name))
            })
            .collect();
        outputs.sort();
        let outputs: Vec<(OutputId, String)> = outputs
            .into_iter()
            .map(|(_, _, id, name)| (id, name))
            .collect();
        let windows: Vec<super::share_picker::WindowInfo> = self
            .stacking_order()
            .into_iter()
            .enumerate()
            .filter_map(|(stack_index, id)| {
                let w = self.windows.get(&id)?.borrow();
                Some(super::share_picker::WindowInfo {
                    id,
                    identifier: w.identifier.clone(),
                    title: w.title.clone(),
                    stack_index,
                })
            })
            .collect();
        super::share_picker::resolve_candidates(lines, &outputs, &windows)
    }

    /// The picker candidate at global point `(x, y)`: the topmost window there
    /// if it may be shared, else the output there if that may be.
    pub fn share_candidate_at(&self, x: i32, y: i32) -> Option<usize> {
        use super::share_picker::ShareTarget;
        let picker = self.share_picker.as_ref()?;
        let ui = &self.config.ui;
        for id in self.stacking_order() {
            let Some(window) = self.windows.get(&id) else {
                continue;
            };
            let (wx, wy, ww, wh) = window.borrow().frame_rect(ui);
            if x >= wx && x < wx + ww && y >= wy && y < wy + wh {
                if let Some(idx) = picker.index_of(ShareTarget::Window(id)) {
                    return Some(idx);
                }
                break;
            }
        }
        let output_id = self.output_at_point(x, y)?;
        picker.index_of(ShareTarget::Output(output_id))
    }

    /// The highlighted picker target's rectangle in global logical
    /// coordinates, and the text naming it.
    pub fn share_highlight(&self) -> Option<((i32, i32, i32, i32), String)> {
        use super::share_picker::ShareTarget;
        let picker = self.share_picker.as_ref()?;
        let candidate = picker.candidates.get(picker.hovered?)?;
        match candidate.target {
            ShareTarget::Window(id) => {
                let w = self.windows.get(&id)?.borrow();
                if w.hidden {
                    return None;
                }
                let name = w
                    .title
                    .clone()
                    .filter(|t| !t.is_empty())
                    .or_else(|| w.app_id.clone())
                    .unwrap_or_else(|| "this window".to_string());
                Some((
                    w.frame_rect(&self.config.ui),
                    format!("Share \u{201c}{name}\u{201d}"),
                ))
            }
            ShareTarget::Output(id) => {
                let o = self.outputs.get(&id)?.borrow();
                let name = o.name.clone().unwrap_or_else(|| "this screen".to_string());
                Some((
                    (o.x, o.y, o.width, o.height),
                    format!("Share screen {name} ({}\u{d7}{})", o.width, o.height),
                ))
            }
        }
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

    /// The area a new window is placed in: the usable area of the output it
    /// was assigned, its parent's, or the one new windows go to.
    fn placement_area(&self, window: &Window) -> Option<(i32, i32, i32, i32)> {
        let output = window
            .output
            .as_ref()
            .and_then(|o| o.upgrade())
            .or_else(|| {
                let parent = self.windows.get(&window.parent?)?.borrow();
                parent.output.as_ref().and_then(|o| o.upgrade())
            })
            .or_else(|| {
                let output_id = self
                    .preferred_output_for_window_id(window.id)
                    .or(self.current_output)?;
                self.outputs.get(&output_id).cloned()
            })?;
        let output = output.borrow();
        let output_rect = (output.x, output.y, output.width, output.height);
        intersect_rects(output.usable_area(), output_rect)
    }

    /// Give a new window its first position, near (`x`, `y`) but moved so its
    /// frame lies within its placement area. A window too large for the area
    /// is pinned to its top-left, keeping the titlebar reachable, and shrunk
    /// in the next manage sequence.
    fn place_new_window(&mut self, window_id: WindowId, x: i32, y: i32) {
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        let (x, y, oversized) = {
            let w = window.borrow();
            match self.placement_area(&w) {
                Some(area) => {
                    fit_new_window((x, y, w.width, w.height), self.frame_insets(&w), area)
                }
                None => (x, y, false),
            }
        };
        let mut w = window.borrow_mut();
        w.set_position(x, y);
        if oversized {
            w.pending_initial_fit = true;
            if let Some(ref rwm) = self.rwm {
                rwm.manage_dirty();
            }
        }
    }

    /// Shrink windows that were placed while larger than their area, as far
    /// as their minimum size allows.
    fn fit_oversized_new_windows(&mut self) {
        for window in self.windows.values() {
            if !window.borrow().pending_initial_fit {
                continue;
            }
            let (area, insets) = {
                let w = window.borrow();
                (self.placement_area(&w), self.frame_insets(&w))
            };
            let mut w = window.borrow_mut();
            w.pending_initial_fit = false;
            // Sized by other means since it was placed.
            let resized_since = w.maximized
                || w.snap_state.is_some()
                || !matches!(w.fullscreen, super::window::FullscreenState::None)
                || !matches!(w.operator, super::window::Operator::None);
            let Some((_, _, area_w, area_h)) = area.filter(|_| !resized_since) else {
                continue;
            };
            let (left, top, right, bottom) = insets;
            let width = w.width.min(area_w - left - right).max(w.min_width).max(1);
            let height = w.height.min(area_h - top - bottom).max(w.min_height).max(1);
            if (width, height) != (w.width, w.height) {
                w.propose_dimensions(width, height);
            }
        }
    }

    fn apply_initial_positions(&mut self) {
        // Dialogs (windows with a parent) get centred over their parent
        // rather than being cascaded along with regular toplevels.
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
                let cx = parent.x + (parent.width - w.width) / 2;
                let cy = parent.y + (parent.height - w.height) / 2;
                Some((id, cx, cy))
            })
            .collect();
        for (id, x, y) in dialog_positions {
            self.place_new_window(id, x, y);
        }

        let mut output_windows: HashMap<OutputId, Vec<WindowId>> = HashMap::new();

        for (&window_id, window) in &self.windows {
            let w = window.borrow();
            // Wait for the window's first size: placement needs it, and River
            // doesn't show the window before then anyway.
            if !w.position_undefined || w.width <= 0 || w.height <= 0 {
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

                self.place_new_window(*window_id, x, y);
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
            let mut out = output.borrow_mut();
            if let Some(desktop) = out.desktop_surface.as_mut() {
                desktop.dirty = true;
            }
            if let Some(background) = out.desktop_background.as_mut() {
                background.dirty = true;
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

        let scale = output_ref.desktop_scale();
        let icon_size_px = scale.px(super::desktop::ICON_SIZE);

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

        self.activate_window(window_id);
    }

    /// Restore a window if it is minimized, raise and focus it.
    pub fn activate_window(&mut self, window_id: WindowId) {
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        let was_hidden = {
            let mut w = window.borrow_mut();
            let was_hidden = w.hidden;
            w.show();
            w.place_top();
            was_hidden
        };
        if was_hidden {
            self.mark_all_desktops_dirty();
        }
        self.focus(window_id);
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

        // The picker overlay covers every window; their borders don't apply.
        if matches!(
            seat.borrow().pointer_target,
            super::PointerTarget::SharePicker(_)
        ) {
            seat.borrow_mut()
                .set_cursor_shape(Some(CursorShape::Crosshair));
            return;
        }

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
                        o.desktop_surface
                            .as_ref()
                            .and_then(|d| d.icon_at(surface_x, surface_y))
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
            super::PointerTarget::SharePicker(_) => Some(CursorShape::Crosshair),
            // MenuShield hides the cursor itself, so don't touch it here.
            super::PointerTarget::MenuShield(_) | super::PointerTarget::None => None,
        };

        seat.borrow_mut().set_cursor_shape(shape);
    }

    /// Windows anchored to an output that moved keep their position relative
    /// to it (river re-anchors remaining outputs on unplug, which would
    /// otherwise strand them at stale global coordinates).
    fn apply_output_shift(&mut self, output_id: OutputId, dx: i32, dy: i32) {
        for window in self.windows.values() {
            let mut w = window.borrow_mut();
            // A window being moved or resized follows the pointer, whose
            // global position the layout change doesn't touch; shifting it
            // would only be undone by the next pointer motion.
            if w.position_undefined || !matches!(w.operator, super::window::Operator::None) {
                continue;
            }
            let anchored = w
                .output
                .as_ref()
                .and_then(|o| o.upgrade())
                .map(|o| o.borrow().id == output_id)
                .unwrap_or(false);
            if !anchored {
                continue;
            }
            // set_position() keeps pre_fullscreen in sync with the position
            // while the window is windowed; only shift it ourselves otherwise.
            let tracks_pre_fullscreen =
                matches!(w.fullscreen, super::window::FullscreenState::None)
                    && !w.pending_unfullscreen_restore;
            let (new_x, new_y) = (w.x + dx, w.y + dy);
            w.set_position(new_x, new_y);
            if let Some(ref mut saved) = w.pre_snap {
                saved.x += dx;
                saved.y += dy;
            }
            if !tracks_pre_fullscreen {
                if let Some(ref mut saved) = w.pre_fullscreen {
                    saved.x += dx;
                    saved.y += dy;
                }
            }
        }
    }

    /// The output other than `exclude` whose center is closest to the center
    /// of `rect` (x, y, width, height); ties go to the lowest id.
    fn nearest_output(&self, rect: (i32, i32, i32, i32), exclude: OutputId) -> Option<OutputId> {
        let center = |x: i32, y: i32, w: i32, h: i32| {
            (
                i64::from(x) * 2 + i64::from(w),
                i64::from(y) * 2 + i64::from(h),
            )
        };
        let (cx, cy) = center(rect.0, rect.1, rect.2, rect.3);
        self.outputs
            .iter()
            .filter(|(&id, _)| id != exclude)
            .map(|(&id, output)| {
                let output = output.borrow();
                let (ox, oy) = center(output.x, output.y, output.width, output.height);
                ((ox - cx).pow(2) + (oy - cy).pow(2), id)
            })
            .min()
            .map(|(_, id)| id)
    }

    /// Restore the windowed geometry of a window that was fullscreen on a
    /// removed output while anchored to a remaining one.
    fn restore_from_stale_fullscreen(&mut self, window_id: WindowId) {
        let Some(window) = self.windows.get(&window_id) else {
            return;
        };
        let mut w = window.borrow_mut();
        if !w.clear_stale_fullscreen() {
            return;
        }
        // Leaving fullscreen leaves position and dimensions undefined until
        // both are proposed.
        if let Some(saved) = w.pre_fullscreen {
            w.set_position(saved.x, saved.y);
            w.propose_dimensions(saved.width, saved.height);
        } else {
            // No windowed geometry to go back to: let the client pick its
            // size, at the top-left of the output it's anchored to.
            let origin = w
                .output
                .as_ref()
                .and_then(|output| output.upgrade())
                .map(|output| {
                    let (x, y, _, _) = output.borrow().usable_area();
                    (x, y)
                });
            if let Some((x, y)) = origin {
                w.set_position(x, y);
            }
            w.propose_preferred_dimensions();
        }
    }

    /// Keep windows on an output that changed size (a new resolution or scale)
    /// usable: snapped and maximized windows are snapped again to the new
    /// size, and windows that fit the output before but not anymore are moved
    /// back onto it, shrunk as far as needed and their minimum size allows.
    /// Windows that already reached past the output's edges were put there
    /// on purpose and stay where they are.
    fn refit_output_windows(&mut self, output_id: OutputId, old_width: i32, old_height: i32) {
        use super::window::{FullscreenState, Operator, SavedGeometry, SnapState};

        let Some(output) = self.outputs.get(&output_id) else {
            return;
        };
        let (old_rect, new_rect, area) = {
            let out = output.borrow();
            let new_rect = (out.x, out.y, out.width, out.height);
            // The usable area may still reflect exclusive zones laid out for
            // the old size; never let it reach past the output.
            let area = intersect_rects(out.usable_area(), new_rect).unwrap_or(new_rect);
            ((out.x, out.y, old_width, old_height), new_rect, area)
        };
        if old_width <= 0 || old_height <= 0 || area.2 <= 0 || area.3 <= 0 {
            return;
        }

        let window_ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, window)| {
                let w = window.borrow();
                !w.position_undefined
                    && matches!(w.operator, Operator::None)
                    && w.output
                        .as_ref()
                        .and_then(|o| o.upgrade())
                        .is_some_and(|o| o.borrow().id == output_id)
            })
            .map(|(&id, _)| id)
            .collect();

        for window_id in window_ids {
            let Some(window) = self.windows.get(&window_id).cloned() else {
                continue;
            };
            let insets = self.frame_insets(&window.borrow());
            let min_size = {
                let w = window.borrow();
                (w.min_width, w.min_height)
            };
            let refit = |rect: SavedGeometry| {
                refit_rect(
                    (rect.x, rect.y, rect.width, rect.height),
                    insets,
                    min_size,
                    old_rect,
                    new_rect,
                    area,
                )
                .map(|(x, y, width, height)| SavedGeometry {
                    x,
                    y,
                    width,
                    height,
                })
            };

            let (fullscreen, snap, current, pre_snap, pre_fullscreen) = {
                let w = window.borrow();
                let snap = w.snap_state.or(w.maximized.then_some(SnapState::Maximized));
                let current = SavedGeometry {
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                };
                (
                    !matches!(w.fullscreen, FullscreenState::None),
                    snap,
                    current,
                    w.pre_snap,
                    w.pre_fullscreen,
                )
            };

            if fullscreen {
                // River sizes fullscreen windows; fix up where they go back to.
                if let Some(saved) = pre_fullscreen.and_then(refit) {
                    window.borrow_mut().pre_fullscreen = Some(saved);
                }
                continue;
            }

            let geometry = match snap {
                Some(snap) => {
                    let (x, y, width, height) = self.snapped_geometry(window_id, snap, area);
                    Some(SavedGeometry {
                        x,
                        y,
                        width,
                        height,
                    })
                }
                None => refit(current),
            };
            let pre_snap = snap.and(pre_snap).and_then(refit);

            let mut w = window.borrow_mut();
            if let Some(saved) = pre_snap {
                w.pre_snap = Some(saved);
            }
            if let Some(geometry) = geometry {
                w.set_position(geometry.x, geometry.y);
                if (geometry.width, geometry.height) != (current.width, current.height) {
                    w.propose_dimensions(geometry.width, geometry.height);
                }
            }
        }
    }

    /// Snap the snapped and maximized windows anchored to `output_id` again
    /// to its current usable area, so they follow bars that appear, go away
    /// or change size. Other windows stay where they are.
    fn resnap_output_windows(&mut self, output_id: OutputId) {
        use super::window::{FullscreenState, Operator, SnapState};

        let Some(output) = self.outputs.get(&output_id) else {
            return;
        };
        let area = {
            let out = output.borrow();
            let rect = (out.x, out.y, out.width, out.height);
            intersect_rects(out.usable_area(), rect).unwrap_or(rect)
        };
        if area.2 <= 0 || area.3 <= 0 {
            return;
        }

        let snapped: Vec<(WindowId, SnapState)> = self
            .windows
            .iter()
            .filter_map(|(&id, window)| {
                let w = window.borrow();
                let anchored = w
                    .output
                    .as_ref()
                    .and_then(|o| o.upgrade())
                    .is_some_and(|o| o.borrow().id == output_id);
                if !anchored
                    || w.position_undefined
                    || !matches!(w.operator, Operator::None)
                    || !matches!(w.fullscreen, FullscreenState::None)
                {
                    return None;
                }
                let snap = w
                    .snap_state
                    .or(w.maximized.then_some(SnapState::Maximized))?;
                Some((id, snap))
            })
            .collect();

        for (window_id, snap) in snapped {
            let (x, y, width, height) = self.snapped_geometry(window_id, snap, area);
            let Some(window) = self.windows.get(&window_id) else {
                continue;
            };
            let mut w = window.borrow_mut();
            if (w.x, w.y) != (x, y) {
                w.set_position(x, y);
            }
            if (w.width, w.height) != (width, height) {
                w.propose_dimensions(width, height);
            }
        }
    }

    /// Carry `rect`, a window's geometry within `old_area`, over to output
    /// `target_id`, keeping its position relative to the usable area. The
    /// window keeps its size unless it doesn't fit or `resize` is set. A
    /// snapped or maximized window is snapped the same way on the target, and
    /// its restore geometry moves along. Returns the geometry the window was
    /// given.
    fn relocate_window(
        &mut self,
        window_id: WindowId,
        rect: super::window::SavedGeometry,
        old_area: (i32, i32, i32, i32),
        target_id: OutputId,
        resize: bool,
    ) -> super::window::SavedGeometry {
        let new_area = self
            .outputs
            .get(&target_id)
            .map(|output| output.borrow().usable_area())
            .unwrap_or(old_area);
        let (snap_state, maximized, pre_snap) = self
            .windows
            .get(&window_id)
            .map(|window| {
                let w = window.borrow();
                (w.snap_state, w.maximized, w.pre_snap)
            })
            .unwrap_or_default();

        let snap_state = snap_state.or(maximized.then_some(super::window::SnapState::Maximized));
        let too_large = rect.width.max(1) > new_area.2 || rect.height.max(1) > new_area.3;
        let (x, y, width, height) = match snap_state {
            Some(snap) => self.snapped_geometry(window_id, snap, new_area),
            None => map_rect_between_areas(
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                old_area,
                new_area,
                too_large,
            ),
        };
        let mapped_pre_snap = if snap_state.is_some() {
            pre_snap.map(|s| {
                let (x, y, width, height) =
                    map_rect_between_areas(s.x, s.y, s.width, s.height, old_area, new_area, true);
                super::window::SavedGeometry {
                    x,
                    y,
                    width,
                    height,
                }
            })
        } else {
            None
        };

        self.set_window_output(window_id, target_id);
        if let Some(window) = self.windows.get(&window_id) {
            let mut w = window.borrow_mut();
            if let Some(saved) = mapped_pre_snap {
                w.pre_snap = Some(saved);
            }
            w.set_position(x, y);
            if snap_state.is_some() || too_large || resize {
                w.propose_dimensions(width, height);
            }
        }
        super::window::SavedGeometry {
            x,
            y,
            width,
            height,
        }
    }

    /// Move a window from a removed output onto a remaining one, mapping its
    /// geometry proportionally. Maximized/snapped windows are snapped the same
    /// way on the target; fullscreen-on-the-dead-output degrades to
    /// its pre-fullscreen geometry per the protocol's implicit exit.
    fn migrate_orphaned_window(&mut self, window_id: WindowId, dead_area: (i32, i32, i32, i32)) {
        let target_id = self
            .current_output
            .filter(|id| self.outputs.contains_key(id))
            .or_else(|| self.outputs.keys().min().copied());
        let Some(target_id) = target_id else {
            self.pending_output_orphans.push((window_id, dead_area));
            return;
        };
        let Some(target) = self.outputs.get(&target_id).cloned() else {
            return;
        };
        let new_area = target.borrow().usable_area();
        if new_area.2 <= 0 || new_area.3 <= 0 {
            self.pending_output_orphans.push((window_id, dead_area));
            return;
        }

        let (rect, stale_fullscreen, unplaced) = {
            let Some(window) = self.windows.get(&window_id) else {
                return;
            };
            let mut w = window.borrow_mut();

            // Use the pre-fullscreen geometry as migration base.
            let stale_fullscreen = w.clear_stale_fullscreen();
            let rect = if stale_fullscreen {
                w.pre_fullscreen.unwrap_or(super::window::SavedGeometry {
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                })
            } else {
                super::window::SavedGeometry {
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                }
            };
            (rect, stale_fullscreen, w.position_undefined)
        };

        // Not placed yet: there is no geometry to carry over, and the initial
        // placement will still happen once the window is set up.
        if unplaced && !stale_fullscreen {
            self.set_window_output(window_id, target_id);
            return;
        }

        // Leaving fullscreen leaves position and dimensions undefined until
        // both are proposed, so always propose the windowed size.
        let placed = self.relocate_window(window_id, rect, dead_area, target_id, stale_fullscreen);
        if stale_fullscreen {
            if let Some(window) = self.windows.get(&window_id) {
                window.borrow_mut().pre_fullscreen = Some(placed);
            }
        }
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

/// Proportionally map a rect from one output area to another, clamped to
/// fit. `resize = false` keeps the rect's size (clamped); `true` scales it
/// with the area ratio.
fn map_rect_between_areas(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    old_area: (i32, i32, i32, i32),
    new_area: (i32, i32, i32, i32),
    resize: bool,
) -> (i32, i32, i32, i32) {
    let (ox, oy, ow, oh) = old_area;
    let (nx, ny, nw, nh) = new_area;
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
}

/// The overlap of two (x, y, width, height) rects, if any.
fn intersect_rects(
    a: (i32, i32, i32, i32),
    b: (i32, i32, i32, i32),
) -> Option<(i32, i32, i32, i32)> {
    let x1 = a.0.max(b.0);
    let y1 = a.1.max(b.1);
    let x2 = (a.0 + a.2).min(b.0 + b.2);
    let y2 = (a.1 + a.3).min(b.1 + b.3);
    (x2 > x1 && y2 > y1).then_some((x1, y1, x2 - x1, y2 - y1))
}

/// Move a new window's content `rect` so its frame (content plus `insets`:
/// left, top, right, bottom) lies within `area`, by as little as possible.
/// Returns the new position and whether the frame is too large to fit, in
/// which case it's pinned to the area's top-left.
fn fit_new_window(
    rect: (i32, i32, i32, i32),
    insets: (i32, i32, i32, i32),
    area: (i32, i32, i32, i32),
) -> (i32, i32, bool) {
    let (x, y, width, height) = rect;
    let (left, top, right, bottom) = insets;
    let (ax, ay, aw, ah) = area;
    let frame_w = width + left + right;
    let frame_h = height + top + bottom;
    let frame_x = (x - left).clamp(ax, (ax + aw - frame_w).max(ax));
    let frame_y = (y - top).clamp(ay, (ay + ah - frame_h).max(ay));
    (frame_x + left, frame_y + top, frame_w > aw || frame_h > ah)
}

/// Whether rect `inner` lies entirely within rect `outer`.
fn rect_contains(outer: (i32, i32, i32, i32), inner: (i32, i32, i32, i32)) -> bool {
    inner.0 >= outer.0
        && inner.1 >= outer.1
        && inner.0 + inner.2 <= outer.0 + outer.2
        && inner.1 + inner.3 <= outer.1 + outer.3
}

/// Refit a window's content `rect` after its output changed from `old_rect`
/// to `new_rect`. Only a window whose frame (content plus `insets`: left, top,
/// right, bottom) fit the old output but not the new one is touched: it is
/// moved into `area` by as little as possible, and shrunk to fit if needed but
/// no smaller than `min_size`. If it can't fit, it's pinned to the top-left
/// so its titlebar stays reachable. Returns the new content rect, or `None`
/// if the window should stay as it is.
fn refit_rect(
    rect: (i32, i32, i32, i32),
    insets: (i32, i32, i32, i32),
    min_size: (i32, i32),
    old_rect: (i32, i32, i32, i32),
    new_rect: (i32, i32, i32, i32),
    area: (i32, i32, i32, i32),
) -> Option<(i32, i32, i32, i32)> {
    let (left, top, right, bottom) = insets;
    let frame = (
        rect.0 - left,
        rect.1 - top,
        rect.2 + left + right,
        rect.3 + top + bottom,
    );
    if !rect_contains(old_rect, frame) || rect_contains(new_rect, frame) {
        return None;
    }

    let (ax, ay, aw, ah) = area;
    let width = rect.2.min(aw - left - right).max(min_size.0).max(1);
    let height = rect.3.min(ah - top - bottom).max(min_size.1).max(1);
    let frame_w = width + left + right;
    let frame_h = height + top + bottom;
    let frame_x = frame.0.clamp(ax, (ax + aw - frame_w).max(ax));
    let frame_y = frame.1.clamp(ay, (ay + ah - frame_h).max(ay));
    Some((frame_x + left, frame_y + top, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context with outputs `(id, x, y, width, height)`, no Wayland objects.
    fn context_with_outputs(outputs: &[(OutputId, i32, i32, i32, i32)]) -> Context {
        let mut ctx = Context::new(false);
        for &(id, x, y, width, height) in outputs {
            let mut output = Output::new(id);
            output.update_position(x, y);
            output.update_dimensions(width, height);
            ctx.outputs.insert(id, Rc::new(RefCell::new(output)));
        }
        ctx
    }

    /// Add a placed window `(x, y, width, height)` anchored to `output_id`.
    fn add_window(
        ctx: &mut Context,
        id: WindowId,
        output_id: OutputId,
        rect: (i32, i32, i32, i32),
    ) {
        let mut window = Window::new(id);
        window.output = ctx.outputs.get(&output_id).map(Rc::downgrade);
        window.set_position(rect.0, rect.1);
        window.update_dimensions(rect.2, rect.3);
        ctx.windows.insert(id, Rc::new(RefCell::new(window)));
    }

    #[test]
    fn map_rect_keeps_size_and_relative_position() {
        let old = (1920, 0, 1920, 1080);
        let new = (0, 0, 1280, 720);
        // Top-left at 25% / 50% of the old area, size fits the new one.
        assert_eq!(
            map_rect_between_areas(2400, 540, 400, 300, old, new, false),
            (320, 360, 400, 300)
        );
    }

    #[test]
    fn map_rect_scales_when_resizing() {
        let old = (0, 0, 2000, 1000);
        let new = (0, 0, 1000, 500);
        assert_eq!(
            map_rect_between_areas(1000, 0, 1000, 1000, old, new, true),
            (500, 0, 500, 500)
        );
    }

    #[test]
    fn map_rect_clamps_into_new_area() {
        let old = (0, 0, 2000, 1000);
        let new = (100, 50, 1000, 500);
        // Too large without resizing: clamped to the area and pinned to it.
        assert_eq!(
            map_rect_between_areas(1900, 900, 1500, 800, old, new, false),
            (100, 50, 1000, 500)
        );
        // Starting off the old area's right edge: kept inside the new one.
        assert_eq!(
            map_rect_between_areas(2500, 0, 200, 100, old, new, false),
            (900, 50, 200, 100)
        );
    }

    #[test]
    fn output_shift_moves_window_and_saved_geometry_once() {
        let mut ctx = context_with_outputs(&[(1, 1920, 0, 1920, 1080)]);
        add_window(&mut ctx, 10, 1, (2000, 100, 800, 600));
        {
            let mut w = ctx.windows[&10].borrow_mut();
            w.snap_state = Some(super::super::window::SnapState::Left);
            w.pre_snap = Some(super::super::window::SavedGeometry {
                x: 2100,
                y: 200,
                width: 640,
                height: 480,
            });
        }

        ctx.pending_output_shifts.push((1, -1920, 0));
        ctx.handle_manage_start();

        let w = ctx.windows[&10].borrow();
        assert_eq!((w.x, w.y), (80, 100));
        let pre_fullscreen = w.pre_fullscreen.unwrap();
        assert_eq!((pre_fullscreen.x, pre_fullscreen.y), (80, 100));
        let pre_snap = w.pre_snap.unwrap();
        assert_eq!((pre_snap.x, pre_snap.y), (180, 200));
    }

    #[test]
    fn output_shift_moves_fullscreen_restore_geometry() {
        let mut ctx = context_with_outputs(&[(1, 1920, 0, 1920, 1080)]);
        add_window(&mut ctx, 10, 1, (2000, 100, 800, 600));
        {
            let output = Rc::downgrade(&ctx.outputs[&1]);
            let mut w = ctx.windows[&10].borrow_mut();
            w.fullscreen = super::super::window::FullscreenState::Output(output);
        }

        ctx.pending_output_shifts.push((1, -1920, 0));
        ctx.handle_manage_start();

        let w = ctx.windows[&10].borrow();
        let pre_fullscreen = w.pre_fullscreen.unwrap();
        assert_eq!((pre_fullscreen.x, pre_fullscreen.y), (80, 100));
    }

    #[test]
    fn removed_fullscreen_output_restores_windowed_geometry() {
        let mut ctx = context_with_outputs(&[(1, 0, 0, 1920, 1080), (2, 1920, 0, 3840, 2160)]);
        add_window(&mut ctx, 10, 2, (2100, 100, 800, 600));
        {
            let output = Rc::downgrade(&ctx.outputs[&2]);
            let mut w = ctx.windows[&10].borrow_mut();
            w.fullscreen = super::super::window::FullscreenState::Output(output);
            // River reports the fullscreen size while fullscreen.
            w.update_dimensions(3840, 2160);
        }

        ctx.destroy_output(2);
        ctx.handle_manage_start();

        let w = ctx.windows[&10].borrow();
        assert!(matches!(
            w.fullscreen,
            super::super::window::FullscreenState::None
        ));
        assert_eq!(w.proposed_dimensions(), (800, 600));
        let pre_fullscreen = w.pre_fullscreen.unwrap();
        assert_eq!((pre_fullscreen.width, pre_fullscreen.height), (800, 600));
        assert_eq!((w.x, w.y), (90, 50));
    }

    #[test]
    fn removed_output_exits_fullscreen_of_window_anchored_elsewhere() {
        let mut ctx = context_with_outputs(&[(1, 0, 0, 1920, 1080), (2, 1920, 0, 3840, 2160)]);
        add_window(&mut ctx, 10, 1, (100, 100, 800, 600));
        {
            let output = Rc::downgrade(&ctx.outputs[&2]);
            let mut w = ctx.windows[&10].borrow_mut();
            w.fullscreen = super::super::window::FullscreenState::Output(output);
            w.update_dimensions(3840, 2160);
        }

        ctx.destroy_output(2);
        ctx.handle_manage_start();

        let w = ctx.windows[&10].borrow();
        assert!(matches!(
            w.fullscreen,
            super::super::window::FullscreenState::None
        ));
        assert_eq!((w.x, w.y), (100, 100));
        assert_eq!(w.proposed_dimensions(), (800, 600));
    }

    #[test]
    fn removed_output_is_not_kept_as_last_focused() {
        let mut ctx = context_with_outputs(&[(1, 0, 0, 1920, 1080), (2, 1920, 0, 1920, 1080)]);
        ctx.current_output = Some(1);
        ctx.last_focused_output = Some(2);

        ctx.destroy_output(2);

        assert_eq!(ctx.last_focused_output, Some(1));
        assert_eq!(ctx.current_output_for_new_window(), Some(1));
    }

    #[test]
    fn removed_output_only_reanchors_unplaced_windows() {
        let mut ctx = context_with_outputs(&[(1, 0, 0, 1920, 1080), (2, 1920, 0, 1920, 1080)]);
        let mut window = Window::new(10);
        window.output = Some(Rc::downgrade(&ctx.outputs[&2]));
        ctx.windows.insert(10, Rc::new(RefCell::new(window)));

        ctx.destroy_output(2);
        ctx.handle_manage_start();

        let w = ctx.windows[&10].borrow();
        assert!(w.position_undefined);
        let anchor = w.output.as_ref().and_then(|o| o.upgrade()).unwrap();
        assert_eq!(anchor.borrow().id, 1);
    }

    #[test]
    fn removed_current_output_hands_over_to_nearest_output() {
        // Narrow output 1 on the left, removed output 2, output 3 on the right.
        let layout = [
            (1, 0, 0, 1280, 1024),
            (2, 1280, 0, 1920, 1080),
            (3, 3200, 0, 1920, 1080),
        ];
        let mut ctx = context_with_outputs(&layout);
        ctx.current_output = Some(2);
        add_window(&mut ctx, 10, 2, (1380, 100, 800, 600));
        ctx.destroy_output(2);
        ctx.handle_manage_start();
        assert_eq!(ctx.current_output, Some(1));
        let w = ctx.windows[&10].borrow();
        let anchor = w.output.as_ref().and_then(|o| o.upgrade()).unwrap();
        assert_eq!(anchor.borrow().id, 1);
        drop(anchor);
        drop(w);

        // Mirrored: now output 3 is closer.
        let layout = [
            (1, 0, 0, 1920, 1080),
            (2, 1920, 0, 1920, 1080),
            (3, 3840, 0, 1280, 1024),
        ];
        let mut ctx = context_with_outputs(&layout);
        ctx.current_output = Some(2);
        ctx.destroy_output(2);
        assert_eq!(ctx.current_output, Some(3));
    }

    #[test]
    fn orphans_follow_surviving_current_output() {
        let layout = [
            (1, 0, 0, 1920, 1080),
            (2, 1920, 0, 1920, 1080),
            (3, 3840, 0, 1920, 1080),
        ];
        let mut ctx = context_with_outputs(&layout);
        ctx.current_output = Some(3);
        add_window(&mut ctx, 10, 2, (2000, 100, 800, 600));
        ctx.destroy_output(2);
        ctx.handle_manage_start();
        let w = ctx.windows[&10].borrow();
        let anchor = w.output.as_ref().and_then(|o| o.upgrade()).unwrap();
        assert_eq!(anchor.borrow().id, 3);
    }

    #[test]
    fn output_shift_skips_window_under_pointer_operation() {
        let mut ctx = context_with_outputs(&[(1, 1920, 0, 1920, 1080)]);
        add_window(&mut ctx, 10, 1, (2000, 100, 800, 600));
        ctx.windows[&10]
            .borrow_mut()
            .start_move(std::rc::Weak::new());

        ctx.pending_output_shifts.push((1, -1920, 0));
        ctx.handle_manage_start();

        let mut w = ctx.windows[&10].borrow_mut();
        assert_eq!((w.x, w.y), (2000, 100));
        // The next pointer motion places it relative to where the drag began.
        w.apply_op_delta(10, 0);
        assert_eq!((w.x, w.y), (2010, 100));
    }

    #[test]
    fn removed_output_resnaps_window_exactly() {
        let mut ctx = context_with_outputs(&[(1, 0, 0, 1280, 1024), (2, 1280, 0, 1920, 1080)]);
        let b = ctx.config.ui.border_width;
        let t = super::super::titlebar::titlebar_height(&ctx.config.ui);
        add_window(
            &mut ctx,
            10,
            2,
            (2240 + b, b + t, 960 - 2 * b, 1080 - 2 * b - t),
        );
        {
            let mut w = ctx.windows[&10].borrow_mut();
            w.decoration = Some(WindowDecoration::Ssd);
            w.snap_state = Some(super::super::window::SnapState::Right);
            w.pre_snap = Some(super::super::window::SavedGeometry {
                x: 1480,
                y: 100,
                width: 960,
                height: 540,
            });
        }

        ctx.destroy_output(2);
        ctx.handle_manage_start();

        let w = ctx.windows[&10].borrow();
        assert_eq!((w.x, w.y), (640 + b, b + t));
        assert_eq!(w.proposed_dimensions(), (640 - 2 * b, 1024 - 2 * b - t));
        assert_eq!(w.snap_state, Some(super::super::window::SnapState::Right));
        let pre_snap = w.pre_snap.unwrap();
        assert_eq!(
            (pre_snap.x, pre_snap.y, pre_snap.width, pre_snap.height),
            (133, 95, 640, 512)
        );
    }

    #[test]
    fn refit_rect_leaves_windows_that_still_fit_or_never_did() {
        let old = (0, 0, 2560, 1440);
        let new = (0, 0, 1920, 1080);
        // Still fits.
        assert_eq!(
            refit_rect((100, 100, 800, 600), (0, 0, 0, 0), (0, 0), old, new, new),
            None
        );
        // Reached past the old output's edge already.
        assert_eq!(
            refit_rect((2000, 100, 800, 600), (0, 0, 0, 0), (0, 0), old, new, new),
            None
        );
    }

    #[test]
    fn refit_rect_moves_and_shrinks_by_as_little_as_needed() {
        let old = (0, 0, 2560, 1440);
        let new = (0, 0, 1920, 1080);
        let insets = (2, 30, 2, 2);
        // Fits in size, only moved back in.
        assert_eq!(
            refit_rect((1500, 200, 800, 600), insets, (0, 0), old, new, new),
            Some((1118, 200, 800, 600))
        );
        // Too large: shrunk to the area, frame included.
        assert_eq!(
            refit_rect((100, 100, 2400, 1300), insets, (0, 0), old, new, new),
            Some((2, 30, 1916, 1048))
        );
        // Minimum size wins over fitting; pinned to the top-left.
        assert_eq!(
            refit_rect((100, 100, 2400, 1300), insets, (2000, 0), old, new, new),
            Some((2, 30, 2000, 1048))
        );
    }

    #[test]
    fn resized_output_refits_windows() {
        let mut ctx = context_with_outputs(&[(1, 0, 0, 2560, 1440)]);
        // Fits before, not after.
        add_window(&mut ctx, 10, 1, (1500, 800, 800, 600));
        // Already reaching past the edge: left alone.
        add_window(&mut ctx, 11, 1, (2200, 100, 800, 600));
        // Maximized: follows the new size.
        add_window(&mut ctx, 12, 1, (0, 0, 2560, 1440));
        {
            let mut w = ctx.windows[&12].borrow_mut();
            w.maximized = true;
            w.snap_state = Some(super::super::window::SnapState::Maximized);
        }

        ctx.outputs[&1].borrow_mut().update_dimensions(1920, 1080);
        ctx.pending_output_resizes.push((1, 2560, 1440));
        ctx.handle_manage_start();

        let w = ctx.windows[&10].borrow();
        assert_eq!((w.x, w.y), (1120, 480));
        drop(w);
        let w = ctx.windows[&11].borrow();
        assert_eq!((w.x, w.y), (2200, 100));
        drop(w);
        let w = ctx.windows[&12].borrow();
        assert_eq!((w.x, w.y), (0, 0));
        assert_eq!(w.proposed_dimensions(), (1920, 1080));
    }

    #[test]
    fn usable_area_change_resnaps_snapped_windows_only() {
        use super::super::window::SnapState;

        let mut ctx = context_with_outputs(&[(1, 0, 0, 2560, 1440)]);
        // Maximized and left-snapped: follow the new area.
        add_window(&mut ctx, 10, 1, (0, 0, 2560, 1440));
        add_window(&mut ctx, 11, 1, (0, 0, 1280, 1440));
        // Free-floating: left alone.
        add_window(&mut ctx, 12, 1, (100, 10, 800, 600));
        {
            let mut w = ctx.windows[&10].borrow_mut();
            w.maximized = true;
            w.snap_state = Some(SnapState::Maximized);
        }
        ctx.windows[&11].borrow_mut().snap_state = Some(SnapState::Left);

        // A 32px bar appears at the top.
        ctx.outputs[&1]
            .borrow_mut()
            .update_non_exclusive_area(0, 32, 2560, 1408);
        ctx.pending_usable_area_changes.push(1);
        ctx.handle_manage_start();

        let w = ctx.windows[&10].borrow();
        assert_eq!((w.x, w.y), (0, 32));
        assert_eq!(w.proposed_dimensions(), (2560, 1408));
        drop(w);
        let w = ctx.windows[&11].borrow();
        assert_eq!((w.x, w.y), (0, 32));
        assert_eq!(w.proposed_dimensions(), (1280, 1408));
        drop(w);
        let w = ctx.windows[&12].borrow();
        assert_eq!((w.x, w.y), (100, 10));
    }

    #[test]
    fn fit_new_window_moves_or_pins_frame() {
        let area = (0, 0, 1366, 768);
        let insets = (2, 30, 2, 2);
        // Fits where it is.
        assert_eq!(
            fit_new_window((100, 100, 800, 600), insets, area),
            (100, 100, false)
        );
        // Fits, but placed across the bottom-right edge.
        assert_eq!(
            fit_new_window((700, 300, 800, 600), insets, area),
            (564, 166, false)
        );
        // Too large: pinned so the titlebar stays reachable.
        assert_eq!(
            fit_new_window((40, 60, 1920, 1080), insets, area),
            (2, 30, true)
        );
    }

    #[test]
    fn oversized_new_window_is_pinned_then_shrunk() {
        let mut ctx = context_with_outputs(&[(1, 0, 0, 1366, 768)]);
        let mut window = Window::new(10);
        window.output = Some(Rc::downgrade(&ctx.outputs[&1]));
        window.decoration = Some(WindowDecoration::Ssd);
        window.min_width = 400;
        window.update_dimensions(1920, 1080);
        ctx.windows.insert(10, Rc::new(RefCell::new(window)));
        let (left, top, right, bottom) = ctx.frame_insets(&ctx.windows[&10].borrow());

        ctx.apply_initial_positions();
        {
            let w = ctx.windows[&10].borrow();
            assert_eq!((w.x, w.y), (left, top));
            assert!(w.pending_initial_fit);
        }

        ctx.handle_manage_start();
        let w = ctx.windows[&10].borrow();
        assert!(!w.pending_initial_fit);
        assert_eq!(
            w.proposed_dimensions(),
            (1366 - left - right, 768 - top - bottom)
        );
    }

    #[test]
    fn new_windows_wait_for_their_size_and_cascade_within_the_area() {
        let mut ctx = context_with_outputs(&[(1, 0, 0, 1366, 768)]);
        for id in [10, 11] {
            let mut window = Window::new(id);
            window.output = Some(Rc::downgrade(&ctx.outputs[&1]));
            ctx.windows.insert(id, Rc::new(RefCell::new(window)));
        }

        // No size yet: not placed.
        ctx.apply_initial_positions();
        assert!(ctx.windows[&10].borrow().position_undefined);

        for id in [10, 11] {
            ctx.windows[&id].borrow_mut().update_dimensions(1200, 700);
        }
        ctx.apply_initial_positions();
        // The second cascade slot would reach past the right edge.
        for id in [10, 11] {
            let w = ctx.windows[&id].borrow();
            assert!(!w.position_undefined);
            assert!(!w.pending_initial_fit);
            assert!(w.x >= 0 && w.x + w.width <= 1366, "x = {}", w.x);
            assert!(w.y >= 0 && w.y + w.height <= 768, "y = {}", w.y);
        }
    }

    fn context_with_output_ids(output_ids: &[OutputId]) -> Context {
        let mut ctx = Context::new(true);
        for &id in output_ids {
            ctx.outputs
                .insert(id, Rc::new(RefCell::new(Output::new(id))));
        }
        ctx.current_output = output_ids.first().copied();
        ctx
    }

    /// Adds a window on `output_id` and pushes it to the back of the focus
    /// stack, so windows added later are less recently focused.
    fn add_stacked_window(ctx: &mut Context, id: WindowId, output_id: OutputId, hidden: bool) {
        let mut window = Window::new(id);
        window.output = Some(Rc::downgrade(&ctx.outputs[&output_id]));
        window.hidden = hidden;
        ctx.windows.insert(id, Rc::new(RefCell::new(window)));
        ctx.focus_stack.push(id);
    }

    #[test]
    fn destroying_focused_window_focuses_most_recent_remaining_window() {
        let mut ctx = context_with_output_ids(&[1]);
        add_stacked_window(&mut ctx, 10, 1, false);
        add_stacked_window(&mut ctx, 11, 1, false);
        add_stacked_window(&mut ctx, 12, 1, false);
        ctx.focus(10);

        ctx.destroy_window(10);

        assert_eq!(ctx.focused_window, Some(11));
        assert_eq!(ctx.focus_stack, vec![11, 12]);
        assert!(!ctx.windows.contains_key(&10));
    }

    #[test]
    fn destroying_focused_window_skips_minimized_windows() {
        let mut ctx = context_with_output_ids(&[1]);
        add_stacked_window(&mut ctx, 10, 1, false);
        add_stacked_window(&mut ctx, 11, 1, true);
        add_stacked_window(&mut ctx, 12, 1, false);
        ctx.focus(10);

        ctx.destroy_window(10);

        assert_eq!(ctx.focused_window, Some(12));
    }

    #[test]
    fn destroying_focused_window_prefers_windows_on_the_same_output() {
        let mut ctx = context_with_output_ids(&[1, 2]);
        add_stacked_window(&mut ctx, 10, 1, false);
        add_stacked_window(&mut ctx, 11, 2, false);
        add_stacked_window(&mut ctx, 12, 1, false);
        ctx.focus(10);

        ctx.destroy_window(10);

        assert_eq!(ctx.focused_window, Some(12));
    }

    #[test]
    fn destroying_unfocused_window_keeps_focus() {
        let mut ctx = context_with_output_ids(&[1]);
        add_stacked_window(&mut ctx, 10, 1, false);
        add_stacked_window(&mut ctx, 11, 1, false);
        ctx.focus(11);

        ctx.destroy_window(10);

        assert_eq!(ctx.focused_window, Some(11));
        assert_eq!(ctx.focus_stack, vec![11]);
    }

    #[test]
    fn destroying_last_visible_window_clears_focus() {
        let mut ctx = context_with_output_ids(&[1]);
        add_stacked_window(&mut ctx, 10, 1, false);
        add_stacked_window(&mut ctx, 11, 1, true);
        ctx.focus(10);

        ctx.destroy_window(10);

        assert_eq!(ctx.focused_window, None);
        assert_eq!(ctx.focus_stack, vec![11]);
    }

    #[test]
    fn unlocking_restores_focus_to_previously_focused_window() {
        let mut ctx = context_with_output_ids(&[1]);
        add_stacked_window(&mut ctx, 10, 1, false);
        add_stacked_window(&mut ctx, 11, 1, false);
        ctx.focus(11);
        ctx.session_locked = true;

        ctx.handle_session_unlocked();
        assert!(!ctx.session_locked);
        ctx.handle_manage_start();

        assert_eq!(ctx.focused_window, Some(11));
        assert_eq!(ctx.focus_stack, vec![11, 10]);
        assert!(!ctx.pending_unlock_focus);
    }

    #[test]
    fn unlocking_focuses_next_window_when_previous_was_minimized() {
        let mut ctx = context_with_output_ids(&[1]);
        add_stacked_window(&mut ctx, 10, 1, false);
        add_stacked_window(&mut ctx, 11, 1, false);
        ctx.focus(10);
        ctx.windows[&10].borrow_mut().hidden = true;

        ctx.handle_session_unlocked();
        ctx.handle_manage_start();

        assert_eq!(ctx.focused_window, Some(11));
    }

    #[test]
    fn unlocking_focuses_most_recent_window_when_nothing_was_focused() {
        let mut ctx = context_with_output_ids(&[1]);
        add_stacked_window(&mut ctx, 10, 1, false);
        add_stacked_window(&mut ctx, 11, 1, false);

        ctx.handle_session_unlocked();
        ctx.handle_manage_start();

        assert_eq!(ctx.focused_window, Some(10));
    }

    #[test]
    fn manage_start_without_unlock_leaves_focus_alone() {
        let mut ctx = context_with_output_ids(&[1]);
        add_stacked_window(&mut ctx, 10, 1, false);

        ctx.handle_manage_start();

        assert_eq!(ctx.focused_window, None);
    }
}
