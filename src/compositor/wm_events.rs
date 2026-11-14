//! Events for the window manager: windows, outputs, the seat, and pointer
//! input on the window manager's own surfaces, reported the way River's
//! window-management protocol did (see `crate::wm`).

use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::desktop::space::SpaceElement;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::Output;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Rectangle, Serial};
use smithay::wayland::compositor::{
    send_surface_state, with_states, with_surface_tree_downward, TraversalAction,
};
use smithay::wayland::fractional_scale::with_fractional_scale;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity as WlrKeyboardInteractivity, Layer as WlrLayer,
};
use smithay::wayland::shell::xdg::{SurfaceCachedState, ToplevelSurface, XdgToplevelSurfaceData};

use crate::binding;
use crate::canoe;
use crate::canoe::scale::FractionalSurface;
use crate::canoe::surface::{Decoration, Surface};
use crate::config;
use crate::wm::{OutputHandle, WindowHandle};

use super::focus::KeyboardFocus;
use super::wm_glue::*;
use super::{ManagedOutput, ManagedWindow, Shell, State};
use crate::wm::WindowKey;

/// How long a window's new position waits for the client to take the size
/// proposed with it.
pub(super) const POSITION_SYNC_TIMEOUT: Duration = Duration::from_millis(150);

/// How long the global of an output that went away stays, disabled, for
/// clients that bind it before they hear it is gone.
const RETIRED_GLOBAL_LINGER: Duration = Duration::from_secs(5);

impl State {
    // ------------------------------------------------------------------
    // Manage and render sequences

    /// Run manage and render sequences until the window manager is
    /// satisfied, then carry out what it asked for.
    pub fn run_wm(&mut self) {
        let mut rounds = 0;
        // Requests queued outside a sequence (by an input handler, say) are
        // applied by the next one.
        if !self.requests.is_empty() {
            self.wm_dirty = true;
        }
        let trace = std::env::var_os("CANOE_TRACE_WM").is_some();
        let t0 = Instant::now();
        let mut phases = Vec::new();
        while self.wm_dirty && rounds < 8 {
            rounds += 1;
            self.wm_dirty = false;
            let t = Instant::now();
            self.manage_sequence();
            phases.push(("manage", t.elapsed()));
            let t = Instant::now();
            self.apply_requests();
            self.render_sequence();
            phases.push(("render", t.elapsed()));
            let t = Instant::now();
            self.apply_requests();
            phases.push(("apply", t.elapsed()));
        }
        if self.wm_dirty {
            self.schedule_wm();
        }
        let t = Instant::now();
        self.send_pending_configures();
        self.restack_x11_windows();
        self.update_keyboard_focus();
        phases.push(("focus", t.elapsed()));
        let t = Instant::now();
        self.update_surface_outputs();
        self.update_wlr_toplevels();
        phases.push(("outputs", t.elapsed()));
        self.queue_redraw_all();
        self.maybe_launch_startup_commands();
        if trace {
            eprintln!(
                "wm: {:?} total, {} windows: {:?}",
                t0.elapsed(),
                self.windows.len(),
                phases
            );
        }
    }

