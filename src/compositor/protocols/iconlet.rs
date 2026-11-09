//! Server side of `xx-iconlet-v1` (see `xx-iconlet-v1.xml` next to this
//! file), an experimental protocol for the desktop icons of minimized
//! windows.
//!
//! * `xx_iconic_toplevel_v1.icon_geometry` and `icon_hidden` tell a client
//!   where its window's desktop icon is. The compositor works out the icons'
//!   rectangles whenever it lays out a desktop and hands them to
//!   [`set_icon_geometry`], which sends what changed.
//! * `get_icon_surface` makes a wl_surface the icon, which the compositor
//!   draws in the icon's place once it has a buffer ([`icon_surface`]). It
//!   always asks for icons of [`ICON_SIZE`] logical pixels.
//! * `set_iconlet` marks the window as an iconlet ([`is_iconlet`]), which the
//!   window manager keeps minimized, out of window lists and ahead of other
//!   icons; activating its icon sends `activate` ([`send_activate`]).
//!
//! The state lives with the toplevel's wl_surface, so it can be read when
//! the window is created. The handler hears about changes of the iconlet
//! flag and the icon surface.
//!
//! ```ignore
//! delegate_iconlet!(State);
//!
//! IconletManagerState::new::<State>(&display_handle);
//! ```

use std::sync::Mutex;

use smithay::output::Output;
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
};
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::compositor::{give_role, with_states};
use smithay::wayland::shell::xdg::XdgShellHandler;

#[allow(missing_docs, clippy::all)]
pub mod server {
    use smithay::reexports::wayland_protocols::xdg::shell::server::*;
    use smithay::reexports::wayland_server;
    use smithay::reexports::wayland_server::protocol::*;

