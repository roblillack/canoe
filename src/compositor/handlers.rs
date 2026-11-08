//! Smithay protocol handlers: what clients ask the compositor for.

use std::sync::Mutex;

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output, PopupKeyboardGrab,
    PopupKind, PopupPointerGrab, PopupUngrabStrategy, WindowSurfaceType,
};
use smithay::input::pointer::{CursorImageStatus, Focus};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::utils::{Logical, Point, Rectangle, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler,
    CompositorState,
};
use smithay::wayland::dmabuf::{DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::foreign_toplevel_list::{ForeignToplevelListHandler, ForeignToplevelListState};
use smithay::wayland::fractional_scale::{with_fractional_scale, FractionalScaleHandler};
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraintsHandler};
use smithay::wayland::selection::data_device::{
    set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
    ServerDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
    WlrLayerShellState,
};
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::{
    Mode as KdeMode, OrgKdeKwinServerDecoration,
};
use smithay::reexports::wayland_server::WEnum;
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::{
    delegate_compositor, delegate_cursor_shape, delegate_data_control, delegate_data_device,
    delegate_dmabuf, delegate_foreign_toplevel_list, delegate_fractional_scale,
    delegate_idle_inhibit, delegate_kde_decoration, delegate_idle_notify, delegate_layer_shell, delegate_output,
    delegate_pointer_constraints, delegate_presentation, delegate_primary_selection,
    delegate_relative_pointer, delegate_seat, delegate_session_lock,
    delegate_single_pixel_buffer, delegate_viewporter,
    delegate_xdg_activation, delegate_xdg_decoration, delegate_xdg_shell,
};

use super::{ClientState, LockState, State};
use crate::canoe;

/// The linux-dmabuf global, when the renderer can import dmabufs.
pub struct DmabufGlobal {
    pub state: DmabufState,
    #[allow(dead_code)] // keeps the global's handle
    pub global: smithay::wayland::dmabuf::DmabufGlobal,
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(xwayland) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &xwayland.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.backend_early_import(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root) {
                window.window.on_commit();
            }
            if &root == surface {
                if let Some(icon) = super::protocols::toplevel_icon::commit(surface) {
                    self.toplevel_icon_changed(surface, icon);
                }
                self.toplevel_committed(surface);
            }
        }

        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(ref xdg)) = self.popups.find_popup(surface) {
            if !xdg.is_initial_configure_sent() {
                let _ = xdg.send_configure();
            }
        }

        self.layer_committed(surface);
        self.queue_redraw_all();
    }
}

impl State {
    /// Upload buffers early where the backend wants it (no-op for now).
    fn backend_early_import(&mut self, _surface: &WlSurface) {}

    /// A layer surface committed: send the initial configure and re-arrange
    /// its output.
    fn layer_committed(&mut self, surface: &WlSurface) {
        let output = self.outputs.iter().map(|o| o.output.clone()).find(|o| {
            let map = layer_map_for_output(o);
            map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some()
        });
        let Some(output) = output else {
            return;
        };
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<LayerSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        let mut map = layer_map_for_output(&output);
        map.arrange();
        if !initial_configure_sent {
            if let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL) {
                layer.layer_surface().send_configure();
            }
        }
        drop(map);
        self.layers_changed(&output);
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf.as_mut().expect("dmabuf without a global").state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        if super::backend::Backend::import_dmabuf(self, &dmabuf) {
            let _ = notifier.successful::<State>();
        } else {
            notifier.failed();
        }
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.add_toplevel(surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: WlSeat, _serial: Serial) {
        self.window_request(
            surface.wl_surface(),
            canoe::WindowEvent::Move(self.wm_seat_weak()),
        );
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: WlSeat,
        _serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        self.window_request(
            surface.wl_surface(),
            canoe::WindowEvent::Resize(self.wm_seat_weak(), u32::from(edges)),
        );
    }

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, serial: Serial) {
        let seat = self.seat.clone();
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Ok(mut grab) = self.popups.grab_popup(root.into(), kind, &seat, serial) else {
            return;
        };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.window_request(surface.wl_surface(), canoe::WindowEvent::Maximize);
        // The protocol wants a configure in reply even if nothing changes.
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.window_request(surface.wl_surface(), canoe::WindowEvent::Unmaximize);
        surface.send_configure();
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        let output = output.and_then(|wl| Output::from_resource(&wl));
        let output = self.wm_output_weak(output.as_ref());
        self.window_request(surface.wl_surface(), canoe::WindowEvent::Fullscreen(output));
        surface.send_configure();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.window_request(surface.wl_surface(), canoe::WindowEvent::Unfullscreen);
        surface.send_configure();
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        self.window_request(surface.wl_surface(), canoe::WindowEvent::Minimize);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        super::protocols::xdg_foreign::toplevel_destroyed(self, surface.wl_surface());
        self.remove_toplevel(surface.wl_surface());
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.toplevel_properties_changed(surface.wl_surface());
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.toplevel_properties_changed(surface.wl_surface());
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        self.toplevel_parent_changed(&surface);
    }
}

