//! Server side of `wlr-foreign-toplevel-management-unstable-v1`
//! (`zwlr_foreign_toplevel_manager_v1`, version 3).
//!
//! Taskbars (waybar's `wlr/taskbar`, sfwbar, nwg-panel and others) use it to
//! list the windows and their state, and to activate, minimize, maximize,
//! fullscreen or close them.
//!
//! # How it fits into the compositor
//!
//! The compositor creates a [`WlrToplevel`] for each window it shows with
//! [`ForeignToplevelManagerState::new_toplevel`], hands it the window's
//! current [`ToplevelInfo`] with [`WlrToplevel::update`] whenever something
//! may have changed (only the differences are sent), and ends it with
//! [`ForeignToplevelManagerState::close`]. It also calls
//! [`ForeignToplevelManagerState::output_bound`] when a client binds a
//! `wl_output`, so that client learns which windows are on it.
//!
//! Requests come in through
//! [`ForeignToplevelManagerHandler::toplevel_request`]. Whatever the
//! compositor does about them shows up in the next update.
//!
//! ```ignore
//! impl ForeignToplevelManagerHandler for State {
//!     fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState {
//!         &mut self.foreign_toplevel_manager_state
//!     }
//!     fn toplevel_request(&mut self, toplevel: &WlrToplevel, request: ToplevelRequest) { ... }
//! }
//! delegate_foreign_toplevel_manager!(State);
//!
//! let state = ForeignToplevelManagerState::new::<State>(&display_handle);
//! ```

use std::sync::{Arc, Mutex, Weak};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

const VERSION: u32 = 3;

/// The compositor's side of foreign toplevel management.
pub trait ForeignToplevelManagerHandler:
    GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData>
    + Dispatch<ZwlrForeignToplevelManagerV1, ()>
    + Dispatch<ZwlrForeignToplevelHandleV1, ToplevelData>
    + 'static
{
    /// Returns the protocol state.
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState;

    /// A client asked for something to happen to `toplevel`.
    fn toplevel_request(&mut self, toplevel: &WlrToplevel, request: ToplevelRequest);
}

/// What a client can ask for.
#[derive(Debug, Clone, PartialEq)]
pub enum ToplevelRequest {
    /// Focus the window (the seat is ignored: there is only one).
    Activate,
    Maximize,
    Unmaximize,
    Minimize,
    Unminimize,
    /// Fullscreen, on the output given as a hint.
    Fullscreen(Option<Output>),
    Unfullscreen,
    Close,
}

/// The states of a toplevel, as the protocol's `state` enum lists them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToplevelStates {
    pub maximized: bool,
    pub minimized: bool,
    pub activated: bool,
    pub fullscreen: bool,
}

impl ToplevelStates {
    /// The `state` event's array for a handle of `version`: native-endian
    /// u32 values, fullscreen only from version 2 on.
    fn to_array(self, version: u32) -> Vec<u8> {
        use zwlr_foreign_toplevel_handle_v1::State;
        let mut states = Vec::new();
        for (on, state) in [
            (self.maximized, State::Maximized),
            (self.minimized, State::Minimized),
            (self.activated, State::Activated),
            (self.fullscreen && version >= 2, State::Fullscreen),
        ] {
            if on {
                states.extend_from_slice(&(state as u32).to_ne_bytes());
            }
        }
        states
    }
}

/// What taskbars are told about a toplevel.
#[derive(Debug, Clone, Default)]
pub struct ToplevelInfo {
    pub title: String,
    pub app_id: String,
    pub states: ToplevelStates,
    /// The outputs the toplevel is on.
    pub outputs: Vec<Output>,
    pub parent: Option<WlrToplevel>,
}

/// One toplevel as this protocol shows it. Cheap to clone; clones are the
/// same toplevel.
#[derive(Debug, Clone)]
pub struct WlrToplevel(Arc<Mutex<Inner>>);

impl PartialEq for WlrToplevel {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Debug, Default)]
struct Inner {
    title: String,
    app_id: String,
    states: ToplevelStates,
    outputs: Vec<Output>,
    parent: Option<Weak<Mutex<Inner>>>,
    /// One handle per manager that was told about this toplevel.
    handles: Vec<ZwlrForeignToplevelHandleV1>,
    closed: bool,
}

/// Data of a `zwlr_foreign_toplevel_handle_v1`: the toplevel it stands for.
pub struct ToplevelData {
    toplevel: Weak<Mutex<Inner>>,
}

impl ToplevelData {
    /// The toplevel, unless it was closed (the handle is inert then).
    fn toplevel(&self) -> Option<WlrToplevel> {
        let inner = self.toplevel.upgrade()?;
        if inner.lock().unwrap().closed {
            return None;
        }
        Some(WlrToplevel(inner))
    }
}

/// Global data of the manager.
pub struct ForeignToplevelGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

