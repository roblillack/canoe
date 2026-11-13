//! Server side of `xx-primary-output-v1` (see `xx-primary-output-v1.xml`
//! next to this file), canoe's protocol for showing and choosing the
//! primary output, and for seeing which outputs a closed lid covers.
//!
//! The compositor tells the module which output is primary with
//! [`PrimaryOutputState::set_primary`], and which outputs are a laptop's
//! panel and whether the lid is closed with [`PrimaryOutputState::set_lids`];
//! both announce changes to every bound manager. A client's choice goes to
//! [`PrimaryOutputHandler::choose_primary_output`]; the client hears back
//! either way.
//!
//! ```ignore
//! delegate_primary_output!(State);
//!
//! PrimaryOutputState::new::<State>(&display_handle);
//! ```

use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

#[allow(missing_docs, clippy::all)]
pub mod server {
    use smithay::reexports::wayland_server;

    pub mod __interfaces {
        use smithay::reexports::wayland_server::backend as wayland_backend;
        wayland_scanner::generate_interfaces!("src/compositor/protocols/xx-primary-output-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("src/compositor/protocols/xx-primary-output-v1.xml");
}

use server::xx_primary_output_manager_v1::{self, LidState, XxPrimaryOutputManagerV1};

const VERSION: u32 = 1;

/// The compositor's side of xx-primary-output-v1.
pub trait PrimaryOutputHandler:
    GlobalDispatch<XxPrimaryOutputManagerV1, ()> + Dispatch<XxPrimaryOutputManagerV1, ()> + 'static
{
    fn primary_output_state(&mut self) -> &mut PrimaryOutputState;

    /// A client chose the output named `name` as primary, or none. Accept it
    /// with [`PrimaryOutputState::set_primary`], or leave things as they
    /// are; the client is told the outcome.
    fn choose_primary_output(&mut self, name: Option<String>);
}

/// State of the xx-primary-output protocol.
pub struct PrimaryOutputState {
    global: GlobalId,
    managers: Vec<XxPrimaryOutputManagerV1>,
    primary: Option<String>,
    /// The connected laptop panels by name, and whether the lid is closed.
    lids: Vec<(String, bool)>,
}

fn send_lid(manager: &XxPrimaryOutputManagerV1, name: &str, closed: bool) {
    let state = if closed {
        LidState::Closed
    } else {
        LidState::Open
    };
    manager.lid(name.to_string(), state);
}

impl PrimaryOutputState {
    /// Create the `xx_primary_output_manager_v1` global.
    pub fn new<D: PrimaryOutputHandler>(display: &DisplayHandle) -> Self {
        Self {
            global: display.create_global::<D, XxPrimaryOutputManagerV1, _>(VERSION, ()),
            managers: Vec::new(),
            primary: None,
            lids: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// The name of the primary output, if there is one.
    pub fn primary(&self) -> Option<&str> {
        self.primary.as_deref()
    }

    /// Make the output named `name` primary, or none, and tell the clients
    /// if that changed anything.
    pub fn set_primary(&mut self, name: Option<String>) {
        if self.primary == name {
            return;
        }
        self.primary = name;
        self.managers.retain(|m| m.is_alive());
        for manager in &self.managers {
            manager.primary(self.primary.clone());
        }
    }
}

impl PrimaryOutputState {
    /// The connected laptop panels are those named in `lids`, with the lid
    /// closed or not. Tell the clients about new panels and changed lids.
    pub fn set_lids(&mut self, lids: Vec<(String, bool)>) {
        self.managers.retain(|m| m.is_alive());
        for (name, closed) in &lids {
            if !self.lids.contains(&(name.clone(), *closed)) {
                for manager in &self.managers {
                    send_lid(manager, name, *closed);
                }
            }
        }
        self.lids = lids;
    }
}

impl<D: PrimaryOutputHandler> GlobalDispatch<XxPrimaryOutputManagerV1, (), D>
    for PrimaryOutputState
{
    fn bind(
        state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XxPrimaryOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        let protocol = state.primary_output_state();
        manager.primary(protocol.primary.clone());
        for (name, closed) in &protocol.lids {
            send_lid(&manager, name, *closed);
        }
        protocol.managers.push(manager);
    }
}

impl<D: PrimaryOutputHandler> Dispatch<XxPrimaryOutputManagerV1, (), D> for PrimaryOutputState {
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &XxPrimaryOutputManagerV1,
        request: xx_primary_output_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let name = match request {
            xx_primary_output_manager_v1::Request::SetPrimary { name } => Some(name),
            xx_primary_output_manager_v1::Request::UnsetPrimary => None,
            xx_primary_output_manager_v1::Request::Destroy => return,
        };
        let before = state.primary_output_state().primary.clone();
        state.choose_primary_output(name);
        // Changes went to every manager; otherwise answer this one.
        let after = state.primary_output_state().primary.clone();
        if after == before {
            manager.primary(after);
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, manager: &XxPrimaryOutputManagerV1, _data: &()) {
        state
            .primary_output_state()
            .managers
            .retain(|m| m != manager);
    }
}

macro_rules! delegate_primary_output {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::compositor::protocols::primary_output::server::xx_primary_output_manager_v1::XxPrimaryOutputManagerV1: ()
        ] => $crate::compositor::protocols::primary_output::PrimaryOutputState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::compositor::protocols::primary_output::server::xx_primary_output_manager_v1::XxPrimaryOutputManagerV1: ()
        ] => $crate::compositor::protocols::primary_output::PrimaryOutputState);
    };
}
pub(crate) use delegate_primary_output;
