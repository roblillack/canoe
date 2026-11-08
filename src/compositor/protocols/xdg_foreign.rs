//! Server side of `xdg-foreign-unstable-v2` (`zxdg_exporter_v2` and
//! `zxdg_importer_v2`, version 1).
//!
//! A client exports one of its toplevels and gets a handle, a random string
//! it passes to another client (usually over D-Bus). That client imports the
//! handle and makes the imported toplevel the parent of one of its own
//! toplevels, as if it had called `xdg_toplevel.set_parent` with it. This is
//! how portal dialogs (file choosers, screen-share and print dialogs, which
//! run in xdg-desktop-portal's process) attach to the window of the app that
//! opened them.
//!
//! Smithay has an implementation, but it never sends `destroyed` to
//! importers, tracks only the last child per handle, and accepts any surface
//! for export. This one follows the protocol and wlroots:
//!
//! * Only xdg toplevels can be exported or be given a foreign parent;
//!   anything else is an `invalid_surface` error. A parent that would make
//!   a loop is ignored: the protocol doesn't say, and an error would
//!   disconnect the portal.
//! * Importing an unknown handle gives an import that is told `destroyed`
//!   right away.
//! * Destroying an export, or the toplevel it exports, invalidates every
//!   relationship set up through its handle: the children lose that parent
//!   and every import of the handle gets `destroyed` and becomes inert.
//! * Destroying an import removes the parent from the children it was set
//!   on.
//!
//! The compositor calls [`toplevel_destroyed`] when an xdg toplevel goes
//! away. Parent changes are reported through
//! [`XdgShellHandler::parent_changed`], like the client's own
//! `set_parent`.
//!
//! ```ignore
//! impl XdgForeignHandler for State {
//!     fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
//!         &mut self.xdg_foreign_state
//!     }
//! }
//! delegate_xdg_foreign!(State);
//!
//! let xdg_foreign_state = XdgForeignState::new::<State>(&display_handle);
//! ```

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::Mutex;

use smithay::reexports::wayland_protocols::xdg::foreign::zv2::server::{
    zxdg_exported_v2::{self, ZxdgExportedV2},
    zxdg_exporter_v2::{self, ZxdgExporterV2},
    zxdg_imported_v2::{self, ZxdgImportedV2},
    zxdg_importer_v2::{self, ZxdgImporterV2},
};
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::is_valid_parent;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellHandler, XdgToplevelSurfaceData};

const VERSION: u32 = 1;

/// The compositor's side of xdg-foreign.
pub trait XdgForeignHandler:
    XdgShellHandler
    + GlobalDispatch<ZxdgExporterV2, ()>
    + GlobalDispatch<ZxdgImporterV2, ()>
    + Dispatch<ZxdgExporterV2, ()>
    + Dispatch<ZxdgImporterV2, ()>
    + Dispatch<ZxdgExportedV2, ExportedData>
    + Dispatch<ZxdgImportedV2, ImportedData>
    + 'static
{
    /// Returns the protocol state.
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState;
}

/// Data of an export: its handle.
pub struct ExportedData {
    handle: String,
}

/// Data of an import.
pub struct ImportedData {
    inner: Mutex<Imported>,
}

struct Imported {
    /// The handle while the import is valid; `None` once it is inert.
    handle: Option<String>,
    /// The surfaces this import was made the parent of.
    children: Vec<WlSurface>,
}

struct Export {
    /// The exported toplevel.
    surface: WlSurface,
    /// The valid imports of the handle.
    imports: Vec<ZxdgImportedV2>,
}

/// State of the xdg-foreign protocol.
pub struct XdgForeignState {
    exporter: GlobalId,
    importer: GlobalId,
    /// Exports whose toplevel is alive, by handle.
    exports: HashMap<String, Export>,
}

impl XdgForeignState {
    /// Create the `zxdg_exporter_v2` and `zxdg_importer_v2` globals.
    pub fn new<D: XdgForeignHandler>(display: &DisplayHandle) -> Self {
        Self {
            exporter: display.create_global::<D, ZxdgExporterV2, _>(VERSION, ()),
            importer: display.create_global::<D, ZxdgImporterV2, _>(VERSION, ()),
            exports: HashMap::new(),
        }
    }