/// State of the foreign toplevel management protocol.
pub struct ForeignToplevelManagerState {
    global: GlobalId,
    display: DisplayHandle,
    /// Managers that haven't stopped.
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    /// Open toplevels, oldest first.
    toplevels: Vec<WlrToplevel>,
}

impl ForeignToplevelManagerState {
    /// Create the `zwlr_foreign_toplevel_manager_v1` global, visible to all
    /// clients.
    pub fn new<D: ForeignToplevelManagerHandler>(display: &DisplayHandle) -> Self {
        Self::new_with_filter::<D, _>(display, |_| true)
    }

    /// Create the global, visible to the clients `filter` accepts.
    pub fn new_with_filter<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: ForeignToplevelManagerHandler,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global = display.create_global::<D, ZwlrForeignToplevelManagerV1, _>(
            VERSION,
            ForeignToplevelGlobalData {
                filter: Box::new(filter),
            },
        );
        Self {
            global,
            display: display.clone(),
            managers: Vec::new(),
            toplevels: Vec::new(),
        }
    }

    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// Announce a new toplevel to every manager.
    pub fn new_toplevel<D: ForeignToplevelManagerHandler>(
        &mut self,
        info: ToplevelInfo,
    ) -> WlrToplevel {
        let toplevel = WlrToplevel(Arc::new(Mutex::new(Inner::default())));
        toplevel.0.lock().unwrap().set(info);
        for manager in &self.managers {
            announce::<D>(&self.display, manager, &toplevel);
        }
        self.toplevels.push(toplevel.clone());
        toplevel
    }

    /// The toplevel is gone: its handles get `closed` and become inert.
    pub fn close(&mut self, toplevel: &WlrToplevel) {
        self.toplevels.retain(|t| t != toplevel);
        let mut inner = toplevel.0.lock().unwrap();
        if inner.closed {
            return;
        }
        inner.closed = true;
        for handle in inner.handles.drain(..) {
            handle.closed();
        }
    }

    /// A client bound `wl_output` for `output`: tell its handles of the
    /// toplevels on that output.
    pub fn output_bound(&mut self, output: &Output, wl_output: &WlOutput) {
        let Some(client) = wl_output.client() else {
            return;
        };
        for toplevel in &self.toplevels {
            let inner = toplevel.0.lock().unwrap();
            if !inner.outputs.contains(output) {
                continue;
            }
            for handle in &inner.handles {
                if handle.client().as_ref() == Some(&client) {
                    handle.output_enter(wl_output);
                    handle.done();
                }
            }
        }
    }
}

impl WlrToplevel {
    /// The toplevel now looks like `info`: send what changed.
    pub fn update(&self, info: ToplevelInfo) {
        let mut inner = self.0.lock().unwrap();
        if inner.closed {
            return;
        }
        let parent_changed = !same_parent(&inner.parent, &info.parent);
        let parent = info.parent.clone();
        let (entered, left) = diff_outputs(&inner.outputs, &info.outputs);
        let title_changed = inner.title != info.title;
        let app_id_changed = inner.app_id != info.app_id;
        let states_changed = inner.states != info.states;
        if !(parent_changed
            || title_changed
            || app_id_changed
            || states_changed
            || !entered.is_empty()
            || !left.is_empty())
        {
            return;
        }
        inner.set(info);
        for handle in &inner.handles {
            if title_changed {
                handle.title(inner.title.clone());
            }
            if app_id_changed {
                handle.app_id(inner.app_id.clone());
            }
            let Some(client) = handle.client() else {
                continue;
            };
            for output in &left {
                for wl_output in output.client_outputs(&client) {
                    handle.output_leave(&wl_output);
                }
            }
            for output in &entered {
                for wl_output in output.client_outputs(&client) {
                    handle.output_enter(&wl_output);
                }
            }
            if states_changed {
                handle.state(inner.states.to_array(handle.version()));
            }
            if parent_changed && handle.version() >= 3 {
                let parent = parent
                    .as_ref()
                    .filter(|p| !Arc::ptr_eq(&p.0, &self.0))
                    .and_then(|p| p.handle_for(&client));
                handle.parent(parent.as_ref());
            }
            handle.done();
        }
    }

    /// This toplevel's handle for `client`, if it has one.
    fn handle_for(&self, client: &Client) -> Option<ZwlrForeignToplevelHandleV1> {
        let inner = self.0.lock().unwrap();
        inner
            .handles
            .iter()
            .find(|h| h.client().as_ref() == Some(client))
            .cloned()
    }
}

impl Inner {
    fn set(&mut self, info: ToplevelInfo) {
        self.title = info.title;
        self.app_id = info.app_id;
        self.states = info.states;
        self.outputs = info.outputs;
        self.parent = info.parent.map(|p| Arc::downgrade(&p.0));
    }

    fn parent(&self) -> Option<WlrToplevel> {
        self.parent
            .as_ref()
            .and_then(Weak::upgrade)
            .map(WlrToplevel)
    }
}

