//! Server side of `wlr-output-power-management-unstable-v1`
//! (`zwlr_output_power_manager_v1`, version 1).
//!
//! This lets tools such as `wlopm` (typically run by `swayidle`) turn
//! outputs off and on again without taking them out of the layout: windows
//! stay where they are, only the monitor goes dark.
//!
//! # How it fits into the compositor
//!
//! A client asks for a power control of a `wl_output` and is told the
//! output's current mode right away. The compositor decides which outputs
//! can be power managed ([`OutputPowerHandler::output_power_mode`] returns
//! `None` for the others, and the client gets `failed`) and carries out
//! mode changes ([`OutputPowerHandler::set_output_power_mode`]). A request
//! for the mode an output is already in goes to
//! [`OutputPowerHandler::output_power_mode_reasserted`], e.g. to make sure
//! an output that should be on really shows something.
//!
//! The compositor tells this module about changes:
//!
//! * [`OutputPowerManagerState::mode_changed`] whenever an output's mode
//!   changes, whether a client asked for it or not. Every control of that
//!   output gets a `mode` event.
//! * [`OutputPowerManagerState::output_removed`] when an output leaves the
//!   layout (unplugged or disabled). Its controls get `failed` and become
//!   inert, as the protocol asks.
//!
//! ```ignore
//! impl OutputPowerHandler for State {
//!     fn output_power_state(&mut self) -> &mut OutputPowerManagerState {
//!         &mut self.output_power_state
//!     }
//!     fn output_power_mode(&self, output: &Output) -> Option<Mode> { ... }
//!     fn set_output_power_mode(&mut self, output: &Output, mode: Mode) -> bool {
//!         // switch the output, then
//!         self.output_power_state.mode_changed(output, mode);
//!         true
//!     }
//! }
//! delegate_output_power!(State);
//!
//! let output_power_state = OutputPowerManagerState::new::<State>(&display_handle);
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::output::{Output, WeakOutput};
use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};
pub use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_v1::Mode;
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};

const VERSION: u32 = 1;

/// The compositor's side of output power management.
pub trait OutputPowerHandler:
    GlobalDispatch<ZwlrOutputPowerManagerV1, OutputPowerGlobalData>
    + Dispatch<ZwlrOutputPowerManagerV1, ()>
    + Dispatch<ZwlrOutputPowerV1, OutputPowerData>
    + 'static
{
    /// Returns the protocol state.
    fn output_power_state(&mut self) -> &mut OutputPowerManagerState;

    /// The current power mode of `output`, or `None` if it can't be power
    /// managed (for instance because it isn't part of the layout).
    fn output_power_mode(&self, output: &Output) -> Option<Mode>;

    /// A client asked to switch `output` to `mode`, which differs from its
    /// current mode. Returns whether that worked; on success the
    /// implementation reports the change with
    /// [`OutputPowerManagerState::mode_changed`].
    fn set_output_power_mode(&mut self, output: &Output, mode: Mode) -> bool;

    /// A client asked for the mode `output` is already in. The client is
    /// told the mode either way; by default nothing else happens.
    fn output_power_mode_reasserted(&mut self, _output: &Output, _mode: Mode) {}
}

/// Global data of the manager.
pub struct OutputPowerGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

/// Data of a power control: the output it controls, and whether it was
/// already told it failed (after which it ignores requests).
pub struct OutputPowerData {
    output: WeakOutput,
    failed: AtomicBool,
}

impl OutputPowerData {
    fn output(&self) -> Option<Output> {
        if self.failed.load(Ordering::Relaxed) {
            return None;
        }
        self.output.upgrade()
    }
}

/// State of the output power management protocol.
pub struct OutputPowerManagerState {
    global: GlobalId,
    /// Live controls that haven't failed.
    controls: Vec<ZwlrOutputPowerV1>,
}

impl OutputPowerManagerState {
    /// Create the `zwlr_output_power_manager_v1` global, visible to all
    /// clients.
    pub fn new<D: OutputPowerHandler>(display: &DisplayHandle) -> Self {
        Self::new_with_filter::<D, _>(display, |_| true)
    }

