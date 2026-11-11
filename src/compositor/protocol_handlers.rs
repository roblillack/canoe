//! Handlers for the protocols canoe implements itself.

use smithay::backend::input::ButtonState;
use smithay::output::{Mode, Scale};

use super::protocols::gamma_control::{
    delegate_gamma_control, GammaControlHandler, GammaControlManagerState,
};
use super::protocols::output_management::{
    delegate_output_management, HeadConfiguration, ModeConfiguration, OutputManagementHandler,
    OutputManagementState,
};
use super::protocols::output_power::{
    delegate_output_power, Mode as PowerMode, OutputPowerHandler, OutputPowerManagerState,
};
use super::protocols::plasma_shell::{delegate_plasma_shell, PlasmaShellHandler};
use super::protocols::toplevel_icon::delegate_toplevel_icon;
use super::protocols::virtual_pointer::{
    delegate_virtual_pointer, VirtualPointer, VirtualPointerEvent, VirtualPointerHandler,
};
use super::protocols::xdg_foreign::{delegate_xdg_foreign, XdgForeignHandler, XdgForeignState};
use super::State;
use crate::wm::OutputHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

impl VirtualPointerHandler for State {
    fn virtual_pointer_event(
        &mut self,
        pointer: &VirtualPointer<Self>,
        event: VirtualPointerEvent,
    ) {
        match event {
            VirtualPointerEvent::Motion { time, delta } => {
                let location = self.pointer_location + delta;
                self.on_pointer_motion(location, time, Some((delta, delta, time as u64 * 1000)));
            }
            VirtualPointerEvent::MotionAbsolute(motion) => {
                // Over the bound output, else the whole layout.
                let area = match motion.output.clone().or_else(|| pointer.output()) {
                    Some(output) => self.output_geometry(&output),
                    None => self.layout_bounds(),
                };
                if let Some(area) = area {
                    let location = motion.position_in(area);
                    self.on_pointer_motion(location, motion.time, None);
                }
            }
            VirtualPointerEvent::Button {
                time,
                button,
                state,
            } => {
                self.on_pointer_button(button, state == ButtonState::Pressed, time);
            }
            VirtualPointerEvent::Axis(frame) => self.on_pointer_axis(frame),
            VirtualPointerEvent::Frame => {}
        }
    }

    fn virtual_pointer_destroyed(&mut self, _pointer: &VirtualPointer<Self>) {
        // Don't leave buttons stuck down.
        let time = self.start_time.elapsed().as_millis() as u32;
        for button in self.pressed_buttons.clone() {
            self.on_pointer_button(button, false, time);
        }
    }
}

impl State {
    /// The bounding box of all outputs.
    pub fn layout_bounds(&self) -> Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>> {
        self.active_outputs()
            .filter_map(|o| self.output_geometry(&o.output))
            .reduce(|a, b| a.merge(b))
    }

    /// Take an output out of the layout, or back in. For the window manager
    /// this is the same as unplugging it and plugging it in again.
    pub fn set_output_enabled(&mut self, output: &smithay::output::Output, enabled: bool) {
        let Some(index) = self.outputs.iter().position(|o| &o.output == output) else {
            return;
        };
        if self.outputs[index].enabled == enabled {
            return;
        }
        eprintln!(
            "canoe: {} {}",
            output.name(),
            if enabled { "enabled" } else { "disabled" }
        );
        self.outputs[index].enabled = enabled;
        self.output_management_state.set_enabled(output, enabled);
        if enabled {
            // Enabling an output turns it on.
            self.outputs[index].powered = true;
            self.outputs[index].frame_pending = false;
            let key = self.outputs[index].key;
            let handle = OutputHandle::new(key, self.requests.clone());
            let id = self.context.borrow_mut().create_output(handle).borrow().id;
            self.outputs[index].id = id;
            self.outputs[index].reported = None;
            self.outputs[index].reported_usable = None;
            self.outputs[index].global = Some(output.create_global::<State>(&self.display_handle));
            self.output_changed(output);
        } else {
            self.output_power_state.output_removed(output);
            // The output keeps its CRTC: don't leave a client's ramps on it.
            self.gamma_control_state.output_removed(output);
            super::backend::Backend::set_gamma(self, output, None);
            let id = self.outputs[index].id;
            let global = self.outputs[index].global.take();
            self.retire_output(output, global);
            self.context.borrow_mut().destroy_output(id);
            self.update_x11_settings();
            self.schedule_wm();
        }
        super::backend::Backend::update_output_power(self, output);
    }