fn same_parent(old: &Option<Weak<Mutex<Inner>>>, new: &Option<WlrToplevel>) -> bool {
    match (old, new) {
        (None, None) => true,
        (Some(old), Some(new)) => std::ptr::eq(old.as_ptr(), Arc::as_ptr(&new.0)),
        _ => false,
    }
}

/// Outputs in `new` but not `old`, and in `old` but not `new`.
fn diff_outputs(old: &[Output], new: &[Output]) -> (Vec<Output>, Vec<Output>) {
    let entered = new.iter().filter(|o| !old.contains(o)).cloned().collect();
    let left = old.iter().filter(|o| !new.contains(o)).cloned().collect();
    (entered, left)
}

/// Create `toplevel`'s handle for `manager` and send everything about it.
fn announce<D: ForeignToplevelManagerHandler>(
    display: &DisplayHandle,
    manager: &ZwlrForeignToplevelManagerV1,
    toplevel: &WlrToplevel,
) {
    let Some(client) = manager.client() else {
        return;
    };
    let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, D>(
        display,
        manager.version(),
        ToplevelData {
            toplevel: Arc::downgrade(&toplevel.0),
        },
    ) else {
        return;
    };
    manager.toplevel(&handle);

    // The parent's handle is looked up before taking this toplevel's lock.
    let parent = toplevel.0.lock().unwrap().parent();
    let parent = parent
        .filter(|p| p != toplevel)
        .and_then(|p| p.handle_for(&client));

    let mut inner = toplevel.0.lock().unwrap();
    handle.title(inner.title.clone());
    handle.app_id(inner.app_id.clone());
    for output in &inner.outputs {
        for wl_output in output.client_outputs(&client) {
            handle.output_enter(&wl_output);
        }
    }
    handle.state(inner.states.to_array(handle.version()));
    if handle.version() >= 3 {
        handle.parent(parent.as_ref());
    }
    handle.done();
    inner.handles.push(handle);
}

impl<D: ForeignToplevelManagerHandler>
    GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData, D>
    for ForeignToplevelManagerState
{
    fn bind(
        state: &mut D,
        handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &ForeignToplevelGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        let protocol = state.foreign_toplevel_manager_state();
        for toplevel in &protocol.toplevels {
            announce::<D>(handle, &manager, toplevel);
        }
        protocol.managers.push(manager);
    }

    fn can_view(client: Client, global_data: &ForeignToplevelGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D: ForeignToplevelManagerHandler> Dispatch<ZwlrForeignToplevelManagerV1, (), D>
    for ForeignToplevelManagerState
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            let managers = &mut state.foreign_toplevel_manager_state().managers;
            if managers.contains(resource) {
                managers.retain(|m| m != resource);
                resource.finished();
            }
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        state
            .foreign_toplevel_manager_state()
            .managers
            .retain(|m| m != resource);
    }
}

impl<D: ForeignToplevelManagerHandler> Dispatch<ZwlrForeignToplevelHandleV1, ToplevelData, D>
    for ForeignToplevelManagerState
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &ToplevelData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;
        let request = match request {
            Request::SetRectangle { width, height, .. } => {
                if width < 0 || height < 0 {
                    resource.post_error(
                        zwlr_foreign_toplevel_handle_v1::Error::InvalidRectangle,
                        format!("invalid rectangle size {width}x{height}"),
                    );
                }
                // Only a hint for minimize animations, which canoe hasn't.
                return;
            }
            Request::Destroy => return,
            Request::Activate { .. } => ToplevelRequest::Activate,
            Request::SetMaximized => ToplevelRequest::Maximize,
            Request::UnsetMaximized => ToplevelRequest::Unmaximize,
            Request::SetMinimized => ToplevelRequest::Minimize,
            Request::UnsetMinimized => ToplevelRequest::Unminimize,
            Request::SetFullscreen { output } => {
                ToplevelRequest::Fullscreen(output.as_ref().and_then(Output::from_resource))
            }
            Request::UnsetFullscreen => ToplevelRequest::Unfullscreen,
            Request::Close => ToplevelRequest::Close,
            _ => return,
        };
        // Inert after closed.
        if let Some(toplevel) = data.toplevel() {
            state.toplevel_request(&toplevel, request);
        }
    }

    fn destroyed(
        _state: &mut D,
        _client: ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
        data: &ToplevelData,
    ) {
        if let Some(inner) = data.toplevel.upgrade() {
            inner.lock().unwrap().handles.retain(|h| h != resource);
        }
    }
}

macro_rules! delegate_foreign_toplevel_manager {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1:
                $crate::compositor::protocols::foreign_toplevel::ForeignToplevelGlobalData
        ] => $crate::compositor::protocols::foreign_toplevel::ForeignToplevelManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1: ()
        ] => $crate::compositor::protocols::foreign_toplevel::ForeignToplevelManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1:
                $crate::compositor::protocols::foreign_toplevel::ToplevelData
        ] => $crate::compositor::protocols::foreign_toplevel::ForeignToplevelManagerState);
    };
}
pub(crate) use delegate_foreign_toplevel_manager;