    fn manage_sequence(&mut self) {
        // River sends the pointer position in every manage sequence.
        let (x, y) = (
            self.pointer_location.x.round() as i32,
            self.pointer_location.y.round() as i32,
        );
        self.wm_pointer_position(x, y);

        self.context.borrow_mut().handle_manage_start();
        // After a config reload, rebuild key bindings.
        if self.context.borrow_mut().take_pending_rebind_xkb() {
            reregister_xkb_bindings(self);
        }
        // If in icon focus mode, validate that the selected icon still exists
        {
            let mut context = self.context.borrow_mut();
            if let Some(output_id) = context.icon_focus_output {
                let selected_still_valid = context
                    .outputs
                    .get(&output_id)
                    .and_then(|o| {
                        o.borrow()
                            .desktop_surface
                            .as_ref()
                            .and_then(|d| d.selected_icon)
                    })
                    .map(|wid| {
                        context
                            .windows
                            .get(&wid)
                            .map(|w| w.borrow().hidden)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if !selected_still_valid {
                    if let Some(seat_id) = context.current_seat {
                        context.exit_icon_focus(seat_id);
                    }
                }
            }
        }
        self.context.borrow().finish_manage();
        render_all_desktop_surfaces(self);
    }

    /// Launch the startup commands once the outputs and the desktop exist.
    fn maybe_launch_startup_commands(&mut self) {
        if !self.startup_pending || self.outputs.is_empty() {
            return;
        }
        self.startup_pending = false;
        let cmds = self.context.borrow().config.startup_cmds.clone();
        for cmd in &cmds {
            self.context.borrow().spawn(cmd);
        }
    }

    // ------------------------------------------------------------------
    // Windows

    pub fn add_toplevel(&mut self, toplevel: ToplevelSurface) {
        let window = Window::new_wayland_window(toplevel.clone());
        let surface = toplevel.wl_surface().clone();
        self.add_window(Shell::Xdg(toplevel), window, surface);
    }

    /// Show a new window and tell the window manager about it.
    pub(super) fn add_window(
        &mut self,
        shell: Shell,
        smithay_window: Window,
        surface: WlSurface,
    ) -> WindowKey {
        let key = self.next_key();
        let handle = WindowHandle::new(key, self.requests.clone());
        let window = self.context.borrow_mut().create_window(handle);
        let window_id = window.borrow().id;

        // Decorations: the shadow below the content, the frame above it.
        {
            let shadow_surface = Surface::new();
            let shadow_decoration = Decoration::below(&shadow_surface);
            window.borrow_mut().shadow = Some(canoe::WindowShadow::new(
                shadow_surface,
                shadow_decoration,
                FractionalSurface::new(),
            ));
            let surface = Surface::new();
            let decoration = Decoration::above(&surface);
            window.borrow_mut().titlebar = Some(canoe::Titlebar::new(
                surface,
                decoration,
                FractionalSurface::new(),
            ));
        }

        let (app_id, title) = shell_properties(&shell);
        let pid = shell
            .x11_pid()
            .map(|pid| pid as i32)
            .or_else(|| {
                surface
                    .client()
                    .and_then(|client| client.get_credentials(&self.display_handle).ok())
                    .map(|credentials| credentials.pid)
            })
            .unwrap_or(0);
        {
            let mut w = window.borrow_mut();
            w.app_id = app_id;
            w.title = title;
            w.pid = pid;
            w.skip_switcher = super::protocols::plasma_shell::skip_switcher(&surface);
            w.decoration_hint = Some(match &shell {
                // What River reports for a client without a decoration
                // object, unless it asked through KDE's protocol already.
                Shell::Xdg(toplevel) => {
                    super::handlers::kde_decoration_hint(toplevel.wl_surface()).unwrap_or(0)
                }
                Shell::X11(x11) => super::xwayland::decoration_hint(x11),
            });
            w.queue_event(canoe::WindowEvent::Init);
        }

        self.windows.push(ManagedWindow {
            key,
            id: window_id,
            window: smithay_window,
            shell,
            surface,
            x11_size: None,
            // Announced to taskbars and capture clients once mapped.
            foreign: None,
            wlr_toplevel: None,
            pos: (0, 0).into(),
            hidden: false,
            maximized: false,
            fullscreen: None,
            clip: Default::default(),
            content_clip: Default::default(),
            ssd: true,
            reported_size: None,
            reported_hints: None,
            needs_configure: false,
            decoration_mode_dirty: false,
            pending_positions: Vec::new(),
            initial_commit: false,
            warp: None,
            icon: None,
        });
        self.schedule_wm();
        key
    }

    pub fn toplevel_committed(&mut self, surface: &WlSurface) {
        let Some(key) = self.window_for_surface(surface).map(|w| w.key) else {
            return;
        };
        self.report_size_hints(key);
        let window = self.window_by_key_mut(key).unwrap();
        if !window.configurable() {
            // The window manager picks the size, decorations and state the
            // window starts with.
            window.initial_commit = true;
            self.run_wm();
            if let Some(window) = self.window_by_key_mut(key) {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_configure();
                }
                window.needs_configure = false;
            }
            return;
        }

        let has_buffer =
            smithay::backend::renderer::utils::with_renderer_surface_state(surface, |state| {
                state.buffer().is_some()
            })
            .unwrap_or(false);
        if !has_buffer {
            return;
        }
        let size = window.window.geometry().size;
        let key = window.key;
        self.settle_position(key);
        self.capture_window_damage(key);
        let window = self.window_by_key_mut(key).unwrap();
        if size.w > 0 && size.h > 0 && window.reported_size != Some(size) {
            window.reported_size = Some(size);
            let id = window.id;
            if window.foreign.is_none() {
                let (app_id, title) = shell_properties(&window.shell);
                let foreign = self.foreign_toplevel_state.new_toplevel::<State>(
                    title.as_deref().unwrap_or(""),
                    app_id.as_deref().unwrap_or(""),
                );
                if let Some(w) = self.context.borrow().windows.get(&id) {
                    w.borrow_mut().identifier = Some(foreign.identifier());
                }
                self.window_by_key_mut(key).unwrap().foreign = Some(foreign);
            }
            if let Some(w) = self.context.borrow().windows.get(&id) {
                w.borrow_mut().update_dimensions(size.w, size.h);
            }
            self.schedule_wm();
        }
    }

    /// Tell the window manager about changed size limits.
    pub(super) fn report_size_hints(&mut self, key: u64) {
        let Some(window) = self.window_by_key_mut(key) else {
            return;
        };
        let (min, max) = match &window.shell {
            Shell::Xdg(toplevel) => with_states(toplevel.wl_surface(), |states| {
                let mut cached = states.cached_state.get::<SurfaceCachedState>();
                let current = cached.current();
                (current.min_size, current.max_size)
            }),
            Shell::X11(x11) => (
                x11.min_size().unwrap_or_default(),
                x11.max_size().unwrap_or_default(),
            ),
        };
        let hints = (min.w, min.h, max.w, max.h);
        if window.reported_hints == Some(hints) {
            return;
        }
        window.reported_hints = Some(hints);
        let id = window.id;
        {
            let context = self.context.borrow();
            if let Some(w) = context.windows.get(&id) {
                let mut w = w.borrow_mut();
                w.min_width = min.w;
                w.min_height = min.h;
                w.max_width = max.w;
                w.max_height = max.h;
                context.apply_rules_to_window(&mut w);
            }
        }
        self.schedule_wm();
    }

    pub fn toplevel_properties_changed(&mut self, surface: &WlSurface) {
        let Some(window) = self.window_for_surface(surface) else {
            return;
        };
        let id = window.id;
        let (app_id, title) = shell_properties(&window.shell);
        if let Some(foreign) = window.foreign.as_ref() {
            foreign.send_title(title.as_deref().unwrap_or(""));
            foreign.send_app_id(app_id.as_deref().unwrap_or(""));
            foreign.send_done();
        }
        {
            let context = self.context.borrow();
            if let Some(w) = context.windows.get(&id) {
                let mut w = w.borrow_mut();
                // A minimized window's desktop icon shows its title, and its
                // app id picks the icon.
                let icon_changed = w.hidden && (w.app_id != app_id || w.title != title);
                w.app_id = app_id;
                w.title = title;
                context.apply_rules_to_window(&mut w);
                drop(w);
                if icon_changed {
                    context.mark_all_desktops_dirty();
                }
            }
        }
        self.schedule_wm();
    }

    /// The client set a new icon on its toplevel `surface` (`None`: back to
    /// the default one).
    pub fn toplevel_icon_changed(
        &mut self,
        surface: &WlSurface,
        icon: Option<std::sync::Arc<canoe::ClientIcon>>,
    ) {
        let Some(id) = self.window_for_surface(surface).map(|w| w.id) else {
            return;
        };
        let context = self.context.borrow();
        let Some(window) = context.windows.get(&id) else {
            return;
        };
        let hidden = {
            let mut w = window.borrow_mut();
            w.client_icon = icon;
            w.client_icon_pixmap = None;
            w.hidden
        };
        // Only minimized windows show their icon.
        if hidden {
            context.mark_all_desktops_dirty();
            drop(context);
            self.schedule_wm();
        }
    }

    /// The client asked for its toplevel `surface` to be left out of window
    /// switchers (`skip`), or to be listed again.
    pub fn toplevel_skip_switcher_changed(&mut self, surface: &WlSurface, skip: bool) {
        let Some(id) = self.window_for_surface(surface).map(|w| w.id) else {
            // Not a window (yet); `add_window` reads the flag.
            return;
        };
        if let Some(window) = self.context.borrow().windows.get(&id) {
            window.borrow_mut().skip_switcher = skip;
        }
    }

    /// Where each minimized window's desktop icon is: its output and its
    /// image's rectangle there, in logical pixels.
    pub fn desktop_icon_rects(&self) -> Vec<(canoe::WindowId, Output, Rectangle<i32, Logical>)> {
        let context = self.context.borrow();
        let mut rects = Vec::new();
        for (&output_id, output) in &context.outputs {
            let Some(managed) = self.output_by_id(output_id) else {
                continue;
            };
            let output = output.borrow();
            let Some(desktop) = output.desktop_surface.as_ref() else {
                continue;
            };
            // The desktop covers its output, so its pixels count from the
            // output's corner.
            let device = desktop.scale.device();
            for (window_id, (x, y, w, h)) in desktop.icon_rects() {
                let logical = |v: i32| (v as f64 / device).round() as i32;
                let rect = Rectangle::new(
                    (logical(x), logical(y)).into(),
                    (logical(w), logical(h)).into(),
                );
                rects.push((window_id, managed.output.clone(), rect));
            }
        }
        rects
    }

    /// Remember where windows' desktop icons are now.
    pub fn update_icon_geometry(&mut self) {
        let rects = self.desktop_icon_rects();
        // Where minimized windows fly to and come back from. A window keeps
        // its icon's last place, since it is shown again before canoe lays
        // out the desktop without the icon.
        let icons: Vec<(canoe::WindowId, Rectangle<i32, Logical>)> = rects
            .iter()
            .filter_map(|(id, output, rect)| {
                let origin = self.output_geometry(output)?.loc;
                Some((*id, Rectangle::new(origin + rect.loc, rect.size)))
            })
            .collect();
        for window in &mut self.windows {
            if let Some((_, rect)) = icons.iter().find(|(id, _)| *id == window.id) {
                window.icon = Some(*rect);
            }
        }
    }

    pub fn remove_toplevel(&mut self, surface: &WlSurface) {
        let Some(index) = self.windows.iter().position(|w| w.wl_surface() == surface) else {
            return;
        };
        let window = self.windows.remove(index);
        if let Some(foreign) = window.foreign.as_ref() {
            self.image_capture_state.toplevel_closed(foreign);
            foreign.send_closed();
            self.foreign_toplevel_state.remove_toplevel(foreign);
        }
        if let Some(toplevel) = window.wlr_toplevel.as_ref() {
            self.foreign_toplevel_manager_state.close(toplevel);
        }
        if self.wm_focus == Some(window.key) {
            self.wm_focus = None;
        }
        if self.pointer_window == Some(window.id) {
            self.pointer_window = None;
            self.wm_pointer_leave();
        }
        self.context.borrow_mut().destroy_window(window.id);
        if self.context.borrow().share_picker.is_some() {
            render_share_picker(self);
        }
        self.schedule_wm();
        self.schedule_trim();
    }

    /// Send the configures the window manager's requests made necessary.
    pub fn send_pending_configures(&mut self) {
        for window in &mut self.windows {
            // What the window manager decided last, once per batch: a
            // decision changing back and forth within it reaches nobody.
            if std::mem::take(&mut window.decoration_mode_dirty) {
                window.send_decoration_mode();
            }
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            if !window.needs_configure {
                continue;
            }
            // Nothing may be sent before the client's initial commit.
            if !toplevel.is_initial_configure_sent() {
                continue;
            }
            toplevel.send_pending_configure();
            window.needs_configure = false;
        }
        self.configure_x11_windows();
    }

    // ------------------------------------------------------------------
    // Keyboard focus

    /// Give the keyboard to whoever should have it: the lock screen, a
    /// layer surface that asked for it, else the window the window manager
    /// focused.
    pub fn update_keyboard_focus(&mut self) {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;

        let target: Option<KeyboardFocus> = if self.lock_state != super::LockState::Unlocked {
            self.outputs
                .iter()
                .filter_map(|o| o.lock_surface.as_ref())
                .next()
                .map(|lock| lock.wl_surface().clone().into())
        } else if let Some((surface, _)) = self.layer_focus.as_ref() {
            Some(surface.clone().into())
        } else {
            self.wm_focus
                .and_then(|key| self.window_by_key(key))
                .filter(|w| !w.hidden)
                .map(|w| match &w.shell {
                    Shell::Xdg(_) => w.wl_surface().clone().into(),
                    Shell::X11(x11) => {
                        KeyboardFocus::X11(Box::new(x11.clone()), w.wl_surface().clone())
                    }
                })
        };

        // Activated state follows the window manager's focus.
        let active = self.wm_focus;
        for window in &mut self.windows {
            let activated = Some(window.key) == active;
            match &window.shell {
                Shell::Xdg(toplevel) => {
                    let changed = toplevel.with_pending_state(|state| {
                        let had = state.states.contains(XdgState::Activated);
                        if activated {
                            state.states.set(XdgState::Activated);
                        } else {
                            state.states.unset(XdgState::Activated);
                        }
                        had != activated
                    });
                    if changed {
                        window.needs_configure = true;
                    }
                }
                Shell::X11(x11) => {
                    if x11.is_activated() != activated {
                        let _ = x11.set_activated(activated);
                    }
                }
            }
        }
        self.send_pending_configures();

        let keyboard = self.seat.get_keyboard().unwrap();
        if keyboard.is_grabbed() {
            return;
        }
        if keyboard.current_focus() != target {
            keyboard.set_focus(self, target, Serial::from(0));
        }
    }

    /// Re-evaluate which layer surface, if any, takes the keyboard.
    pub fn update_layer_focus(&mut self) {
        let mut exclusive = None;
        for output in &self.outputs {
            let map = layer_map_for_output(&output.output);
            for layer in [WlrLayer::Overlay, WlrLayer::Top] {
                for surface in map.layers_on(layer) {
                    if surface.cached_state().keyboard_interactivity
                        == WlrKeyboardInteractivity::Exclusive
                    {
                        exclusive.get_or_insert_with(|| surface.wl_surface().clone());
                    }
                }
            }
        }
        match (exclusive, self.layer_focus.clone()) {
            (Some(surface), Some((focused, true))) if surface == focused => {}
            (Some(surface), _) => {
                self.layer_focus = Some((surface, true));
                self.wm_layer_focus(true, true);
            }
            (None, Some((_, true))) => self.release_layer_focus(),
            (None, _) => {}
        }
        self.update_keyboard_focus();
    }

    /// Give an on-demand layer surface the keyboard after a click on it.
    pub fn focus_layer_on_demand(&mut self, surface: &WlSurface) {
        if matches!(self.layer_focus, Some((_, true))) {
            return;
        }
        self.layer_focus = Some((surface.clone(), false));
        self.wm_layer_focus(true, false);
        self.update_keyboard_focus();
    }

    pub fn release_layer_focus(&mut self) {
        if self.layer_focus.take().is_some() {
            self.wm_layer_focus(false, false);
        }
        self.update_keyboard_focus();
    }

    /// River's layer-shell seat events: focus_exclusive, focus_non_exclusive
    /// and focus_none.
    fn wm_layer_focus(&mut self, focused: bool, exclusive: bool) {
        let seat = {
            let context = self.context.borrow();
            context
                .current_seat
                .and_then(|id| context.seats.get(&id).cloned())
        };
        let Some(seat) = seat else {
            return;
        };
        {
            let mut seat_ref = seat.borrow_mut();
            if focused && exclusive {
                seat_ref.focus_exclusive = true;
                seat_ref.queue_action(binding::Action::ClearFocus);
            } else if focused {
                seat_ref.focus_exclusive = false;
            } else {
                let was_exclusive = seat_ref.focus_exclusive;
                seat_ref.focus_exclusive = false;
                if was_exclusive {
                    seat_ref.queue_action(binding::Action::RestoreFocus);
                }
            }
        }
        self.schedule_wm();
    }

    // ------------------------------------------------------------------
    // Outputs

    pub fn add_output(&mut self, output: Output) {
        let key = self.next_key();
        let handle = OutputHandle::new(key, self.requests.clone());
        let id = self.context.borrow_mut().create_output(handle).borrow().id;
        let global = output.create_global::<State>(&self.display_handle);
        self.output_management_state.add_head(&output);
        self.output_management_state
            .set_serial_number(&output, super::output_profiles::serial_number(&output));
        self.outputs.push(ManagedOutput {
            key,
            id,
            output: output.clone(),
            enabled: true,
            reported: None,
            reported_usable: None,
            lock_surface: None,
            global: Some(global),
            powered: true,
            redraw_needed: true,
            frame_pending: false,
        });
        self.output_management_state.update::<State>();
        self.output_changed(&output);
        self.announce_lids();
        self.queue_output_profile();
    }

    pub fn remove_output(&mut self, output: &Output) {
        let Some(index) = self.outputs.iter().position(|o| &o.output == output) else {
            return;
        };
        let managed = self.outputs.remove(index);
        self.output_management_state.remove_head(output);
        self.output_power_state.output_removed(output);
        self.gamma_control_state.output_removed(output);
        self.screencopy_state.output_removed(output);
        self.image_capture_state.output_removed(output);
        self.output_management_state.update::<State>();
        self.retire_output(output, managed.global);
        self.context.borrow_mut().destroy_output(managed.id);
        if self.default_layer_output == Some(managed.key) {
            self.default_layer_output = None;
        }
        self.update_x11_settings();
        self.schedule_wm();
        self.announce_lids();
        self.queue_output_profile();
    }

    /// Let clients know `output`, no longer among the active outputs, is
    /// going away: its layer surfaces close, surfaces leave it and taskbars
    /// stop listing windows on it, all before its global goes. A client
    /// that destroys its `wl_output` as the global goes gets anything that
    /// names it afterwards with a null output, which e.g. kitty crashes on.
    /// The global is disabled at once and removed a moment later, so a
    /// client binding it meanwhile isn't killed for binding a global that
    /// is gone.
    pub fn retire_output(&mut self, output: &Output, global: Option<GlobalId>) {
        let layers: Vec<_> = layer_map_for_output(output).layers().cloned().collect();
        for layer in layers {
            layer.layer_surface().send_close();
            layer_map_for_output(output).unmap_layer(&layer);
        }
        for window in &self.windows {
            window.window.output_leave(output);
        }
        self.update_wlr_toplevels();
        let Some(global) = global else {
            return;
        };
        self.display_handle.disable_global::<State>(global.clone());
        let timer = Timer::from_duration(RETIRED_GLOBAL_LINGER);
        let inserted = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.display_handle.remove_global::<State>(global.clone());
            TimeoutAction::Drop
        });
        if let Err(err) = inserted {
            eprintln!(
                "canoe: cannot remove the global of {}: {err}",
                output.name()
            );
        }
    }

    /// Report an output's position, size and scale after they changed.
    pub fn output_changed(&mut self, output: &Output) {
        let Some(geometry) = self.output_geometry(output) else {
            return;
        };
        let Some(managed) = self.outputs.iter_mut().find(|o| &o.output == output) else {
            return;
        };
        let (id, key) = (managed.id, managed.key);
        let rect = (
            geometry.loc.x,
            geometry.loc.y,
            geometry.size.w,
            geometry.size.h,
        );
        let previous = managed.reported.replace(rect);
        let scale = output.current_scale().fractional_scale();
        let canoe_output = self.context.borrow().outputs.get(&id).cloned();
        let Some(canoe_output) = canoe_output else {
            return;
        };
        {
            let mut out = canoe_output.borrow_mut();
            out.name = Some(output.name());
            out.scale = scale.ceil().max(1.0) as i32;
            out.preferred_scale =
                Some((scale * canoe::scale::FRACTIONAL_DENOMINATOR as f64).round() as u32);
        }
        match previous {
            None => {
                let mut out = canoe_output.borrow_mut();
                out.update_position(rect.0, rect.1);
                out.update_dimensions(rect.2, rect.3);
            }
            Some(old) => {
                let (dx, dy) = (rect.0 - old.0, rect.1 - old.1);
                canoe_output.borrow_mut().update_position(rect.0, rect.1);
                if dx != 0 || dy != 0 {
                    self.context
                        .borrow_mut()
                        .pending_output_shifts
                        .push((id, dx, dy));
                }
                canoe_output.borrow_mut().update_dimensions(rect.2, rect.3);
                if (old.2, old.3) != (rect.2, rect.3) {
                    self.context
                        .borrow_mut()
                        .pending_output_resizes
                        .push((id, old.2, old.3));
                }
            }
        }
        {
            let mut out = canoe_output.borrow_mut();
            if let Some(desktop) = out.desktop_surface.as_mut() {
                desktop.dirty = true;
            }
            if let Some(background) = out.desktop_background.as_mut() {
                background.dirty = true;
            }
        }
        let _ = key;
        ensure_desktop_surfaces(self, id);
        configure_canoe_layers(self, id);
        layer_map_for_output(output).arrange();
        self.layers_changed(output);
        self.update_surface_outputs();
        self.update_x11_settings();
        self.schedule_wm();
    }

    /// The layer surfaces of `output` changed: report the area left for
    /// windows and re-evaluate keyboard focus.
    pub fn layers_changed(&mut self, output: &Output) {
        let Some(geometry) = self.output_geometry(output) else {
            return;
        };
        let zone = layer_map_for_output(output).non_exclusive_zone();
        let usable = (
            geometry.loc.x + zone.loc.x,
            geometry.loc.y + zone.loc.y,
            zone.size.w,
            zone.size.h,
        );
        let Some(managed) = self.outputs.iter_mut().find(|o| &o.output == output) else {
            return;
        };
        if managed.reported_usable != Some(usable) {
            managed.reported_usable = Some(usable);
            let id = managed.id;
            let mut context = self.context.borrow_mut();
            if let Some(out) = context.outputs.get(&id).cloned() {
                let mut out = out.borrow_mut();
                out.update_non_exclusive_area(usable.0, usable.1, usable.2, usable.3);
                // The desktop surface's input region leaves exclusive zones
                // out; it is redrawn at the end of the next manage sequence.
                if let Some(desktop) = out.desktop_surface.as_mut() {
                    desktop.dirty = true;
                }
                // Snapped and maximized windows follow at the next manage
                // sequence.
                context.pending_usable_area_changes.push(id);
            }
            drop(context);
            self.schedule_wm();
        }
        self.update_layer_focus();
    }

    /// Tell surfaces which outputs they are on, and at what scale.
    pub fn update_surface_outputs(&mut self) {
        let outputs: Vec<(Output, Rectangle<i32, Logical>)> = self
            .active_outputs()
            .filter_map(|o| Some((o.output.clone(), self.output_geometry(&o.output)?)))
            .collect();
        for window in &self.windows {
            let geo = window.window.geometry();
            let bbox = Rectangle::new(
                window.pos - geo.loc + window.window.bbox().loc,
                window.window.bbox().size,
            );
            let mut primary: Option<(Output, i32)> = None;
            for (output, area) in &outputs {
                match bbox.intersection(*area) {
                    Some(overlap) if !window.hidden => {
                        let overlap_local = Rectangle::new(overlap.loc - area.loc, overlap.size);
                        window.window.output_enter(output, overlap_local);
                        let size = overlap.size.w * overlap.size.h;
                        if primary.as_ref().is_none_or(|(_, best)| size > *best) {
                            primary = Some((output.clone(), size));
                        }
                    }
                    _ => window.window.output_leave(output),
                }
            }
            if let Some((output, _)) = primary {
                send_scale(window.wl_surface(), &output);
            }
        }
        for (output, _) in &outputs {
            let map = layer_map_for_output(output);
            for layer in map.layers() {
                send_scale(layer.wl_surface(), output);
            }
        }
    }

    // ------------------------------------------------------------------
    // Session lock

    pub fn session_locked_changed(&mut self, locked: bool) {
        if locked {
            // Nothing gets shared from behind the lock screen. The lock
            // binding mode below replaces the picker's.
            finish_share_picker(self, false, false);
            for seat in self.context.borrow().seats.values() {
                seat.borrow_mut().unhandled_actions.retain(|action| {
                    !matches!(
                        action,
                        binding::Action::SwitchMode {
                            mode: config::Mode::SharePicker
                        }
                    )
                });
            }
            self.context.borrow_mut().session_locked = true;
            if let Some(seat_id) = self.context.borrow().current_seat {
                if let Some(seat) = self.context.borrow().seats.get(&seat_id) {
                    seat.borrow_mut().switch_mode(config::Mode::Lock);
                }
            }
        } else {
            self.context.borrow_mut().handle_session_unlocked();
            if let Some(seat_id) = self.context.borrow().current_seat {
                if let Some(seat) = self.context.borrow().seats.get(&seat_id) {
                    seat.borrow_mut().switch_mode(config::Mode::Default);
                }
            }
            // River clears keyboard focus on unlock; the window manager
            // sends it again in the next manage sequence.
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.set_focus(self, None, Serial::from(0));
        }
        self.schedule_wm();
        self.queue_redraw_all();
    }

    // ------------------------------------------------------------------
    // Seat events

    fn wm_seat(&self) -> Option<Rc<std::cell::RefCell<canoe::Seat>>> {
        let context = self.context.borrow();
        context
            .current_seat
            .and_then(|id| context.seats.get(&id).cloned())
    }

    /// river_seat_v1.pointer_enter
    pub fn wm_pointer_enter(&mut self, window_id: canoe::WindowId) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let seat_id = seat.borrow().id;
        let (weak, sloppy_focus) = {
            let context = self.context.borrow();
            let weak = context.windows.get(&window_id).map(Rc::downgrade);
            (weak, context.config.sloppy_focus)
        };
        if let Some(weak) = weak {
            seat.borrow_mut().window_below_pointer = Some(weak);
            if sloppy_focus {
                self.context.borrow_mut().focus(window_id);
            }
            self.context.borrow_mut().update_cursor_for_seat(seat_id);
        }
        self.schedule_wm();
    }

    /// river_seat_v1.pointer_leave
    pub fn wm_pointer_leave(&mut self) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let seat_id = seat.borrow().id;
        seat.borrow_mut().window_below_pointer = None;
        self.context.borrow_mut().update_cursor_for_seat(seat_id);
        self.schedule_wm();
    }

    /// river_seat_v1.window_interaction: a click on a window or its frame.
    pub fn wm_window_interaction(&mut self, window_id: canoe::WindowId, button: u32) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let seat_id = seat.borrow().id;
        if !self.context.borrow().windows.contains_key(&window_id) {
            return;
        }
        self.context.borrow_mut().close_window_menu();
        {
            let mut context = self.context.borrow_mut();
            if context.icon_focus_output.is_some() {
                context.exit_icon_focus(seat_id);
            }
            context.focus(window_id);
            context.handle_window_interaction(seat_id, window_id, button);
        }
        // A click elsewhere takes the keyboard from an on-demand layer
        // surface.
        if matches!(self.layer_focus, Some((_, false))) {
            self.release_layer_focus();
        }
        self.schedule_wm();
    }

    /// river_seat_v1.op_delta
    pub fn wm_op_delta(&mut self, dx: i32, dy: i32) {
        let pointer = self
            .wm_seat()
            .map(|seat| {
                let seat = seat.borrow();
                (seat.pointer_x, seat.pointer_y)
            })
            .unwrap_or_default();
        {
            let context = self.context.borrow();
            if let Some(wid) = context.focused_window {
                if let Some(window) = context.windows.get(&wid) {
                    let mut w = window.borrow_mut();
                    if (dx, dy) != (0, 0) {
                        if let Some(anchor) = w.take_move_restore() {
                            context.restore_for_move(
                                &mut w,
                                anchor,
                                pointer.0 - dx,
                                pointer.1 - dy,
                            );
                        }
                    }
                    w.apply_op_delta(dx, dy);
                }
            }
        }
        self.schedule_wm();
    }

    /// river_seat_v1.op_release
    pub fn wm_op_release(&mut self) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let seat_id = seat.borrow().id;
        let (window_id, was_move) = {
            let context = self.context.borrow();
            match context
                .focused_window
                .and_then(|wid| context.windows.get(&wid).map(|w| (wid, w)))
            {
                Some((wid, window)) => {
                    let mut w = window.borrow_mut();
                    let was_move = matches!(w.operator, canoe::window::Operator::Move { .. });
                    w.end_operation();
                    (Some(wid), was_move)
                }
                None => (None, false),
            }
        };
        seat.borrow().end_pointer_op();
        if was_move {
            if let Some(window_id) = window_id {
                self.context
                    .borrow_mut()
                    .update_window_output_from_position(window_id);
            }
        }
        self.context.borrow_mut().update_cursor_for_seat(seat_id);
        self.schedule_wm();
    }

    /// river_seat_v1.pointer_position
    pub fn wm_pointer_position(&mut self, x: i32, y: i32) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let seat_id = seat.borrow().id;
        {
            let seat_ref = seat.borrow();
            if (seat_ref.pointer_x, seat_ref.pointer_y) == (x, y) {
                return;
            }
        }
        seat.borrow_mut().update_pointer_position(x, y);
        self.context
            .borrow_mut()
            .update_window_output_from_pointer(seat_id, x, y);
        self.context.borrow_mut().update_cursor_for_seat(seat_id);
        update_menu_hover_from_global(self, seat_id);
        let titlebar_window =
            self.context
                .borrow()
                .windows
                .iter()
                .find_map(|(&window_id, window)| {
                    window.borrow().titlebar_left_down.then_some(window_id)
                });
        if let Some(window_id) = titlebar_window {
            if update_titlebar_hover_from_global(self, window_id, x, y) {
                self.schedule_wm();
            }
        }
    }

    // ------------------------------------------------------------------
    // Bindings

    /// river_xkb_binding_v1.pressed
    pub fn xkb_binding_pressed(&mut self, index: usize) {
        self.xkb_binding_event(index, true);
    }

    /// river_xkb_binding_v1.released
    pub fn xkb_binding_released(&mut self, index: usize) {
        self.xkb_binding_event(index, false);
    }

    fn xkb_binding_event(&mut self, index: usize, pressed: bool) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let seat_id = seat.borrow().id;
        let (action, binding_event, enabled) = {
            let seat_ref = seat.borrow();
            let Some(binding) = seat_ref.xkb_bindings.get(index) else {
                return;
            };
            (binding.action.clone(), binding.event, binding.enabled)
        };

        // While the window switcher menu is open (Super+Tab), ignore every key
        // except the ones that drive the menu.
        let alt_tab_active =
            self.context.borrow().window_menu_mode == Some(canoe::WindowMenuMode::AltTab);

        if pressed {
            if !enabled || binding_event != binding::BindingEvent::Pressed {
                return;
            }
            if alt_tab_active
                && !matches!(
                    action,
                    binding::Action::WindowMenuCycle { .. }
                        | binding::Action::WindowMenuCycleApp
                        | binding::Action::WindowMenuCancel
                )
            {
                return;
            }
            match action {
                binding::Action::WindowMenuCycle { direction } => {
                    handle_window_menu_cycle(self, direction);
                }
                binding::Action::WindowMenuCycleApp => {
                    handle_window_menu_cycle_app(self);
                }
                binding::Action::IconSelectNext
                | binding::Action::IconSelectPrev
                | binding::Action::IconSelectUp
                | binding::Action::IconSelectDown => {
                    self.context.borrow_mut().execute_action(action, seat_id);
                    render_all_desktop_surfaces(self);
                }
                binding::Action::IconActivate => {
                    self.context.borrow_mut().execute_action(action, seat_id);
                    render_all_desktop_surfaces(self);
                    self.schedule_wm();
                }
                binding::Action::IconCancel => {
                    self.context.borrow_mut().execute_action(action, seat_id);
                    render_all_desktop_surfaces(self);
                }
                binding::Action::SharePickerConfirm => {
                    finish_share_picker(self, true, true);
                }
                binding::Action::SharePickerCancel => {
                    finish_share_picker(self, false, true);
                }
                binding::Action::SharePickerNext | binding::Action::SharePickerPrev => {
                    let step = if matches!(action, binding::Action::SharePickerNext) {
                        1
                    } else {
                        -1
                    };
                    if let Some(picker) = self.context.borrow_mut().share_picker.as_mut() {
                        picker.cycle(step);
                    }
                    render_share_picker(self);
                }
                _ => {
                    seat.borrow_mut().queue_action(action);
                    self.schedule_wm();
                }
            }
        } else {
            if !enabled || binding_event != binding::BindingEvent::Released {
                return;
            }
            if alt_tab_active && !matches!(action, binding::Action::WindowMenuCommit) {
                return;
            }
            match action {
                binding::Action::WindowMenuCommit => {
                    handle_window_menu_commit(self, seat_id);
                }
                _ => {
                    seat.borrow_mut().queue_action(action);
                }
            }
            self.schedule_wm();
        }
        self.queue_redraw_all();
    }

    /// river_pointer_binding_v1.pressed/released
    pub fn pointer_binding_event(&mut self, index: usize, pressed: bool) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let mut seat_ref = seat.borrow_mut();
        if let Some(binding) = seat_ref.pointer_bindings.get(index) {
            let wanted = if pressed {
                binding::BindingEvent::Pressed
            } else {
                binding::BindingEvent::Released
            };
            if binding.enabled && binding.event == wanted {
                let action = binding.action.clone();
                seat_ref.queue_action(action);
            }
        }
        drop(seat_ref);
        self.schedule_wm();
    }

    /// Whether a key press matches one of the window manager's bindings:
    /// either with the key's unmodified keysyms and all modifiers, or with
    /// the keysyms xkb translated and the modifiers it didn't consume (what
    /// River does).
    pub fn match_xkb_binding(
        &self,
        raw_syms: &[u32],
        translated_syms: &[u32],
        modifiers: u32,
        consumed: u32,
    ) -> Option<usize> {
        let seat = self.wm_seat()?;
        let seat = seat.borrow();
        let matches = |syms: &[u32], mods: u32| {
            seat.xkb_bindings.iter().position(|binding| {
                binding.enabled && binding.modifiers == mods && syms.contains(&binding.keysym)
            })
        };
        matches(raw_syms, modifiers).or_else(|| matches(translated_syms, modifiers & !consumed))
    }

    pub fn match_pointer_binding(&self, button: u32, modifiers: u32) -> Option<usize> {
        let seat = self.wm_seat()?;
        let seat = seat.borrow();
        seat.pointer_bindings.iter().position(|binding| {
            binding.enabled && binding.button == button && binding.modifiers == modifiers
        })
    }

    // ------------------------------------------------------------------
    // Pointer input on the window manager's own surfaces

    /// The pointer entered one of the window manager's surfaces at
    /// `(surface_x, surface_y)` (wl_pointer.enter).
    pub fn canoe_pointer_enter(
        &mut self,
        target: canoe::PointerTarget,
        surface_x: f64,
        surface_y: f64,
    ) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let seat_id = seat.borrow().id;
        let surface_pos = (surface_x.round() as i32, surface_y.round() as i32);
        {
            let mut seat_ref = seat.borrow_mut();
            seat_ref.pointer_target = target;
            seat_ref.last_surface_x = surface_pos.0;
            seat_ref.last_surface_y = surface_pos.1;
            seat_ref.cursor_shape = None;
        }
        if target == canoe::PointerTarget::Menu {
            let changed = self
                .context
                .borrow_mut()
                .update_menu_hover(surface_pos.0, surface_pos.1);
            if changed {
                render_window_menu(self);
            }
        }
        if let canoe::PointerTarget::Desktop(output_id)
        | canoe::PointerTarget::MenuShield(output_id)
        | canoe::PointerTarget::SharePicker(output_id) = target
        {
            update_pointer_position_from_output(self, &seat, output_id, surface_x, surface_y);
        }
        if matches!(target, canoe::PointerTarget::SharePicker(_)) {
            update_share_picker_hover(self, &seat);
        }
        if matches!(target, canoe::PointerTarget::MenuShield(_)) {
            seat.borrow_mut().hide_cursor();
        } else {
            self.context.borrow_mut().update_cursor_for_seat(seat_id);
        }
        if let canoe::PointerTarget::Titlebar(window_id) = target {
            if update_titlebar_hover_from_surface(self, window_id, surface_x, surface_y) {
                self.schedule_wm();
            }
        }
    }

    /// wl_pointer.leave on one of the window manager's surfaces.
    pub fn canoe_pointer_leave(&mut self) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let prev_target = seat.borrow().pointer_target;
        if let canoe::PointerTarget::Titlebar(window_id) = prev_target {
            if clear_titlebar_state(self, window_id) {
                self.schedule_wm();
            }
        }
        let mut seat = seat.borrow_mut();
        seat.pointer_target = canoe::PointerTarget::None;
        seat.cursor_shape = None;
    }

    /// wl_pointer.motion on one of the window manager's surfaces.
    pub fn canoe_pointer_motion(&mut self, surface_x: f64, surface_y: f64) {
        let Some(seat) = self.wm_seat() else {
            return;
        };
        let seat_id = seat.borrow().id;
        {
            let mut seat_ref = seat.borrow_mut();
            seat_ref.last_surface_x = surface_x.round() as i32;
            seat_ref.last_surface_y = surface_y.round() as i32;
        }
        let target = seat.borrow().pointer_target;
        if let canoe::PointerTarget::SharePicker(output_id) = target {
            update_pointer_position_from_output(self, &seat, output_id, surface_x, surface_y);
            update_share_picker_hover(self, &seat);
        } else if target == canoe::PointerTarget::Menu {
            let changed = self
                .context
                .borrow_mut()
                .update_menu_hover(surface_x.round() as i32, surface_y.round() as i32);
            if changed {
                render_window_menu(self);
            }
        } else if let canoe::PointerTarget::Desktop(output_id)
        | canoe::PointerTarget::MenuShield(output_id) = target
        {
            update_pointer_position_from_output(self, &seat, output_id, surface_x, surface_y);
            update_menu_hover_from_surface(self, output_id, surface_x, surface_y);
            if matches!(target, canoe::PointerTarget::Desktop(_)) {
                self.context.borrow_mut().update_cursor_for_seat(seat_id);
            }
        } else if let canoe::PointerTarget::Titlebar(window_id) = target {
            if update_titlebar_hover_from_surface(self, window_id, surface_x, surface_y) {
                self.schedule_wm();
            }
        }
    }

    /// wl_pointer.button on one of the window manager's surfaces.
    pub fn canoe_pointer_button(&mut self, button: u32, pressed: bool) {
        self.canoe_pointer_button_inner(button, pressed);
        self.schedule_wm();
        self.queue_redraw_all();
    }

    fn canoe_pointer_button_inner(&mut self, button: u32, pressed: bool) {
        let state = self;
        let Some(seat) = state.wm_seat() else {
            return;
        };
        let seat_id = seat.borrow().id;
        let target = seat.borrow().pointer_target;
        if matches!(target, canoe::PointerTarget::MenuShield(_)) {
            return;
        }
        // The picker overlay takes the whole click, so act on the
        // release: a left click shares, a right click declines.
        if matches!(target, canoe::PointerTarget::SharePicker(_)) {
            if !pressed {
                if button == config::button::LEFT {
                    finish_share_picker(state, true, true);
                } else if button == config::button::RIGHT {
                    finish_share_picker(state, false, true);
                }
            }
            return;
        }
        match pressed {
            true => {
                if target != canoe::PointerTarget::Menu {
                    let had_menu = {
                        let mut context = state.context.borrow_mut();
                        if context.window_menu.is_some() {
                            context.close_window_menu();
                            true
                        } else {
                            false
                        }
                    };
                    if had_menu {
                        seat.borrow_mut().menu_click_button = None;
                        if matches!(target, canoe::PointerTarget::Desktop(_)) {
                            seat.borrow_mut().queue_action(binding::Action::ClearFocus);
                        }
                        return;
                    }
                }
                match target {
                    canoe::PointerTarget::Desktop(output_id) => {
                        let (px, py) = {
                            let seat_ref = seat.borrow();
                            (seat_ref.last_surface_x, seat_ref.last_surface_y)
                        };
                        if button == config::button::LEFT {
                            // Hit-test desktop icons
                            let hit = {
                                let context = state.context.borrow();
                                context.outputs.get(&output_id).and_then(|o| {
                                    o.borrow()
                                        .desktop_surface
                                        .as_ref()
                                        .and_then(|d| d.icon_at(px, py))
                                })
                            };
                            if let Some(icon_window_id) = hit {
                                // Check for double-click to restore
                                let now = Instant::now();
                                let is_double = {
                                    let seat_ref = seat.borrow();
                                    seat_ref
                                        .last_icon_click
                                        .map(|(last_wid, when)| {
                                            last_wid == icon_window_id
                                                && now.duration_since(when) <= canoe::DOUBLE_CLICK
                                        })
                                        .unwrap_or(false)
                                };
                                seat.borrow_mut().last_icon_click = Some((icon_window_id, now));
                                if is_double {
                                    // Defer through Action::IconActivate so focus_window
                                    // (window-management state) runs inside a manage
                                    // sequence; the preceding single click already
                                    // selected this icon via enter_icon_focus.
                                    let mut seat_mut = seat.borrow_mut();
                                    seat_mut.last_icon_click = None;
                                    seat_mut.queue_action(binding::Action::IconActivate);
                                    drop(seat_mut);
                                } else {
                                    // Single click: select icon and enter icon focus mode
                                    state.context.borrow_mut().enter_icon_focus(
                                        output_id,
                                        icon_window_id,
                                        seat_id,
                                    );
                                    render_desktop_surface(state, output_id);
                                }
                            } else {
                                // Clicked empty space: clear local icon-mode state
                                // synchronously, then queue the protocol-bound mode
                                // switch + clear-focus to run inside the next manage
                                // sequence (this is a wl_pointer event, not part of
                                // any river manage sequence).
                                seat.borrow_mut().last_icon_click = None;
                                {
                                    let mut context = state.context.borrow_mut();
                                    if let Some(focused_output) = context.icon_focus_output.take() {
                                        if let Some(output) = context.outputs.get(&focused_output) {
                                            if let Some(desktop) =
                                                output.borrow_mut().desktop_surface.as_mut()
                                            {
                                                desktop.selected_icon = None;
                                                desktop.dirty = true;
                                            }
                                        }
                                    }
                                }
                                render_desktop_surface(state, output_id);
                                {
                                    let mut seat_mut = seat.borrow_mut();
                                    seat_mut.queue_action(binding::Action::ClearFocus);
                                    seat_mut.queue_action(binding::Action::SwitchMode {
                                        mode: config::Mode::Default,
                                    });
                                }
                            }
                        } else if button == config::button::RIGHT {
                            let icon_hit = {
                                let context = state.context.borrow();
                                context.outputs.get(&output_id).and_then(|o| {
                                    o.borrow()
                                        .desktop_surface
                                        .as_ref()
                                        .and_then(|d| d.icon_at(px, py))
                                })
                            };
                            if let Some(icon_window_id) = icon_hit {
                                // Right-click on an icon: just select it, no menu.
                                seat.borrow_mut().last_icon_click = None;
                                state.context.borrow_mut().enter_icon_focus(
                                    output_id,
                                    icon_window_id,
                                    seat_id,
                                );
                                render_desktop_surface(state, output_id);
                            } else {
                                if state.context.borrow().config.ui.desktop_menu {
                                    let mut context = state.context.borrow_mut();
                                    if context.window_menu.is_some() {
                                        context.close_window_menu();
                                    }
                                    drop(context);
                                    open_window_menu(
                                        state,
                                        output_id,
                                        px,
                                        py,
                                        false,
                                        canoe::WindowMenuMode::Pointer,
                                        Some("Windows".to_string()),
                                        None,
                                    );
                                    update_menu_hover_from_global(state, seat_id);
                                    seat.borrow_mut().menu_click_button = Some(button);
                                    seat.borrow_mut().queue_action(binding::Action::ClearFocus);
                                } else {
                                    seat.borrow_mut().queue_action(binding::Action::ClearFocus);
                                }
                            }
                        } else {
                            seat.borrow_mut().queue_action(binding::Action::ClearFocus);
                        }
                    }
                    canoe::PointerTarget::Menu => {
                        seat.borrow_mut().menu_click_button = Some(button);
                    }
                    canoe::PointerTarget::MenuShield(_) | canoe::PointerTarget::SharePicker(_) => {}
                    canoe::PointerTarget::Titlebar(window_id) => {
                        if button == config::button::LEFT {
                            let (px, py) = {
                                let seat_ref = seat.borrow();
                                (seat_ref.last_surface_x, seat_ref.last_surface_y)
                            };
                            update_titlebar_hover_from_surface(
                                state, window_id, px as f64, py as f64,
                            );
                            let should_render = {
                                let context = state.context.borrow();
                                if let Some(window) = context.windows.get(&window_id) {
                                    let mut w = window.borrow_mut();
                                    w.titlebar_left_down = true;
                                    w.titlebar_pressed = w.titlebar_hovered;
                                    true
                                } else {
                                    false
                                }
                            };
                            if should_render {}
                        }
                    }
                    canoe::PointerTarget::None => {}
                }
            }
            false => {
                if let canoe::PointerTarget::Titlebar(window_id) = target {
                    if button == config::button::LEFT {
                        let (px, py) = {
                            let seat_ref = seat.borrow();
                            (seat_ref.last_surface_x, seat_ref.last_surface_y)
                        };
                        update_titlebar_hover_from_surface(state, window_id, px as f64, py as f64);
                        let action = {
                            let context = state.context.borrow();
                            let Some(window) = context.windows.get(&window_id) else {
                                return;
                            };
                            let mut w = window.borrow_mut();
                            w.titlebar_left_down = false;
                            let hovered = w.titlebar_hovered;
                            let pressed = w.titlebar_pressed.take();
                            if pressed.is_some() && pressed == hovered {
                                pressed
                            } else {
                                None
                            }
                        };

                        match action {
                            Some(canoe::titlebar::TitlebarButton::Close) => {
                                let now = Instant::now();
                                let should_close = {
                                    let mut seat_ref = seat.borrow_mut();
                                    let is_double = seat_ref
                                        .last_close_click
                                        .map(|(last_window, when)| {
                                            last_window == window_id
                                                && now.duration_since(when) <= canoe::DOUBLE_CLICK
                                        })
                                        .unwrap_or(false);
                                    seat_ref.last_close_click = Some((window_id, now));
                                    is_double
                                };
                                if should_close {
                                    if let Some(window) =
                                        state.context.borrow().windows.get(&window_id)
                                    {
                                        window.borrow_mut().queue_event(canoe::WindowEvent::Close);
                                    }
                                }
                            }
                            Some(canoe::titlebar::TitlebarButton::Hide) => {
                                seat.borrow_mut().last_close_click = None;
                                if let Some(window) = state.context.borrow().windows.get(&window_id)
                                {
                                    window
                                        .borrow_mut()
                                        .queue_event(canoe::WindowEvent::Minimize);
                                }
                            }
                            Some(canoe::titlebar::TitlebarButton::Maximize) => {
                                seat.borrow_mut().last_close_click = None;
                                if let Some(window) = state.context.borrow().windows.get(&window_id)
                                {
                                    if window.borrow().maximized {
                                        window
                                            .borrow_mut()
                                            .queue_event(canoe::WindowEvent::Unmaximize);
                                    } else {
                                        window
                                            .borrow_mut()
                                            .queue_event(canoe::WindowEvent::Maximize);
                                    }
                                }
                            }
                            None => {
                                seat.borrow_mut().last_close_click = None;
                            }
                        }
                    }
                    return;
                }
                let (activate, close_menu) = {
                    let seat_ref = seat.borrow();
                    if seat_ref.menu_click_button != Some(button) {
                        (false, false)
                    } else {
                        let context = state.context.borrow();
                        let hovered = context
                            .window_menu
                            .as_ref()
                            .and_then(|menu| menu.hovered)
                            .is_some();
                        if context.window_menu_mode == Some(canoe::WindowMenuMode::Pointer) {
                            if hovered {
                                (true, false)
                            } else {
                                (false, false)
                            }
                        } else {
                            (false, false)
                        }
                    }
                };
                seat.borrow_mut().menu_click_button = None;
                if activate {
                    seat.borrow_mut()
                        .queue_action(binding::Action::ActivateMenuHovered);
                    return;
                }
                if close_menu {
                    state.context.borrow_mut().close_window_menu();
                }
            }
        }
    }
}

/// The app id and title a window set (for X11 windows, their class and
/// title).
pub(super) fn shell_properties(shell: &Shell) -> (Option<String>, Option<String>) {
    match shell {
        Shell::Xdg(toplevel) => with_states(toplevel.wl_surface(), |states| {
            let data = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap();
            (data.app_id.clone(), data.title.clone())
        }),
        Shell::X11(x11) => {
            let non_empty = |s: String| (!s.is_empty()).then_some(s);
            (non_empty(x11.class()), non_empty(x11.title()))
        }
    }
}

/// Tell a surface tree the scale of the output it's on.
fn send_scale(surface: &WlSurface, output: &Output) {
    let scale = output.current_scale();
    let transform = output.current_transform();
    with_surface_tree_downward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |surface, states, _| {
            send_surface_state(surface, states, scale.integer_scale(), transform);
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale.fractional_scale());
            });
        },
        |_, _, _| true,
    );
}