    /// Turn an enabled output's monitor off or back on. It stays in the
    /// layout; while off nothing is drawn on it.
    pub fn set_output_power(&mut self, output: &smithay::output::Output, on: bool) {
        let Some(managed) = self.outputs.iter_mut().find(|o| &o.output == output) else {
            return;
        };
        if !managed.enabled || managed.powered == on {
            return;
        }
        eprintln!(
            "canoe: {} powered {}",
            output.name(),
            if on { "on" } else { "off" }
        );
        managed.powered = on;
        // A frame queued before blanking never completes.
        managed.frame_pending = false;
        super::backend::Backend::update_output_power(self, output);
        let mode = if on { PowerMode::On } else { PowerMode::Off };
        self.output_power_state.mode_changed(output, mode);
    }
}

impl GammaControlHandler for State {
    fn gamma_control_state(&mut self) -> &mut GammaControlManagerState {
        &mut self.gamma_control_state
    }

    fn gamma_size(&self, output: &smithay::output::Output) -> Option<u32> {
        self.managed_output(output).filter(|o| o.enabled)?;
        super::backend::Backend::gamma_size(self, output)
    }

    fn set_gamma(&mut self, output: &smithay::output::Output, ramps: Option<&[u16]>) -> bool {
        super::backend::Backend::set_gamma(self, output, ramps)
    }
}

impl XdgForeignHandler for State {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.xdg_foreign_state
    }
}

impl OutputPowerHandler for State {
    fn output_power_state(&mut self) -> &mut OutputPowerManagerState {
        &mut self.output_power_state
    }

    fn output_power_mode(&self, output: &smithay::output::Output) -> Option<PowerMode> {
        let managed = self.managed_output(output).filter(|o| o.enabled)?;
        Some(if managed.powered {
            PowerMode::On
        } else {
            PowerMode::Off
        })
    }

    fn set_output_power_mode(&mut self, output: &smithay::output::Output, mode: PowerMode) -> bool {
        self.set_output_power(output, mode == PowerMode::On);
        true
    }

    /// Turning on an output that is on already starts it over, in case it
    /// went dark without canoe noticing.
    fn output_power_mode_reasserted(&mut self, output: &smithay::output::Output, mode: PowerMode) {
        if mode == PowerMode::On {
            eprintln!("canoe: {} powered on again", output.name());
            super::backend::Backend::restart_output(self, output);
        }
    }
}

impl OutputManagementHandler for State {
    fn output_management_state(&mut self) -> &mut OutputManagementState {
        &mut self.output_management_state
    }

    fn apply_output_configuration(
        &mut self,
        config: Vec<HeadConfiguration>,
        test_only: bool,
    ) -> bool {
        // Leave at least one output on.
        if !config.iter().any(|head| head.enabled) {
            return false;
        }
        for head in &config {
            if let Some(scale) = head.scale {
                if !(0.25..=8.0).contains(&scale) {
                    return false;
                }
            }
            if let Some(mode) = head.mode {
                let mode = match mode {
                    ModeConfiguration::Mode(mode) => mode,
                    ModeConfiguration::Custom { size, refresh } => Mode {
                        size,
                        refresh: refresh.unwrap_or(60_000),
                    },
                };
                if !super::backend::Backend::can_set_mode(self, &head.output, mode) {
                    return false;
                }
            }
        }
        if test_only {
            return true;
        }

        // Move the layout so it starts at 0,0 at the least: Xwayland's
        // screen starts there, and the pointer can't reach X11 windows on
        // parts of outputs above or left of it.
        let (min_x, min_y) = config
            .iter()
            .filter(|head| head.enabled)
            .map(|head| {
                head.position
                    .unwrap_or_else(|| head.output.current_location())
            })
            .fold((0, 0), |(x, y), pos| (x.min(pos.x), y.min(pos.y)));
        let shift = smithay::utils::Point::from((-min_x, -min_y));

        for head in config {
            let output = head.output.clone();
            if !head.enabled {
                self.set_output_enabled(&output, false);
                continue;
            }
            if let Some(mode) = head.mode {
                let mode = match mode {
                    ModeConfiguration::Mode(mode) => mode,
                    ModeConfiguration::Custom { size, refresh } => Mode {
                        size,
                        refresh: refresh.unwrap_or(60_000),
                    },
                };
                if !super::backend::Backend::set_mode(self, &output, mode) {
                    return false;
                }
            }
            output.change_current_state(
                None,
                head.transform,
                head.scale.map(Scale::Fractional),
                Some(head.position.unwrap_or_else(|| output.current_location()) + shift),
            );
            self.set_output_enabled(&output, true);
            self.output_changed(&output);
        }
        self.queue_redraw_all();
        true
    }
}

delegate_virtual_pointer!(State);
delegate_output_management!(State);
delegate_output_power!(State);
delegate_gamma_control!(State);
delegate_xdg_foreign!(State);
delegate_toplevel_icon!(State);
delegate_plasma_shell!(State);

impl PlasmaShellHandler for State {
    fn skip_switcher_changed(&mut self, surface: &WlSurface, skip: bool) {
        self.toplevel_skip_switcher_changed(surface, skip);
    }
}
