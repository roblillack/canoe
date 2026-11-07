//! Input: key and button bindings, pointer focus, and interactive
//! operations, routed the way River does for canoe's window manager.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::desktop::{layer_map_for_output, WindowSurfaceType};
use smithay::input::keyboard::{FilterResult, Keycode, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};
use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer};

use super::{KeyConsumer, LockState, State};
use crate::canoe::{self, PointerTarget};
use crate::config::modifiers;

/// A relative motion: delta, unaccelerated delta, timestamp in µs.
pub type RelativeMotion = (Point<f64, Logical>, Point<f64, Logical>, u64);

/// What is under the pointer.
#[derive(Debug, Clone)]
pub enum Hit {
    /// A client's surface, with the global position of its origin.
    Client {
        surface: WlSurface,
        origin: Point<f64, Logical>,
        window: Option<canoe::WindowId>,
        layer: Option<WlSurface>,
    },
    /// One of the window manager's surfaces, with the pointer's position on
    /// it.
    Canoe {
        target: PointerTarget,
        local: Point<f64, Logical>,
        window: Option<canoe::WindowId>,
    },
    Nothing,
}

impl Hit {
    fn window(&self) -> Option<canoe::WindowId> {
        match self {
            Hit::Client { window, .. } | Hit::Canoe { window, .. } => *window,
            Hit::Nothing => None,
        }
    }
}

/// River's modifier bits for smithay's modifier state.
pub fn modifier_mask(mods: &ModifiersState) -> u32 {
    let mut mask = 0;
    if mods.shift {
        mask |= modifiers::SHIFT;
    }
    if mods.ctrl {
        mask |= modifiers::CTRL;
    }
    if mods.alt {
        mask |= modifiers::ALT;
    }
    if mods.logo {
        mask |= modifiers::SUPER;
    }
    if mods.iso_level3_shift {
        mask |= modifiers::MOD5;
    }
    mask
}

/// What a key press turned out to be.
enum KeyAction {
    Binding(usize, bool),
    SwitchVt(i32),
}