    pub fn exporter_global(&self) -> GlobalId {
        self.exporter.clone()
    }

    pub fn importer_global(&self) -> GlobalId {
        self.importer.clone()
    }

    /// A handle no other export has, and that other clients can't guess.
    fn new_handle(&self) -> String {
        loop {
            // Each RandomState has fresh keys seeded from the OS.
            let mut handle = String::with_capacity(32);
            for _ in 0..2 {
                let mut hasher = std::hash::RandomState::new().build_hasher();
                hasher.write_usize(self.exports.len());
                handle.push_str(&format!("{:016x}", hasher.finish()));
            }
            if !self.exports.contains_key(&handle) {
                return handle;
            }
        }
    }
}

/// The live xdg toplevel of `surface`.
fn find_toplevel<D: XdgShellHandler>(
    state: &mut D,
    surface: &WlSurface,
) -> Option<ToplevelSurface> {
    state
        .xdg_shell_state()
        .toplevel_surfaces()
        .iter()
        .find(|t| t.wl_surface() == surface)
        .cloned()
}

/// Set or clear `child`'s parent. Returns whether it changed.
fn set_parent(child: &WlSurface, parent: Option<&WlSurface>) -> bool {
    with_states(child, |states| {
        let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
            return false;
        };
        let mut role = data.lock().unwrap();
        if role.parent.as_ref() == parent {
            return false;
        }
        role.parent = parent.cloned();
        true
    })
}

/// Clear the parent of those `children` whose parent is still `parent`
/// (they may have picked another one since), and report the change.
fn unparent<D: XdgForeignHandler>(state: &mut D, children: Vec<WlSurface>, parent: &WlSurface) {
    for child in children {
        if !child.is_alive() {
            continue;
        }
        let is_parent = with_states(&child, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .is_some_and(|data| data.lock().unwrap().parent.as_ref() == Some(parent))
        });
        if is_parent && set_parent(&child, None) {
            if let Some(toplevel) = find_toplevel(state, &child) {
                state.parent_changed(toplevel);
            }
        }
    }
}

/// Invalidate the export with `handle`: its imports get `destroyed` and
/// become inert, and their children lose the parent.
fn revoke<D: XdgForeignHandler>(state: &mut D, handle: &str) {
    let Some(export) = state.xdg_foreign_state().exports.remove(handle) else {
        return;
    };
    let mut children = Vec::new();
    for import in &export.imports {
        if let Some(data) = import.data::<ImportedData>() {
            let mut inner = data.inner.lock().unwrap();
            inner.handle = None;
            children.append(&mut inner.children);
        }
        import.destroyed();
    }
    unparent(state, children, &export.surface);
}

/// An xdg toplevel was destroyed: revoke its exports.
pub fn toplevel_destroyed<D: XdgForeignHandler>(state: &mut D, surface: &WlSurface) {
    let handles: Vec<String> = state
        .xdg_foreign_state()
        .exports
        .iter()
        .filter(|(_, export)| &export.surface == surface)
        .map(|(handle, _)| handle.clone())
        .collect();
    for handle in handles {
        revoke(state, &handle);
    }
}