impl State {
    /// Keep a popup on its toplevel's output.
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        // Popups of windows, then of layer surfaces.
        let (origin, area) = if let Some(window) = self.window_for_surface(&root) {
            let origin = window.pos;
            let area = self
                .output_at(origin.to_f64() + Point::<f64, Logical>::from((1.0, 1.0)))
                .or_else(|| self.outputs.first().map(|o| o.output.clone()))
                .and_then(|o| self.output_geometry(&o));
            (origin, area)
        } else {
            let found = self.outputs.iter().find_map(|o| {
                let map = layer_map_for_output(&o.output);
                let layer = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
                let geo = map.layer_geometry(layer)?;
                let out = self.output_geometry(&o.output)?;
                Some((out.loc + geo.loc, Some(out)))
            });
            match found {
                Some(found) => found,
                None => return,
            }
        };
        let Some(area) = area else {
            return;
        };
        let mut target = area;
        target.loc -= get_popup_toplevel_coords(&kind);
        target.loc -= origin;
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    pub fn wm_seat_weak(&self) -> std::rc::Weak<std::cell::RefCell<canoe::Seat>> {
        let context = self.context.borrow();
        context
            .current_seat
            .and_then(|id| context.seats.get(&id))
            .map(std::rc::Rc::downgrade)
            .unwrap_or_default()
    }

    /// The window manager's output for `output`.
    pub(super) fn wm_output_weak(
        &self,
        output: Option<&Output>,
    ) -> Option<std::rc::Weak<std::cell::RefCell<canoe::Output>>> {
        let id = self.managed_output(output?)?.id;
        self.context
            .borrow()
            .outputs
            .get(&id)
            .map(std::rc::Rc::downgrade)
    }

    /// Queue a request a window made for the window manager.
    pub(super) fn window_request(&mut self, surface: &WlSurface, event: canoe::WindowEvent) {
        let Some(id) = self.window_for_surface(surface).map(|w| w.id) else {
            return;
        };
        if let Some(window) = self.context.borrow().windows.get(&id) {
            window.borrow_mut().queue_event(event);
        }
        self.schedule_wm();
    }

    fn toplevel_parent_changed(&mut self, surface: &ToplevelSurface) {
        let Some(id) = self.window_for_surface(surface.wl_surface()).map(|w| w.id) else {
            return;
        };
        let parent_id = surface
            .parent()
            .and_then(|parent| self.window_for_surface(&parent).map(|w| w.id));
        self.set_window_parent(id, parent_id);
    }

    /// Tell the window manager whose child a window is.
    pub(super) fn set_window_parent(
        &mut self,
        id: canoe::WindowId,
        parent_id: Option<canoe::WindowId>,
    ) {
        {
            let context = self.context.borrow();
            if let Some(window) = context.windows.get(&id) {
                let mut w = window.borrow_mut();
                w.parent = parent_id;
                context.apply_rules_to_window(&mut w);
            }
        }
        self.context.borrow_mut().assign_output_for_window(id);
        self.schedule_wm();
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // A client that creates a decoration object without a mode has no
        // preference.
        self.decoration_hint_changed(toplevel.wl_surface(), 3);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        let hint = match mode {
            DecorationMode::ClientSide => 1,
            DecorationMode::ServerSide => 2,
            _ => 3,
        };
        self.decoration_hint_changed(toplevel.wl_surface(), hint);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.decoration_hint_changed(toplevel.wl_surface(), 3);
    }
}