impl State {
    pub fn process_input_event<B: InputBackend>(&mut self, event: InputEvent<B>) {
        match event {
            InputEvent::Keyboard { event } => {
                let pressed = event.state() == KeyState::Pressed;
                self.on_key(event.key_code(), pressed, event.time_msec());
            }
            InputEvent::PointerMotion { event } => {
                let location = self.pointer_location + event.delta();
                self.on_pointer_motion(
                    location,
                    event.time_msec(),
                    Some((event.delta(), event.delta_unaccel(), event.time())),
                );
            }
            InputEvent::PointerMotionAbsolute { event } => {
                // Absolute devices (the nested window, tablets) map onto
                // the first output.
                let Some(area) = self
                    .active_outputs()
                    .next()
                    .and_then(|o| self.output_geometry(&o.output))
                else {
                    return;
                };
                let location = event.position_transformed(area.size) + area.loc.to_f64();
                self.on_pointer_motion(location, event.time_msec(), None);
            }
            InputEvent::PointerButton { event } => {
                let pressed = event.state() == ButtonState::Pressed;
                self.on_pointer_button(event.button_code(), pressed, event.time_msec());
            }
            InputEvent::PointerAxis { event } => {
                let source = event.source();
                let factor = self.context.borrow().config.scroll_factor;
                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let amount = factor
                        * event.amount(axis).unwrap_or_else(|| {
                            event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0
                        });
                    if amount != 0.0 {
                        frame = frame.value(axis, amount);
                        if let Some(v120) = event.amount_v120(axis) {
                            frame = frame.v120(axis, v120 as i32);
                        }
                    } else if source == AxisSource::Finger {
                        frame = frame.stop(axis);
                    }
                }
                self.on_pointer_axis(frame);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Keyboard

    pub fn on_key(&mut self, keycode: Keycode, pressed: bool, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        // River matches bindings against the modifiers as they were before
        // this key changed them, so a binding on a modifier key itself (the
        // window switcher's commit on releasing Super) fires.
        let previous_mods = self.key_modifiers;
        let state_key = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        self.idle_notifier_state.notify_activity(&self.seat);

        let action = keyboard.input(
            self,
            keycode,
            state_key,
            serial,
            time,
            |state, _mods, handle| {
                if pressed {
                    let raw: Vec<u32> = handle.raw_syms().iter().map(|s| s.raw()).collect();
                    let translated: Vec<u32> =
                        handle.modified_syms().iter().map(|s| s.raw()).collect();
                    for sym in &translated {
                        let vt = sym.wrapping_sub(xkbcommon::xkb::keysyms::KEY_XF86Switch_VT_1);
                        if vt < 12 {
                            state
                                .key_consumers
                                .insert(keycode, KeyConsumer::Binding(None));
                            return FilterResult::Intercept(Some(KeyAction::SwitchVt(
                                vt as i32 + 1,
                            )));
                        }
                    }
                    // Shift is what translation usually consumes.
                    let consumed = if raw != translated {
                        modifiers::SHIFT
                    } else {
                        0
                    };
                    if let Some(index) =
                        state.match_xkb_binding(&raw, &translated, previous_mods, consumed)
                    {
                        state
                            .key_consumers
                            .insert(keycode, KeyConsumer::Binding(Some(index)));
                        return FilterResult::Intercept(Some(KeyAction::Binding(index, true)));
                    }
                    state.key_consumers.insert(keycode, KeyConsumer::Client);
                    FilterResult::Forward
                } else {
                    match state.key_consumers.remove(&keycode) {
                        Some(KeyConsumer::Binding(Some(index))) => {
                            FilterResult::Intercept(Some(KeyAction::Binding(index, false)))
                        }
                        Some(KeyConsumer::Binding(None)) => FilterResult::Intercept(None),
                        // Releases without a press are dropped, as River does.
                        None => FilterResult::Intercept(None),
                        Some(KeyConsumer::Client) => FilterResult::Forward,
                    }
                }
            },
        );
        self.key_modifiers = modifier_mask(&keyboard.modifier_state());

        match action {
            Some(Some(KeyAction::Binding(index, true))) => self.xkb_binding_pressed(index),
            Some(Some(KeyAction::Binding(index, false))) => self.xkb_binding_released(index),
            Some(Some(KeyAction::SwitchVt(vt))) => super::backend::Backend::switch_vt(self, vt),
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Pointer

    /// Keep `location` within the output layout.
    fn clamp_to_outputs(&self, location: Point<f64, Logical>) -> Point<f64, Logical> {
        let areas: Vec<Rectangle<i32, Logical>> = self
            .active_outputs()
            .filter_map(|o| self.output_geometry(&o.output))
            .collect();
        if areas.is_empty() || areas.iter().any(|a| a.to_f64().contains(location)) {
            return location;
        }
        // Closest point on any output.
        let mut best = location;
        let mut best_distance = f64::MAX;
        for area in areas {
            let x = location
                .x
                .clamp(area.loc.x as f64, (area.loc.x + area.size.w) as f64 - 1.0);
            let y = location
                .y
                .clamp(area.loc.y as f64, (area.loc.y + area.size.h) as f64 - 1.0);
            let distance = (x - location.x).powi(2) + (y - location.y).powi(2);
            if distance < best_distance {
                best_distance = distance;
                best = (x, y).into();
            }
        }
        best
    }

    pub fn on_pointer_motion(
        &mut self,
        location: Point<f64, Logical>,
        time: u32,
        relative: Option<RelativeMotion>,
    ) {
        let location = self.clamp_to_outputs(location);
        self.pointer_location = location;
        self.idle_notifier_state.notify_activity(&self.seat);
        let pointer = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();

        if let Some(op) = self.pointer_op.as_ref() {
            let delta = location - op.start;
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location,
                    serial,
                    time,
                },
            );
            pointer.frame(self);
            self.wm_pointer_position(location.x.round() as i32, location.y.round() as i32);
            self.wm_op_delta(delta.x.round() as i32, delta.y.round() as i32);
            self.queue_redraw_all();
            return;
        }

        let focus = self.update_pointer_target(location);
        pointer.motion(
            self,
            focus.clone(),
            &MotionEvent {
                location,
                serial,
                time,
            },
        );
        if let Some((delta, delta_unaccel, utime)) = relative {
            pointer.relative_motion(
                self,
                focus,
                &RelativeMotionEvent {
                    delta,
                    delta_unaccel,
                    utime,
                },
            );
        }
        pointer.frame(self);
        self.wm_pointer_position(location.x.round() as i32, location.y.round() as i32);
        self.queue_redraw_all();
    }

    /// Work out what the pointer is over, tell the window manager about
    /// windows and its own surfaces the pointer entered or left, and return
    /// the client surface that gets pointer events.
    fn update_pointer_target(
        &mut self,
        location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        // A button held on one of the window manager's surfaces keeps
        // sending it the pointer's motion (an implicit grab).
        let hit = match self.canoe_grab {
            Some((target, window)) => match self.canoe_surface_origin(target) {
                Some(origin) => Hit::Canoe {
                    target,
                    local: location - origin,
                    window,
                },
                None => self.hit_test(location),
            },
            None => self.hit_test(location),
        };

        let window = hit.window();
        if window != self.pointer_window {
            if self.pointer_window.is_some() {
                self.wm_pointer_leave();
            }
            self.pointer_window = window;
            if let Some(id) = window {
                self.wm_pointer_enter(id);
            }
        }

        match hit {
            Hit::Canoe { target, local, .. } => {
                if target != self.pointer_canoe_target {
                    if self.pointer_canoe_target != PointerTarget::None {
                        self.canoe_pointer_leave();
                    }
                    self.pointer_canoe_target = target;
                    self.canoe_pointer_enter(target, local.x, local.y);
                } else {
                    self.canoe_pointer_motion(local.x, local.y);
                }
                None
            }
            Hit::Client {
                surface, origin, ..
            } => {
                self.leave_canoe_target();
                Some((surface, origin))
            }
            Hit::Nothing => {
                self.leave_canoe_target();
                None
            }
        }
    }

    pub fn leave_canoe_target(&mut self) {
        if self.pointer_canoe_target != PointerTarget::None {
            self.canoe_pointer_leave();
            self.pointer_canoe_target = PointerTarget::None;
        }
    }

    /// Take pointer focus from clients, for the start of a pointer
    /// operation.
    pub fn clear_client_pointer_focus(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        let location = self.pointer_location;
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: self.start_time.elapsed().as_millis() as u32,
            },
        );
        pointer.frame(self);
    }