    pub mod __interfaces {
        use smithay::reexports::wayland_protocols::xdg::shell::server::__interfaces::*;
        use smithay::reexports::wayland_server::backend as wayland_backend;
        use smithay::reexports::wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("src/compositor/protocols/xx-iconlet-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("src/compositor/protocols/xx-iconlet-v1.xml");
}

use server::{
    xx_icon_surface_v1::XxIconSurfaceV1,
    xx_iconic_toplevel_v1::{self, XxIconicToplevelV1},
    xx_iconlet_manager_v1::{self, XxIconletManagerV1},
};

const VERSION: u32 = 1;
/// The wl_surface role of icon surfaces.
const ROLE: &str = "xx_icon_surface_v1";
/// The size of icons, in logical pixels: that of canoe's desktop icons.
pub const ICON_SIZE: i32 = crate::canoe::ICON_SIZE;

/// The compositor's side of xx-iconlet-v1.
pub trait IconletHandler:
    XdgShellHandler
    + GlobalDispatch<XxIconletManagerV1, ()>
    + Dispatch<XxIconletManagerV1, ()>
    + Dispatch<XxIconicToplevelV1, IconicData>
    + Dispatch<XxIconSurfaceV1, IconSurfaceData>
    + 'static
{
    /// The client of toplevel `surface` declared it an iconlet, or stopped
    /// doing so (`iconlet`).
    fn iconlet_changed(&mut self, surface: &WlSurface, iconlet: bool);

    /// Toplevel `surface` got or lost its icon surface, or the icon surface
    /// committed.
    fn icon_surface_changed(&mut self, surface: &WlSurface);
}

/// Data of an `xx_iconic_toplevel_v1`: its toplevel's surface, or nothing
/// if it was made for an inert toplevel.
pub struct IconicData {
    toplevel: Option<Weak<WlSurface>>,
}

/// Data of an `xx_icon_surface_v1`.
pub struct IconSurfaceData {
    surface: Weak<WlSurface>,
    toplevel: Option<Weak<WlSurface>>,
}

/// What a toplevel's surface keeps of this protocol.
#[derive(Default)]
struct Iconic(Mutex<IconicState>);

#[derive(Default)]
struct IconicState {
    object: Option<XxIconicToplevelV1>,
    iconlet: bool,
    icon_surface: Option<(WlSurface, XxIconSurfaceV1)>,
    /// What the client was told about its icon's position.
    geometry: Option<(Output, Rectangle<i32, Logical>)>,
}

/// The icon surface of a surface's role, telling the toplevel it draws.
#[derive(Default)]
struct IconFor(Mutex<Option<Weak<WlSurface>>>);

fn with_iconic<T>(surface: &WlSurface, f: impl FnOnce(&mut IconicState) -> T) -> T {
    with_states(surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(Iconic::default);
        f(&mut states.data_map.get::<Iconic>().unwrap().0.lock().unwrap())
    })
}

/// Whether the client of toplevel `surface` declared it an iconlet.
pub fn is_iconlet(surface: &WlSurface) -> bool {
    surface.is_alive() && with_iconic(surface, |state| state.iconlet)
}

/// The surface toplevel `surface` draws its icon with, once it has a buffer
/// to show.
pub fn icon_surface(surface: &WlSurface) -> Option<WlSurface> {
    if !surface.is_alive() {
        return None;
    }
    let icon = with_iconic(surface, |state| {
        state
            .icon_surface
            .as_ref()
            .map(|(surface, _)| surface.clone())
    })?;
    let has_buffer =
        smithay::backend::renderer::utils::with_renderer_surface_state(&icon, |state| {
            state.buffer().is_some()
        })
        .unwrap_or(false);
    (icon.is_alive() && has_buffer).then_some(icon)
}

/// The toplevel surface whose icon `surface` is, if it is an icon surface.
pub fn icon_surface_owner(surface: &WlSurface) -> Option<WlSurface> {
    with_states(surface, |states| {
        states
            .data_map
            .get::<IconFor>()
            .and_then(|icon_for| icon_for.0.lock().unwrap().clone())
    })?
    .upgrade()
    .ok()
}

/// Tell the client of toplevel `surface` where its icon is shown, on which
/// output and where on it in logical pixels, or that it isn't (`None`).
/// Only changes are sent.
pub fn set_icon_geometry(surface: &WlSurface, geometry: Option<(Output, Rectangle<i32, Logical>)>) {
    if !surface.is_alive() {
        return;
    }
    let (object, previous) = with_iconic(surface, |state| {
        if state.geometry == geometry {
            return (None, None);
        }
        let previous = std::mem::replace(&mut state.geometry, geometry.clone());
        (state.object.clone(), Some(previous))
    });
    let (Some(object), Some(previous)) = (object, previous) else {
        return;
    };
    let Some(client) = object.client() else {
        return;
    };
    match geometry {
        Some((output, rect)) => {
            let mut outputs = output.client_outputs(&client).peekable();
            if outputs.peek().is_none() {
                object.icon_geometry(None, rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);
            }
            for wl_output in outputs {
                object.icon_geometry(
                    Some(&wl_output),
                    rect.loc.x,
                    rect.loc.y,
                    rect.size.w,
                    rect.size.h,
                );
            }
        }
        None if previous.is_some() => object.icon_hidden(),
        None => {}
    }
}

/// Tell an iconlet's client that the user activated its icon, with an
/// xdg-activation `token` for a window it may open in response.
pub fn send_activate(surface: &WlSurface, token: &str) {
    if !surface.is_alive() {
        return;
    }
    let object = with_iconic(surface, |state| {
        state.object.clone().filter(|_| state.iconlet)
    });
    if let Some(object) = object {
        object.activate(token.to_owned());
    }
}

/// State of the xx-iconlet protocol.
pub struct IconletManagerState {
    global: GlobalId,
}

impl IconletManagerState {
    /// Create the `xx_iconlet_manager_v1` global.
    pub fn new<D: IconletHandler>(display: &DisplayHandle) -> Self {
        Self {
            global: display.create_global::<D, XxIconletManagerV1, _>(VERSION, ()),
        }
    }

    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

impl<D: IconletHandler> GlobalDispatch<XxIconletManagerV1, (), D> for IconletManagerState {
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XxIconletManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D: IconletHandler> Dispatch<XxIconletManagerV1, (), D> for IconletManagerState {
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &XxIconletManagerV1,
        request: xx_iconlet_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xx_iconlet_manager_v1::Request::GetIconicToplevel { id, toplevel } => {
                // A toplevel whose role object is gone gets an inert object.
                let surface = state
                    .xdg_shell_state()
                    .get_toplevel(&toplevel)
                    .map(|toplevel| toplevel.wl_surface().clone());
                let Some(surface) = surface else {
                    data_init.init(id, IconicData { toplevel: None });
                    return;
                };
                let taken = with_iconic(&surface, |state| {
                    state
                        .object
                        .as_ref()
                        .is_some_and(|object| object.is_alive())
                });
                if taken {
                    manager.post_error(
                        xx_iconlet_manager_v1::Error::AlreadyConstructed,
                        "the toplevel already has an xx_iconic_toplevel_v1",
                    );
                    return;
                }
                let object = data_init.init(
                    id,
                    IconicData {
                        toplevel: Some(surface.downgrade()),
                    },
                );
                with_iconic(&surface, |state| {
                    state.object = Some(object);
                    // Tell the new object about an icon that is shown.
                    state.geometry = None;
                });
            }
            xx_iconlet_manager_v1::Request::Destroy => {}
        }
    }
}

/// The toplevel surface of an iconic toplevel object, unless it is inert.
fn toplevel_of(object: &XxIconicToplevelV1, data: &IconicData) -> Option<WlSurface> {
    let surface = data.toplevel.as_ref()?.upgrade().ok()?;
    let ours = with_iconic(&surface, |state| state.object.as_ref() == Some(object));
    ours.then_some(surface)
}

impl<D: IconletHandler> Dispatch<XxIconicToplevelV1, IconicData, D> for IconletManagerState {
    fn request(
        state: &mut D,
        _client: &Client,
        object: &XxIconicToplevelV1,
        request: xx_iconic_toplevel_v1::Request,
        data: &IconicData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xx_iconic_toplevel_v1::Request::SetIconlet => {
                let Some(toplevel) = toplevel_of(object, data) else {
                    return;
                };
                let changed = with_iconic(&toplevel, |state| {
                    !std::mem::replace(&mut state.iconlet, true)
                });
                if changed {
                    state.iconlet_changed(&toplevel, true);
                }
            }
            xx_iconic_toplevel_v1::Request::GetIconSurface { id, surface } => {
                let toplevel = toplevel_of(object, data);
                if let Some(toplevel) = &toplevel {
                    let exists = with_iconic(toplevel, |state| {
                        state
                            .icon_surface
                            .as_ref()
                            .is_some_and(|(surface, role)| surface.is_alive() && role.is_alive())
                    });
                    if exists {
                        object.post_error(
                            xx_iconic_toplevel_v1::Error::IconSurfaceExists,
                            "the toplevel already has an icon surface",
                        );
                        return;
                    }
                }
                if give_role(&surface, ROLE).is_err() {
                    object.post_error(
                        xx_iconic_toplevel_v1::Error::Role,
                        "the surface already has another role",
                    );
                    return;
                }
                let role = data_init.init(
                    id,
                    IconSurfaceData {
                        surface: surface.downgrade(),
                        toplevel: toplevel.as_ref().map(|t| t.downgrade()),
                    },
                );
                role.configure(ICON_SIZE, ICON_SIZE);
                let Some(toplevel) = toplevel else {
                    return;
                };
                with_states(&surface, |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(IconFor::default);
                    *states.data_map.get::<IconFor>().unwrap().0.lock().unwrap() =
                        Some(toplevel.downgrade());
                });
                with_iconic(&toplevel, |state| {
                    state.icon_surface = Some((surface, role))
                });
                state.icon_surface_changed(&toplevel);
            }
            xx_iconic_toplevel_v1::Request::Destroy => {}
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, object: &XxIconicToplevelV1, data: &IconicData) {
        let Some(toplevel) = data.toplevel.as_ref().and_then(|t| t.upgrade().ok()) else {
            return;
        };
        let (iconlet, icon_surface) = with_iconic(&toplevel, |state| {
            if state.object.as_ref() != Some(object) {
                return (false, None);
            }
            state.object = None;
            state.geometry = None;
            (
                std::mem::take(&mut state.iconlet),
                state.icon_surface.take(),
            )
        });
        if let Some((surface, _)) = icon_surface {
            unlink_icon_surface(&surface);
            state.icon_surface_changed(&toplevel);
        }
        if iconlet {
            state.iconlet_changed(&toplevel, false);
        }
    }
}