/// A surface's `org_kde_kwin_server_decoration`, through which GTK asks to
/// be decorated, and the decoration hint its last request amounts to.
#[derive(Default)]
struct KdeDecoration {
    object: Option<OrgKdeKwinServerDecoration>,
    hint: Option<u32>,
    /// The mode last sent: whether canoe decorates the surface.
    sent_ssd: Option<bool>,
}

type KdeDecorationData = Mutex<KdeDecoration>;

fn with_kde_decoration<T>(surface: &WlSurface, f: impl FnOnce(&mut KdeDecoration) -> T) -> T {
    with_states(surface, |states| {
        let data = states
            .data_map
            .get_or_insert_threadsafe(KdeDecorationData::default);
        let mut decoration = data.lock().unwrap();
        f(&mut decoration)
    })
}

/// The decoration hint a toplevel asked for through
/// `org_kde_kwin_server_decoration`, if it did so before it was managed.
pub fn kde_decoration_hint(surface: &WlSurface) -> Option<u32> {
    with_kde_decoration(surface, |decoration| decoration.hint)
}

/// Tell a toplevel's `org_kde_kwin_server_decoration`, if it has one, who
/// draws its decorations.
pub fn send_kde_decoration_mode(surface: &WlSurface, ssd: bool) {
    with_kde_decoration(surface, |decoration| {
        if let Some(object) = decoration.object.as_ref() {
            if decoration.sent_ssd != Some(ssd) {
                object.mode(if ssd {
                    KdeMode::Server
                } else {
                    KdeMode::Client
                });
                decoration.sent_ssd = Some(ssd);
            }
        }
    });
}

impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration_state
    }

    fn new_decoration(&mut self, surface: &WlSurface, decoration: &OrgKdeKwinServerDecoration) {
        with_kde_decoration(surface, |d| {
            *d = KdeDecoration {
                object: Some(decoration.clone()),
                hint: Some(3),
                sent_ssd: None,
            };
        });
        self.decoration_hint_changed(surface, 3);
    }

    fn request_mode(
        &mut self,
        surface: &WlSurface,
        _decoration: &OrgKdeKwinServerDecoration,
        mode: WEnum<KdeMode>,
    ) {
        // GTK asks for client-side decorations when it draws a header bar
        // of its own, and keeps asking whatever it is told back (it only
        // changes its decorations with a new surface), so only supporting
        // CSD is what such a request amounts to. `None` (no decorations at
        // all) is no different.
        let hint = match mode {
            WEnum::Value(KdeMode::Server) => 2,
            _ => 0,
        };
        with_kde_decoration(surface, |d| d.hint = Some(hint));
        self.decoration_hint_changed(surface, hint);
    }

    fn release(&mut self, _decoration: &OrgKdeKwinServerDecoration, surface: &WlSurface) {
        with_kde_decoration(surface, |d| *d = KdeDecoration::default());
        self.decoration_hint_changed(surface, 0);
    }
}