    /// Give pointer focus to what is under the pointer again.
    pub fn refresh_pointer_focus(&mut self) {
        let location = self.pointer_location;
        let time = self.start_time.elapsed().as_millis() as u32;
        self.on_pointer_motion(location, time, None);
    }

    pub fn on_pointer_button(&mut self, button: u32, pressed: bool, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        self.idle_notifier_state.notify_activity(&self.seat);
        if pressed {
            if !self.pressed_buttons.contains(&button) {
                self.pressed_buttons.push(button);
            }
        } else {
            self.pressed_buttons.retain(|&b| b != button);
        }
        let event = ButtonEvent {
            button,
            state: if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
            serial,
            time,
        };

        if !pressed {
            if let Some(index) = self.button_consumers.remove(&button) {
                self.pointer_binding_event(index, false);
                self.check_op_release();
                return;
            }
        }

        if self.pointer_op.is_some() {
            self.check_op_release();
            if self.pressed_buttons.is_empty() {
                self.canoe_grab = None;
            }
            return;
        }

        if pressed {
            // The window manager's pointer bindings come first.
            if self.lock_state == LockState::Unlocked && !pointer.is_grabbed() {
                if let Some(index) = self.match_pointer_binding(button, self.key_modifiers) {
                    self.button_consumers.insert(button, index);
                    self.pointer_binding_event(index, true);
                    return;
                }
            }

            let hit = match self.canoe_grab {
                Some((target, window)) => Hit::Canoe {
                    target,
                    local: (0.0, 0.0).into(),
                    window,
                },
                None => self.hit_test(self.pointer_location),
            };
            if !pointer.is_grabbed() {
                if let Some(window) = hit.window() {
                    self.wm_window_interaction(window);
                }
            }
            match hit {
                Hit::Canoe { target, window, .. } if !pointer.is_grabbed() => {
                    self.canoe_grab = Some((target, window));
                    self.canoe_pointer_button(button, true);
                    return;
                }
                Hit::Client { ref layer, .. } => {
                    if let Some(layer) = layer.clone() {
                        if self.layer_interactivity(&layer) == Some(KeyboardInteractivity::OnDemand)
                        {
                            self.focus_layer_on_demand(&layer);
                        }
                    }
                }
                _ => {}
            }
        } else if self.canoe_grab.is_some() {
            if self.pressed_buttons.is_empty() {
                self.canoe_grab = None;
            }
            self.canoe_pointer_button(button, false);
            if self.pressed_buttons.is_empty() {
                // The surface under the pointer may have changed while the
                // button was held.
                self.refresh_pointer_focus();
            }
            return;
        }

        pointer.button(self, &event);
        pointer.frame(self);
    }

    /// All buttons are up during a pointer operation: tell the window
    /// manager once.
    fn check_op_release(&mut self) {
        let release = match self.pointer_op.as_mut() {
            Some(op) if !op.released && self.pressed_buttons.is_empty() => {
                op.released = true;
                true
            }
            _ => false,
        };
        if release {
            self.wm_op_release();
        }
    }