/// Forget which toplevel icon surface `surface` was for.
fn unlink_icon_surface(surface: &WlSurface) {
    if surface.is_alive() {
        with_states(surface, |states| {
            if let Some(icon_for) = states.data_map.get::<IconFor>() {
                *icon_for.0.lock().unwrap() = None;
            }
        });
    }
}

impl<D: IconletHandler> Dispatch<XxIconSurfaceV1, IconSurfaceData, D> for IconletManagerState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _role: &XxIconSurfaceV1,
        request: server::xx_icon_surface_v1::Request,
        _data: &IconSurfaceData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            server::xx_icon_surface_v1::Request::Destroy => {}
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, role: &XxIconSurfaceV1, data: &IconSurfaceData) {
        if let Ok(surface) = data.surface.upgrade() {
            unlink_icon_surface(&surface);
        }
        let Some(toplevel) = data.toplevel.as_ref().and_then(|t| t.upgrade().ok()) else {
            return;
        };
        let removed = with_iconic(&toplevel, |state| {
            let ours = state.icon_surface.as_ref().is_some_and(|(_, r)| r == role);
            if ours {
                state.icon_surface = None;
            }
            ours
        });
        if removed {
            state.icon_surface_changed(&toplevel);
        }
    }
}

macro_rules! delegate_iconlet {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::compositor::protocols::iconlet::server::xx_iconlet_manager_v1::XxIconletManagerV1: ()
        ] => $crate::compositor::protocols::iconlet::IconletManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::compositor::protocols::iconlet::server::xx_iconlet_manager_v1::XxIconletManagerV1: ()
        ] => $crate::compositor::protocols::iconlet::IconletManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::compositor::protocols::iconlet::server::xx_iconic_toplevel_v1::XxIconicToplevelV1:
                $crate::compositor::protocols::iconlet::IconicData
        ] => $crate::compositor::protocols::iconlet::IconletManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::compositor::protocols::iconlet::server::xx_icon_surface_v1::XxIconSurfaceV1:
                $crate::compositor::protocols::iconlet::IconSurfaceData
        ] => $crate::compositor::protocols::iconlet::IconletManagerState);
    };
}
pub(crate) use delegate_iconlet;
