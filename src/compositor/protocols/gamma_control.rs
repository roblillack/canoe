//! Server side of `wlr-gamma-control-unstable-v1`
//! (`zwlr_gamma_control_manager_v1`, version 1).
//!
//! This lets night-light tools such as wlsunset or gammastep set the gamma
//! ramps of outputs.
//!
//! # How it fits into the compositor
//!
//! A client asks for a gamma control of a `wl_output` and is told the
//! size of the output's gamma ramps right away. The compositor decides
//! which outputs have gamma ramps ([`GammaControlHandler::gamma_size`]
//! returns `None` for the others, and the client gets `failed`) and
//! applies the ramps ([`GammaControlHandler::set_gamma`]).
//!
//! Each output has at most one control: a client asking for a second one
//! while the first is alive gets `failed`. When the control is destroyed,
//! or fails because setting a ramp didn't work, the output's ramps are
//! reset (`set_gamma` with `None`).
//!
//! The compositor calls [`GammaControlManagerState::output_removed`] when
//! an output leaves the layout (unplugged or disabled): its control gets
//! `failed` and becomes inert, as the protocol asks. Resetting the
//! output's ramps is then up to the compositor.
//!
//! ```ignore
//! impl GammaControlHandler for State {
//!     fn gamma_control_state(&mut self) -> &mut GammaControlManagerState {
//!         &mut self.gamma_control_state
//!     }
//!     fn gamma_size(&self, output: &Output) -> Option<u32> { ... }
//!     fn set_gamma(&mut self, output: &Output, ramps: Option<&[u16]>) -> bool { ... }
//! }
//! delegate_gamma_control!(State);
//!
//! let gamma_control_state = GammaControlManagerState::new::<State>(&display_handle);
//! ```

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};

use smithay::output::{Output, WeakOutput};
use smithay::reexports::rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
use smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

const VERSION: u32 = 1;

/// The compositor's side of gamma control.
pub trait GammaControlHandler:
    GlobalDispatch<ZwlrGammaControlManagerV1, GammaControlGlobalData>
    + Dispatch<ZwlrGammaControlManagerV1, ()>
    + Dispatch<ZwlrGammaControlV1, GammaControlData>
    + 'static
{
    /// Returns the protocol state.
    fn gamma_control_state(&mut self) -> &mut GammaControlManagerState;

    /// The number of entries in each of `output`'s gamma ramps, or `None`
    /// if its gamma can't be set (for instance because it isn't part of the
    /// layout).
    fn gamma_size(&self, output: &Output) -> Option<u32>;

    /// Apply gamma ramps to `output`: the red, green and blue ramps one
    /// after another, each [`gamma_size`](Self::gamma_size) entries long.
    /// `None` restores the original (linear) ramps. Returns whether that
    /// worked.
    fn set_gamma(&mut self, output: &Output, ramps: Option<&[u16]>) -> bool;
}

/// Global data of the manager.
pub struct GammaControlGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

/// Data of a gamma control: the output it controls, the size of that
/// output's ramps, and whether it was already told it failed (after which
/// it ignores requests).
pub struct GammaControlData {
    output: WeakOutput,
    size: u32,
    failed: AtomicBool,
}

impl GammaControlData {
    fn output(&self) -> Option<Output> {
        if self.failed.load(Ordering::Relaxed) {
            return None;
        }
        self.output.upgrade()
    }
}

/// State of the gamma control protocol.
pub struct GammaControlManagerState {
    global: GlobalId,
    /// Live controls that haven't failed, at most one per output.
    controls: Vec<ZwlrGammaControlV1>,
}

impl GammaControlManagerState {
    /// Create the `zwlr_gamma_control_manager_v1` global, visible to all
    /// clients.
    pub fn new<D: GammaControlHandler>(display: &DisplayHandle) -> Self {
        Self::new_with_filter::<D, _>(display, |_| true)
    }