impl State {
    /// Decoration hints as River reports them: 0 only_supports_csd (no
    /// decoration object), 1 prefers_csd, 2 prefers_ssd, 3 no_preference.
    fn decoration_hint_changed(&mut self, surface: &WlSurface, hint: u32) {
        let Some(key) = self.window_for_surface(surface).map(|w| w.key) else {
            return;
        };
        let id = self.window_by_key(key).map(|w| w.id).unwrap();
        {
            let context = self.context.borrow();
            let Some(window) = context.windows.get(&id) else {
                return;
            };
            let mut w = window.borrow_mut();
            w.decoration_hint = Some(hint);
            context.apply_rules_to_window(&mut w);
        }
        // The mode must be sent again, as the client just (re)created its
        // decoration object or changed its mind, but only once the window
        // manager's decision is applied: a client asking for a mode right
        // after creating its decoration object gets one answer.
        if let Some(window) = self.window_by_key_mut(key) {
            window.decoration_mode_dirty = true;
            window.needs_configure = true;
        }
        self.schedule_wm();
    }
}

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = output
            .and_then(|o| Output::from_resource(&o))
            .or_else(|| {
                self.default_layer_output
                    .and_then(|key| self.output_by_key(key))
                    .map(|o| o.output.clone())
            })
            .or_else(|| self.active_outputs().next().map(|o| o.output.clone()));
        let Some(output) = output else {
            surface.send_close();
            return;
        };
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&smithay::desktop::LayerSurface::new(surface, namespace)) {
            eprintln!("canoe: failed to map layer surface: {err}");
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let found = self.outputs.iter().map(|o| o.output.clone()).find_map(|o| {
            let map = layer_map_for_output(&o);
            let layer = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned()?;
            drop(map);
            Some((o, layer))
        });
        if let Some((output, layer)) = found {
            layer_map_for_output(&output).unmap_layer(&layer);
            self.layers_changed(&output);
        }
        if self
            .layer_focus
            .as_ref()
            .is_some_and(|(focused, _)| focused == surface.wl_surface())
        {
            self.release_layer_focus();
        }
        self.queue_redraw_all();
    }
}

impl SeatHandler for State {
    type KeyboardFocus = super::focus::KeyboardFocus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.client_cursor = image;
        self.queue_redraw_all();
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&super::focus::KeyboardFocus>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|f| dh.get_client(f.surface().id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        // Wayland clients' selections are offered to X11 clients too.
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(target, source.map(|s| s.mime_types())) {
                eprintln!("canoe: failed to pass the selection to Xwayland: {err}");
            }
        }
    }

    fn send_selection(
        &mut self,
        target: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        // A Wayland client reads the selection an X11 client holds.
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(target, mime_type, fd, self.loop_handle.clone()) {
                eprintln!("canoe: failed to read the selection from Xwayland: {err}");
            }
        }
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl DataControlHandler for State {
    fn data_control_state(&self) -> &DataControlState {
        &self.data_control_state
    }
}

impl OutputHandler for State {
    fn output_bound(&mut self, output: Output, wl_output: WlOutput) {
        self.foreign_toplevel_manager_state
            .output_bound(&output, &wl_output);
    }
}

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // Tell the surface the scale of the output it is (mostly) on; the
        // render loop keeps it updated when that changes.
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        // A window that isn't shown yet opens on the window manager's
        // current output: lay it out for that one right away.
        let current_output = {
            let context = self.context.borrow();
            context
                .current_output
                .and_then(|id| self.output_by_id(id))
                .map(|o| o.output.clone())
        };
        let mapped = self
            .window_for_surface(&root)
            .is_some_and(|w| w.reported_size.is_some());
        let output = if mapped {
            self.output_for_surface(&root)
        } else {
            current_output.or_else(|| self.output_for_surface(&root))
        };
        let scale = output
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0);
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, _data: XdgActivationTokenData) -> bool {
        true
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Only honour fresh tokens, as a click on a notification or launcher
        // gives; River leaves activation to the window manager, which focuses
        // windows it is asked to focus in its manage sequence.
        if token_data.timestamp.elapsed().as_secs() >= 10 {
            return;
        }
        let Some(id) = self.window_for_surface(&surface).map(|w| w.id) else {
            return;
        };
        {
            let mut context = self.context.borrow_mut();
            if context.windows.get(&id).is_some_and(|w| w.borrow().hidden) {
                drop(context);
                return;
            }
            context.focus(id);
        }
        self.schedule_wm();
    }
}

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        self.lock_state = LockState::Locking;
        self.locker = Some(confirmation);
        self.session_locked_changed(true);
        self.check_lock_complete();
        // Don't keep the locker waiting on outputs that never show it.
        self.loop_handle
            .insert_source(
                smithay::reexports::calloop::timer::Timer::from_duration(
                    std::time::Duration::from_millis(500),
                ),
                |_, _, state| {
                    if let Some(locker) = state.locker.take() {
                        locker.lock();
                        state.lock_state = LockState::Locked;
                    }
                    smithay::reexports::calloop::timer::TimeoutAction::Drop
                },
            )
            .ok();
    }

    fn unlock(&mut self) {
        self.lock_state = LockState::Unlocked;
        self.locker = None;
        for output in &mut self.outputs {
            output.lock_surface = None;
        }
        self.session_locked_changed(false);
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            return;
        };
        let Some(managed) = self.outputs.iter_mut().find(|o| o.output == output) else {
            return;
        };
        if let Some(mode) = output.current_mode() {
            let scale = output.current_scale().fractional_scale();
            let size = mode.size.to_f64().to_logical(scale).to_i32_round::<i32>();
            surface.with_pending_state(|state| {
                state.size = Some((size.w as u32, size.h as u32).into());
            });
        }
        surface.send_configure();
        managed.lock_surface = Some(surface.clone());
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(
            self,
            Some(surface.wl_surface().clone().into()),
            Serial::from(0),
        );
        self.check_lock_complete();
        self.queue_redraw_all();
    }
}