    pub fn on_pointer_axis(&mut self, frame: AxisFrame) {
        if self.pointer_op.is_some() || self.canoe_grab.is_some() {
            return;
        }
        if self.pointer_canoe_target != PointerTarget::None {
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    // ------------------------------------------------------------------
    // Hit testing

    fn layer_interactivity(&self, surface: &WlSurface) -> Option<KeyboardInteractivity> {
        self.outputs.iter().find_map(|o| {
            let map = layer_map_for_output(&o.output);
            let layer = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)?;
            Some(layer.cached_state().keyboard_interactivity)
        })
    }

    /// Where one of the window manager's surfaces has its origin, globally.
    pub fn canoe_surface_origin(&self, target: PointerTarget) -> Option<Point<f64, Logical>> {
        let context = self.context.borrow();
        let output_origin = |id: canoe::OutputId| {
            let out = context.outputs.get(&id)?;
            let out = out.borrow();
            Some(Point::<i32, Logical>::from((out.x, out.y)))
        };
        let origin = match target {
            PointerTarget::None => return None,
            PointerTarget::Desktop(id)
            | PointerTarget::MenuShield(id)
            | PointerTarget::SharePicker(id) => output_origin(id)?,
            PointerTarget::Menu => {
                let menu = context.window_menu.as_ref()?;
                output_origin(menu.output_id)?
                    + Point::<i32, Logical>::from((menu.origin_x, menu.origin_y))
            }
            PointerTarget::Titlebar(window_id) => {
                let window = self.window_by_id(window_id)?;
                let titlebar_offset = {
                    let w = context.windows.get(&window_id)?.borrow();
                    match w.titlebar.as_ref()?.surface.role() {
                        canoe::surface::Role::Decoration { offset, .. } => offset,
                        _ => return None,
                    }
                };
                window.pos + Point::<i32, Logical>::from(titlebar_offset)
            }
        };
        Some(origin.to_f64())
    }

    /// What is at `location`, topmost first.
    pub fn hit_test(&self, location: Point<f64, Logical>) -> Hit {
        let Some(output) = self.output_at(location) else {
            return Hit::Nothing;
        };
        let Some(managed) = self.managed_output(&output) else {
            return Hit::Nothing;
        };
        let output_id = managed.id;
        let output_geo = self.output_geometry(&output).unwrap();
        let output_loc = output_geo.loc.to_f64();

        if self.lock_state != LockState::Unlocked {
            return match managed.lock_surface.as_ref() {
                Some(lock) => Hit::Client {
                    surface: lock.wl_surface().clone(),
                    origin: output_loc,
                    window: None,
                    layer: None,
                },
                None => Hit::Nothing,
            };
        }

        // The window manager's overlays: the picker, the switcher's shield
        // and the menu.
        {
            let context = self.context.borrow();
            let local = location - output_loc;
            let (lx, ly) = (local.x.floor() as i32, local.y.floor() as i32);
            if let Some(picker) = context.share_picker.as_ref() {
                if let Some(surface) = picker.surfaces.iter().find(|s| s.output_id == output_id) {
                    if surface.surface.accepts_input_at(lx, ly) {
                        return Hit::Canoe {
                            target: PointerTarget::SharePicker(output_id),
                            local,
                            window: None,
                        };
                    }
                }
            }
            if let Some(shield) = context.window_menu_shield.as_ref() {
                if shield.output_id == output_id && shield.surface.accepts_input_at(lx, ly) {
                    return Hit::Canoe {
                        target: PointerTarget::MenuShield(output_id),
                        local,
                        window: None,
                    };
                }
            }
            if let Some(menu) = context.window_menu.as_ref() {
                if menu.output_id == output_id {
                    let menu_local = local
                        - Point::<f64, Logical>::from((menu.origin_x as f64, menu.origin_y as f64));
                    if menu
                        .surface
                        .accepts_input_at(menu_local.x.floor() as i32, menu_local.y.floor() as i32)
                    {
                        return Hit::Canoe {
                            target: PointerTarget::Menu,
                            local: menu_local,
                            window: None,
                        };
                    }
                }
            }
        }

        if let Some(hit) = self.layer_hit(&output, WlrLayer::Overlay, location) {
            return hit;
        }

        // A fullscreen window covers everything below the overlay layer.
        let fullscreen = self
            .windows
            .iter()
            .rev()
            .find(|w| !w.hidden && w.fullscreen == Some(managed.key));
        if let Some(window) = fullscreen {
            let origin = window.surface_origin(Some(output_geo.loc)).to_f64();
            return match window
                .window
                .surface_under(location - origin, WindowSurfaceType::ALL)
            {
                Some((surface, loc)) => Hit::Client {
                    surface,
                    origin: origin + loc.to_f64(),
                    window: Some(window.id),
                    layer: None,
                },
                None => Hit::Nothing,
            };
        }

        if let Some(hit) = self.layer_hit(&output, WlrLayer::Top, location) {
            return hit;
        }

        for window in self.windows.iter().rev() {
            if window.hidden {
                continue;
            }
            if let Some(hit) = self.window_hit(window, location) {
                return hit;
            }
        }

        if let Some(hit) = self.layer_hit(&output, WlrLayer::Bottom, location) {
            return hit;
        }

        // The desktop (icons, and the empty desktop's clicks).
        {
            let context = self.context.borrow();
            if let Some(out) = context.outputs.get(&output_id) {
                let out = out.borrow();
                if let Some(desktop) = out.desktop_surface.as_ref() {
                    let local = location - output_loc;
                    if desktop
                        .surface
                        .accepts_input_at(local.x.floor() as i32, local.y.floor() as i32)
                    {
                        return Hit::Canoe {
                            target: PointerTarget::Desktop(output_id),
                            local,
                            window: None,
                        };
                    }
                }
            }
        }

        if let Some(hit) = self.layer_hit(&output, WlrLayer::Background, location) {
            return hit;
        }
        Hit::Nothing
    }

    fn layer_hit(
        &self,
        output: &Output,
        layer: WlrLayer,
        location: Point<f64, Logical>,
    ) -> Option<Hit> {
        let output_loc = self.output_geometry(output)?.loc;
        let map = layer_map_for_output(output);
        let local = location - output_loc.to_f64();
        let surface = map.layer_under(layer, local)?;
        let geometry = map.layer_geometry(surface)?;
        let (wl_surface, loc) =
            surface.surface_under(local - geometry.loc.to_f64(), WindowSurfaceType::ALL)?;
        Some(Hit::Client {
            surface: wl_surface,
            origin: (output_loc + geometry.loc + loc).to_f64(),
            window: None,
            layer: Some(surface.wl_surface().clone()),
        })
    }

    /// The part of a window (popups, frame, content) at `location`.
    fn window_hit(
        &self,
        window: &super::ManagedWindow,
        location: Point<f64, Logical>,
    ) -> Option<Hit> {
        let origin = window.surface_origin(None).to_f64();
        // Popups are above everything else of the window.
        if let Some((surface, loc)) = window
            .window
            .surface_under(
                location - origin,
                WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE,
            )
            .filter(|(surface, _)| surface != window.wl_surface())
        {
            if !self.is_toplevel_subsurface(window, &surface) {
                return Some(Hit::Client {
                    surface,
                    origin: origin + loc.to_f64(),
                    window: Some(window.id),
                    layer: None,
                });
            }
        }

        // The frame drawn by the window manager.
        if window.ssd {
            let context = self.context.borrow();
            if let Some(w) = context.windows.get(&window.id) {
                let w = w.borrow();
                if let Some(titlebar) = w.titlebar.as_ref() {
                    if let canoe::surface::Role::Decoration { offset, .. } = titlebar.surface.role()
                    {
                        let frame_origin =
                            (window.pos + Point::<i32, Logical>::from(offset)).to_f64();
                        let local = location - frame_origin;
                        if titlebar
                            .surface
                            .accepts_input_at(local.x.floor() as i32, local.y.floor() as i32)
                        {
                            return Some(Hit::Canoe {
                                target: PointerTarget::Titlebar(window.id),
                                local,
                                window: Some(window.id),
                            });
                        }
                    }
                }
            }
        }

        // The content, minus what the window manager clipped away.
        let content = Rectangle::new(window.pos, window.window.geometry().size);
        let clip = window.content_clip;
        let visible = if clip.is_enabled() {
            Rectangle::new(
                window.pos + Point::<i32, Logical>::from((clip.x, clip.y)),
                smithay::utils::Size::<i32, Logical>::from((clip.width, clip.height)),
            )
        } else {
            content
        };
        if !visible.to_f64().contains(location) && clip.is_enabled() {
            return None;
        }
        let (surface, loc) = window.window.surface_under(
            location - origin,
            WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE,
        )?;
        Some(Hit::Client {
            surface,
            origin: origin + loc.to_f64(),
            window: Some(window.id),
            layer: None,
        })
    }

    /// Whether `surface` belongs to the toplevel's own surface tree (as
    /// opposed to a popup's).
    fn is_toplevel_subsurface(&self, window: &super::ManagedWindow, surface: &WlSurface) -> bool {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        &root == window.wl_surface()
    }
}