    /// Create the global, visible to the clients `filter` accepts.
    pub fn new_with_filter<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GammaControlHandler,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global = display.create_global::<D, ZwlrGammaControlManagerV1, _>(
            VERSION,
            GammaControlGlobalData {
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

    /// `output` left the layout: its control fails. Returns whether it had
    /// one.
    pub fn output_removed(&mut self, output: &Output) -> bool {
        let before = self.controls.len();
        self.controls.retain(|control| {
            if !controls_output(control, output) {
                return true;
            }
            fail(control);
            false
        });
        self.controls.len() != before
    }

    fn has_control(&self, output: &Output) -> bool {
        self.controls.iter().any(|c| controls_output(c, output))
    }

    fn forget(&mut self, control: &ZwlrGammaControlV1) {
        self.controls.retain(|c| c != control);
    }
}

fn controls_output(control: &ZwlrGammaControlV1, output: &Output) -> bool {
    control
        .data::<GammaControlData>()
        .and_then(|data| data.output())
        .is_some_and(|o| &o == output)
}

/// Send `failed`; the control ignores requests from now on.
fn fail(control: &ZwlrGammaControlV1) {
    if let Some(data) = control.data::<GammaControlData>() {
        if data.failed.swap(true, Ordering::Relaxed) {
            return;
        }
    }
    control.failed();
}

/// Why a client's gamma table couldn't be read.
enum ReadError {
    /// The file holds fewer bytes than the ramps need: a protocol error.
    TooShort,
    /// Not readable (a pipe, say): the control fails.
    Io(std::io::Error),
}

/// Read `size` entries for each of the three ramps from the start of `fd`,
/// without blocking.
fn read_ramps(fd: OwnedFd, size: u32) -> Result<Vec<u16>, ReadError> {
    let flags = fcntl_getfl(&fd).map_err(|e| ReadError::Io(e.into()))?;
    fcntl_setfl(&fd, flags | OFlags::NONBLOCK).map_err(|e| ReadError::Io(e.into()))?;
    let mut bytes = vec![0u8; size as usize * 3 * 2];
    match File::from(fd).read_exact_at(&mut bytes, 0) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(ReadError::TooShort)
        }
        Err(err) => return Err(ReadError::Io(err)),
    }
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| u16::from_ne_bytes(b))
        .collect())
}

impl<D: GammaControlHandler> GlobalDispatch<ZwlrGammaControlManagerV1, GammaControlGlobalData, D>
    for GammaControlManagerState
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _global_data: &GammaControlGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, global_data: &GammaControlGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D: GammaControlHandler> Dispatch<ZwlrGammaControlManagerV1, (), D>
    for GammaControlManagerState
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } => {
                let output = Output::from_resource(&output);
                // Another client's control keeps the output.
                let size = output
                    .as_ref()
                    .filter(|o| !state.gamma_control_state().has_control(o))
                    .and_then(|o| state.gamma_size(o))
                    .filter(|&size| size > 0);
                let control = data_init.init(
                    id,
                    GammaControlData {
                        output: output.as_ref().map(Output::downgrade).unwrap_or_default(),
                        size: size.unwrap_or(0),
                        failed: AtomicBool::new(false),
                    },
                );
                match size {
                    Some(size) => {
                        control.gamma_size(size);
                        state.gamma_control_state().controls.push(control);
                    }
                    None => fail(&control),
                }
            }
            zwlr_gamma_control_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D: GammaControlHandler> Dispatch<ZwlrGammaControlV1, GammaControlData, D>
    for GammaControlManagerState
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        data: &GammaControlData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                // Inert after failing.
                let Some(output) = data.output() else {
                    return;
                };
                let ramps = match read_ramps(fd, data.size) {
                    Ok(ramps) => ramps,
                    Err(ReadError::TooShort) => {
                        resource.post_error(
                            zwlr_gamma_control_v1::Error::InvalidGamma,
                            format!("the gamma table must hold 3 ramps of {} entries", data.size),
                        );
                        return;
                    }
                    Err(ReadError::Io(err)) => {
                        eprintln!("canoe: reading a gamma table failed: {err}");
                        fail(resource);
                        state.gamma_control_state().forget(resource);
                        state.set_gamma(&output, None);
                        return;
                    }
                };
                if !state.set_gamma(&output, Some(&ramps)) {
                    fail(resource);
                    state.gamma_control_state().forget(resource);
                    state.set_gamma(&output, None);
                }
            }
            zwlr_gamma_control_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ZwlrGammaControlV1,
        data: &GammaControlData,
    ) {
        let gamma_state = state.gamma_control_state();
        let live = gamma_state.controls.contains(resource);
        gamma_state.forget(resource);
        // Still in charge of the output: restore its ramps.
        if live {
            if let Some(output) = data.output() {
                state.set_gamma(&output, None);
            }
        }
    }
}

macro_rules! delegate_gamma_control {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1:
                $crate::compositor::protocols::gamma_control::GammaControlGlobalData
        ] => $crate::compositor::protocols::gamma_control::GammaControlManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1: ()
        ] => $crate::compositor::protocols::gamma_control::GammaControlManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_v1::ZwlrGammaControlV1:
                $crate::compositor::protocols::gamma_control::GammaControlData
        ] => $crate::compositor::protocols::gamma_control::GammaControlManagerState);
    };
}
pub(crate) use delegate_gamma_control;
