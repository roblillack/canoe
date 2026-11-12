//! Server side of KDE's `org_kde_plasma_shell` (version 8), reduced to the
//! one request canoe has a use for.
//!
//! Plasma uses this protocol for its own shell (panels, the desktop, OSDs),
//! and with it any client can ask for its window to be left out of window
//! switchers. canoe only honors that:
//!
//! * `set_skip_switcher` keeps the window out of the window menu, Super+Tab
//!   and focus cycling. Unlike most surface state it isn't double-buffered:
//!   it applies right away, as in KWin.
//! * Destroying the `org_kde_plasma_surface` lists the window again.
//! * Every other request (roles, positions, panel behavior, skipping the
//!   taskbar, …) is accepted and ignored.
//!
//! A client may set the flag before its surface becomes a toplevel. It is
//! kept with the `wl_surface`, so [`skip_switcher`] reads it when the window
//! is created.
//!
//! ```ignore
//! delegate_plasma_shell!(State);
//!
//! PlasmaShellState::new::<State>(&display_handle);
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};
use smithay::wayland::compositor::with_states;
use wayland_protocols_plasma::plasma_shell::server::{
    org_kde_plasma_shell::{self, OrgKdePlasmaShell},
    org_kde_plasma_surface::{self, OrgKdePlasmaSurface},
};

const VERSION: u32 = 8;

/// The compositor's side of org_kde_plasma_shell.
pub trait PlasmaShellHandler:
    GlobalDispatch<OrgKdePlasmaShell, ()>
    + Dispatch<OrgKdePlasmaShell, ()>
    + Dispatch<OrgKdePlasmaSurface, PlasmaSurfaceData>
    + 'static
{
    /// The client of `surface` asked for its window to be left out of window
    /// switchers (`skip`), or to be listed again.
    fn skip_switcher_changed(&mut self, surface: &WlSurface, skip: bool);
}

/// Data of an `org_kde_plasma_surface`: the surface it was created for.
pub struct PlasmaSurfaceData {
    surface: Weak<WlSurface>,
}

/// Whether a surface's client asked for it to skip window switchers.
#[derive(Default)]
struct SkipSwitcher(AtomicBool);

/// Whether the client of `surface` asked for its window to be left out of
/// window switchers.
pub fn skip_switcher(surface: &WlSurface) -> bool {
    surface.is_alive()
        && with_states(surface, |states| {
            states
                .data_map
                .get::<SkipSwitcher>()
                .is_some_and(|skip| skip.0.load(Ordering::Relaxed))
        })
}

/// Record `skip` for `surface` and tell the compositor if it changed.
fn set_skip_switcher<D: PlasmaShellHandler>(state: &mut D, data: &PlasmaSurfaceData, skip: bool) {
    let Ok(surface) = data.surface.upgrade() else {
        return;
    };
    let previous = with_states(&surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(SkipSwitcher::default);
        states
            .data_map
            .get::<SkipSwitcher>()
            .unwrap()
            .0
            .swap(skip, Ordering::Relaxed)
    });
    if previous != skip {
        state.skip_switcher_changed(&surface, skip);
    }
}

/// State of the org_kde_plasma_shell protocol.
pub struct PlasmaShellState {
    global: GlobalId,
}

impl PlasmaShellState {
    /// Create the `org_kde_plasma_shell` global.
    pub fn new<D: PlasmaShellHandler>(display: &DisplayHandle) -> Self {
        Self {
            global: display.create_global::<D, OrgKdePlasmaShell, _>(VERSION, ()),
        }
    }

    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

impl<D: PlasmaShellHandler> GlobalDispatch<OrgKdePlasmaShell, (), D> for PlasmaShellState {
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<OrgKdePlasmaShell>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D: PlasmaShellHandler> Dispatch<OrgKdePlasmaShell, (), D> for PlasmaShellState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &OrgKdePlasmaShell,
        request: org_kde_plasma_shell::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            org_kde_plasma_shell::Request::GetSurface { id, surface } => {
                data_init.init(
                    id,
                    PlasmaSurfaceData {
                        surface: surface.downgrade(),
                    },
                );
            }
            _ => unreachable!(),
        }
    }
}

impl<D: PlasmaShellHandler> Dispatch<OrgKdePlasmaSurface, PlasmaSurfaceData, D>
    for PlasmaShellState
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &OrgKdePlasmaSurface,
        request: org_kde_plasma_surface::Request,
        data: &PlasmaSurfaceData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            org_kde_plasma_surface::Request::SetSkipSwitcher { skip } => {
                set_skip_switcher(state, data, skip != 0);
            }
            org_kde_plasma_surface::Request::Destroy => {
                set_skip_switcher(state, data, false);
            }
            _ => {}
        }
    }
}

macro_rules! delegate_plasma_shell {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            wayland_protocols_plasma::plasma_shell::server::org_kde_plasma_shell::OrgKdePlasmaShell: ()
        ] => $crate::compositor::protocols::plasma_shell::PlasmaShellState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            wayland_protocols_plasma::plasma_shell::server::org_kde_plasma_shell::OrgKdePlasmaShell: ()
        ] => $crate::compositor::protocols::plasma_shell::PlasmaShellState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            wayland_protocols_plasma::plasma_shell::server::org_kde_plasma_surface::OrgKdePlasmaSurface:
                $crate::compositor::protocols::plasma_shell::PlasmaSurfaceData
        ] => $crate::compositor::protocols::plasma_shell::PlasmaShellState);
    };
}
pub(crate) use delegate_plasma_shell;