impl State {
    fn check_lock_complete(&mut self) {
        if self.lock_state != LockState::Locking {
            return;
        }
        let all = self.active_outputs().all(|o| o.lock_surface.is_some());
        if all {
            if let Some(locker) = self.locker.take() {
                locker.lock();
            }
            self.lock_state = LockState::Locked;
        }
    }
}

impl smithay::wayland::tablet_manager::TabletSeatHandler for State {}

impl ForeignToplevelListHandler for State {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_state
    }
}

impl IdleNotifierHandler for State {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

impl IdleInhibitHandler for State {
    fn inhibit(&mut self, _surface: WlSurface) {
        self.idle_notifier_state.set_is_inhibited(true);
    }

    fn uninhibit(&mut self, _surface: WlSurface) {
        self.idle_notifier_state.set_is_inhibited(false);
    }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(
        &mut self,
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        _location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
    }
}

impl State {
    /// The output whose area contains `point`.
    pub fn output_at(
        &self,
        point: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<Output> {
        self.active_outputs()
            .find(|o| {
                self.output_geometry(&o.output)
                    .is_some_and(|geo| geo.to_f64().contains(point))
            })
            .map(|o| o.output.clone())
    }

    /// An output's area in the global logical layout.
    pub fn output_geometry(
        &self,
        output: &Output,
    ) -> Option<Rectangle<i32, smithay::utils::Logical>> {
        let mode = output.current_mode()?;
        let scale = output.current_scale().fractional_scale();
        let size = mode.size.to_f64().to_logical(scale).to_i32_round::<i32>();
        let size = output.current_transform().transform_size(size);
        Some(Rectangle::new(output.current_location(), size))
    }

    /// The output a toplevel or layer surface is (mostly) shown on.
    pub fn output_for_surface(&self, root: &WlSurface) -> Option<Output> {
        if let Some(window) = self.window_for_surface(root) {
            let center = window.pos.to_f64()
                + window
                    .window
                    .geometry()
                    .size
                    .to_f64()
                    .downscale(2.0)
                    .to_point();
            return self
                .output_at(center)
                .or_else(|| self.active_outputs().next().map(|o| o.output.clone()));
        }
        self.outputs.iter().map(|o| o.output.clone()).find(|o| {
            layer_map_for_output(o)
                .layer_for_surface(root, WindowSurfaceType::ALL)
                .is_some()
        })
    }
}

delegate_compositor!(State);
delegate_xdg_shell!(State);
delegate_xdg_decoration!(State);
delegate_kde_decoration!(State);
delegate_layer_shell!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_primary_selection!(State);
delegate_data_control!(State);
delegate_output!(State);
delegate_viewporter!(State);
delegate_fractional_scale!(State);
delegate_cursor_shape!(State);
delegate_xdg_activation!(State);
delegate_session_lock!(State);
delegate_foreign_toplevel_list!(State);
delegate_presentation!(State);
delegate_single_pixel_buffer!(State);
delegate_idle_notify!(State);
delegate_idle_inhibit!(State);
delegate_pointer_constraints!(State);
delegate_relative_pointer!(State);
delegate_dmabuf!(State);
smithay::delegate_shm!(State);
smithay::delegate_xwayland_shell!(State);