impl<D: XdgForeignHandler> GlobalDispatch<ZxdgExporterV2, (), D> for XdgForeignState {
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZxdgExporterV2>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D: XdgForeignHandler> GlobalDispatch<ZxdgImporterV2, (), D> for XdgForeignState {
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZxdgImporterV2>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D: XdgForeignHandler> Dispatch<ZxdgExporterV2, (), D> for XdgForeignState {
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZxdgExporterV2,
        request: zxdg_exporter_v2::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zxdg_exporter_v2::Request::ExportToplevel { id, surface } => {
                if find_toplevel(state, &surface).is_none() {
                    resource.post_error(
                        zxdg_exporter_v2::Error::InvalidSurface,
                        "only xdg toplevels can be exported",
                    );
                    return;
                }
                let foreign = state.xdg_foreign_state();
                let handle = foreign.new_handle();
                let exported = data_init.init(
                    id,
                    ExportedData {
                        handle: handle.clone(),
                    },
                );
                exported.handle(handle.clone());
                foreign.exports.insert(
                    handle,
                    Export {
                        surface,
                        imports: Vec::new(),
                    },
                );
            }
            zxdg_exporter_v2::Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D: XdgForeignHandler> Dispatch<ZxdgExportedV2, ExportedData, D> for XdgForeignState {
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &ZxdgExportedV2,
        _request: zxdg_exported_v2::Request,
        _data: &ExportedData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // Only `destroy`, handled in `destroyed`.
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        _resource: &ZxdgExportedV2,
        data: &ExportedData,
    ) {
        revoke(state, &data.handle);
    }
}

impl<D: XdgForeignHandler> Dispatch<ZxdgImporterV2, (), D> for XdgForeignState {
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &ZxdgImporterV2,
        request: zxdg_importer_v2::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zxdg_importer_v2::Request::ImportToplevel { id, handle } => {
                let export = state.xdg_foreign_state().exports.get_mut(&handle);
                let valid = export.is_some();
                let imported = data_init.init(
                    id,
                    ImportedData {
                        inner: Mutex::new(Imported {
                            handle: valid.then(|| handle.clone()),
                            children: Vec::new(),
                        }),
                    },
                );
                match export {
                    Some(export) => export.imports.push(imported),
                    None => imported.destroyed(),
                }
            }
            zxdg_importer_v2::Request::Destroy => {}
            _ => {}
        }
    }
}

impl<D: XdgForeignHandler> Dispatch<ZxdgImportedV2, ImportedData, D> for XdgForeignState {
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZxdgImportedV2,
        request: zxdg_imported_v2::Request,
        data: &ImportedData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zxdg_imported_v2::Request::SetParentOf { surface: child } => {
                let Some(toplevel) = find_toplevel(state, &child) else {
                    resource.post_error(
                        zxdg_imported_v2::Error::InvalidSurface,
                        "only xdg toplevels can have a foreign parent",
                    );
                    return;
                };
                // Inert imports ignore requests.
                let Some(handle) = data.inner.lock().unwrap().handle.clone() else {
                    return;
                };
                let Some(parent) = state
                    .xdg_foreign_state()
                    .exports
                    .get(&handle)
                    .map(|export| export.surface.clone())
                else {
                    return;
                };
                if !is_valid_parent(&child, &parent) {
                    return;
                }
                {
                    let mut inner = data.inner.lock().unwrap();
                    inner.children.retain(|c| c.is_alive());
                    if !inner.children.contains(&child) {
                        inner.children.push(child.clone());
                    }
                }
                if set_parent(&child, Some(&parent)) {
                    state.parent_changed(toplevel);
                }
            }
            zxdg_imported_v2::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, resource: &ZxdgImportedV2, data: &ImportedData) {
        let (handle, children) = {
            let mut inner = data.inner.lock().unwrap();
            (inner.handle.take(), std::mem::take(&mut inner.children))
        };
        let Some(handle) = handle else {
            return;
        };
        let Some(export) = state.xdg_foreign_state().exports.get_mut(&handle) else {
            return;
        };
        export.imports.retain(|i| i != resource);
        let parent = export.surface.clone();
        unparent(state, children, &parent);
    }
}

macro_rules! delegate_xdg_foreign {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::foreign::zv2::server::zxdg_exporter_v2::ZxdgExporterV2: ()
        ] => $crate::compositor::protocols::xdg_foreign::XdgForeignState);
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::foreign::zv2::server::zxdg_importer_v2::ZxdgImporterV2: ()
        ] => $crate::compositor::protocols::xdg_foreign::XdgForeignState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::foreign::zv2::server::zxdg_exporter_v2::ZxdgExporterV2: ()
        ] => $crate::compositor::protocols::xdg_foreign::XdgForeignState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::foreign::zv2::server::zxdg_importer_v2::ZxdgImporterV2: ()
        ] => $crate::compositor::protocols::xdg_foreign::XdgForeignState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::foreign::zv2::server::zxdg_exported_v2::ZxdgExportedV2:
                $crate::compositor::protocols::xdg_foreign::ExportedData
        ] => $crate::compositor::protocols::xdg_foreign::XdgForeignState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::foreign::zv2::server::zxdg_imported_v2::ZxdgImportedV2:
                $crate::compositor::protocols::xdg_foreign::ImportedData
        ] => $crate::compositor::protocols::xdg_foreign::XdgForeignState);
    };
}
pub(crate) use delegate_xdg_foreign;