    /// Create the global, visible to the clients `filter` accepts.
    pub fn new_with_filter<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: OutputPowerHandler,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global = display.create_global::<D, ZwlrOutputPowerManagerV1, _>(
            VERSION,
            OutputPowerGlobalData {
                filter: Box::new(filter),
            },
        );
        Self {
            global,
            controls: Vec::new(),
        }
    }

    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// `output` is now in `mode`: tell every control of it.
    pub fn mode_changed(&mut self, output: &Output, mode: Mode) {
        for control in &self.controls {
            if controls_output(control, output) {
                control.mode(mode);
            }
        }
    }

    /// `output` left the layout: its controls fail.
    pub fn output_removed(&mut self, output: &Output) {
        self.controls.retain(|control| {
            if !controls_output(control, output) {
                return true;
            }
            fail(control);
            false
        });
    }
}

fn controls_output(control: &ZwlrOutputPowerV1, output: &Output) -> bool {
    control
        .data::<OutputPowerData>()
        .and_then(|data| data.output())
        .is_some_and(|o| &o == output)
}

/// Send `failed`; the control ignores requests from now on.
fn fail(control: &ZwlrOutputPowerV1) {
    if let Some(data) = control.data::<OutputPowerData>() {
        if data.failed.swap(true, Ordering::Relaxed) {
            return;
        }
    }
    control.failed();
}

impl<D: OutputPowerHandler> GlobalDispatch<ZwlrOutputPowerManagerV1, OutputPowerGlobalData, D>
    for OutputPowerManagerState
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        _global_data: &OutputPowerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, global_data: &OutputPowerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D: OutputPowerHandler> Dispatch<ZwlrOutputPowerManagerV1, (), D> for OutputPowerManagerState {
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } => {
                let output = Output::from_resource(&output);
                let mode = output.as_ref().and_then(|o| state.output_power_mode(o));
                let control = data_init.init(
                    id,
                    OutputPowerData {
                        output: output.as_ref().map(Output::downgrade).unwrap_or_default(),
                        failed: AtomicBool::new(false),
                    },
                );
                match mode {
                    Some(mode) => {
                        control.mode(mode);
                        state.output_power_state().controls.push(control);
                    }
                    None => fail(&control),
                }
            }
            zwlr_output_power_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D: OutputPowerHandler> Dispatch<ZwlrOutputPowerV1, OutputPowerData, D>
    for OutputPowerManagerState
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        data: &OutputPowerData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_power_v1::Request::SetMode { mode } => {
                let mode = match mode {
                    WEnum::Value(mode) => mode,
                    WEnum::Unknown(value) => {
                        resource.post_error(
                            zwlr_output_power_v1::Error::InvalidMode,
                            format!("unknown power mode {value}"),
                        );
                        return;
                    }
                };
                // Inert after failing.
                let Some(output) = data.output() else {
                    return;
                };
                match state.output_power_mode(&output) {
                    // Already there: confirm to the client that asked.
                    Some(current) if current == mode => {
                        state.output_power_mode_reasserted(&output, mode);
                        resource.mode(mode);
                    }
                    Some(_) if state.set_output_power_mode(&output, mode) => {}
                    _ => {
                        fail(resource);
                        state
                            .output_power_state()
                            .controls
                            .retain(|c| c != resource);
                    }
                }
            }
            zwlr_output_power_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ZwlrOutputPowerV1,
        _data: &OutputPowerData,
    ) {
        state
            .output_power_state()
            .controls
            .retain(|c| c != resource);
    }
}

macro_rules! delegate_output_power {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1:
                $crate::compositor::protocols::output_power::OutputPowerGlobalData
        ] => $crate::compositor::protocols::output_power::OutputPowerManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1: ()
        ] => $crate::compositor::protocols::output_power::OutputPowerManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_v1::ZwlrOutputPowerV1:
                $crate::compositor::protocols::output_power::OutputPowerData
        ] => $crate::compositor::protocols::output_power::OutputPowerManagerState);
    };
}
pub(crate) use delegate_output_power;
