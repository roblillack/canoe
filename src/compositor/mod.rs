//! Canoe's compositor, built on Smithay.
//!
//! The compositor owns the Wayland display, the outputs and the input
//! devices, and hosts canoe's window manager (`crate::canoe::Context`) in
//! process. It reports what clients and devices do to the window manager the
//! way River's window-management protocol did, runs the window manager's
//! manage and render sequences when something changed, and carries out what
//! the window manager asked for (see `crate::wm`).

pub mod apply;
pub mod backend;
pub mod capture;
pub mod cursor;
pub mod focus;
pub mod handlers;
pub mod input;
pub mod kwin_screenshot;
pub mod memory;
pub mod output_profiles;
pub mod protocol_handlers;
pub mod protocols;
pub mod render;
pub mod sway_ipc;
pub mod taskbar;
pub mod warp;
pub mod wm_events;
pub mod wm_glue;
pub mod xwayland;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::desktop::{PopupManager, Window};
use smithay::input::keyboard::{Keycode, XkbConfig};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, LoopSignal, Mode, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_data_device_manager::DndAction;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelListState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::idle_notify::IdleNotifierState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::pointer_constraints::PointerConstraintsState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::selection::wlr_data_control::DataControlState;
use smithay::wayland::session_lock::{LockSurface, SessionLockManagerState};
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode;
use smithay::wayland::shell::kde::decoration::KdeDecorationState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellState};
use smithay::wayland::shm::ShmState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xdg_activation::XdgActivationState;

use crate::canoe::{self, Context};
use crate::wm::{self, ClipBox, CursorShape, OutputKey, Requests, WindowKey};

/// Where a window goes once its client drew the size proposed with it.
#[derive(Clone, Copy, Debug)]
pub struct PendingPosition {
    pub pos: Point<i32, Logical>,
    /// The size proposed along with the position.
    pub size: Size<i32, Logical>,
    /// Keep the right and the bottom edge where the window manager put them
    /// should the client draw another size than proposed, as a terminal
    /// does that only takes whole cells. Set while the user drags the left
    /// or top edge.
    pub anchor: (bool, bool),
    /// Since when it waits.
    pub since: Instant,
}

/// What a window's client draws it with.
pub enum Shell {
    Xdg(ToplevelSurface),
    X11(smithay::xwayland::X11Surface),
}

/// A toplevel window the compositor shows.
pub struct ManagedWindow {
    pub key: WindowKey,
    /// The window manager's id for it.
    pub id: canoe::WindowId,
    pub window: Window,
    pub shell: Shell,
    /// The window's main surface (for X11 windows, the one Xwayland
    /// associated with it).
    pub surface: WlSurface,
    /// The size the window manager wants an X11 window to have; `None`
    /// leaves it the size the client picked.
    pub x11_size: Option<Size<i32, Logical>>,
    pub foreign: Option<ForeignToplevelHandle>,
    /// The window as taskbars see it (wlr-foreign-toplevel-management),
    /// announced along with `foreign`.
    pub wlr_toplevel: Option<protocols::foreign_toplevel::WlrToplevel>,
    /// Where the window manager put the content's top left corner.
    pub pos: Point<i32, Logical>,
    pub hidden: bool,
    /// The window manager told the client it is maximized.
    pub maximized: bool,
    /// The output the window is fullscreen on.
    pub fullscreen: Option<OutputKey>,
    pub clip: ClipBox,
    pub content_clip: ClipBox,
    /// Decorated by the window manager (as opposed to by the client).
    pub ssd: bool,
    /// The size last reported to the window manager.
    pub reported_size: Option<Size<i32, Logical>>,
    /// min/max size last reported to the window manager.
    pub reported_hints: Option<(i32, i32, i32, i32)>,
    /// Pending state changed; a configure must be sent.
    pub needs_configure: bool,
    /// The decoration mode must be told to the client, with the next
    /// configure.
    pub decoration_mode_dirty: bool,
    /// Positions to apply only once the client has drawn the sizes proposed
    /// along with them, oldest first, so resizing from the left or top edge
    /// doesn't make the content jitter (River does this with transactions).
    pub pending_positions: Vec<PendingPosition>,
    /// The client made its initial commit.
    pub initial_commit: bool,
    /// The window is animated to a new position and size, or to or from
    /// its desktop icon.
    pub warp: Option<warp::Warp>,
    /// Where the window's desktop icon was last shown, globally.
    pub icon: Option<smithay::utils::Rectangle<i32, Logical>>,
}

impl Shell {
    /// The process id an X11 client announced.
    pub fn x11_pid(&self) -> Option<u32> {
        match self {
            Shell::Xdg(_) => None,
            Shell::X11(x11) => x11.pid(),
        }
    }
}

impl ManagedWindow {
    pub fn wl_surface(&self) -> &WlSurface {
        &self.surface
    }

    /// Where the window manager last put the window, even if it still
    /// waits for its client there.
    pub fn target_position(&self) -> Point<i32, Logical> {
        self.pending_positions.last().map_or(self.pos, |p| p.pos)
    }

    /// Move the window to the newest waiting position whose size the client
    /// acknowledged and drew now, or to the newest one if the oldest waited
    /// too long. Returns whether the window moved somewhere else than the
    /// window manager put it, to keep an edge in place.
    pub fn settle_position(&mut self) -> bool {
        let acked = self
            .toplevel()
            .and_then(|toplevel| toplevel.current_state().size);
        let ready = acked.and_then(|acked| {
            self.pending_positions
                .iter()
                .rposition(|pending| pending.size == acked)
        });
        let expired = self
            .pending_positions
            .first()
            .is_some_and(|p| p.since.elapsed() >= wm_events::POSITION_SYNC_TIMEOUT);
        let index = match ready {
            Some(index) => index,
            None if expired => self.pending_positions.len() - 1,
            None => return false,
        };
        let pending = self.pending_positions[index];
        self.pending_positions.drain(..=index);
        // The size the client took is final, even if it is another than
        // proposed; keep the edge opposite of the one dragged in place.
        let drawn = self.window.geometry().size;
        let mut pos = pending.pos;
        if ready.is_some() {
            if pending.anchor.0 {
                pos.x += pending.size.w - drawn.w;
            }
            if pending.anchor.1 {
                pos.y += pending.size.h - drawn.h;
            }
        }
        self.pos = pos;
        pos != pending.pos
    }

    /// Tell the client whether canoe decorates it, as `ssd` says.
    pub fn send_decoration_mode(&self) {
        let Some(toplevel) = self.toplevel() else {
            return;
        };
        let ssd = self.ssd;
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(if ssd {
                DecorationMode::ServerSide
            } else {
                DecorationMode::ClientSide
            });
        });
        handlers::send_kde_decoration_mode(toplevel.wl_surface(), ssd);
    }

    pub fn toplevel(&self) -> Option<&ToplevelSurface> {
        match &self.shell {
            Shell::Xdg(toplevel) => Some(toplevel),
            Shell::X11(_) => None,
        }
    }

    pub fn x11(&self) -> Option<&smithay::xwayland::X11Surface> {
        match &self.shell {
            Shell::Xdg(_) => None,
            Shell::X11(x11) => Some(x11),
        }
    }

    /// The client may be sent configures: an xdg toplevel made its initial
    /// commit (X11 windows can always be configured).
    pub fn configurable(&self) -> bool {
        match &self.shell {
            Shell::Xdg(toplevel) => toplevel.is_initial_configure_sent(),
            Shell::X11(_) => true,
        }
    }

    /// Where the window's surface (not its geometry) has its origin.
    pub fn surface_origin(
        &self,
        fullscreen_at: Option<Point<i32, Logical>>,
    ) -> Point<i32, Logical> {
        fullscreen_at.unwrap_or(self.pos) - self.window.geometry().loc
    }
}

/// An output the compositor shows things on.
pub struct ManagedOutput {
    pub key: OutputKey,
    pub id: canoe::OutputId,
    pub output: Output,
    pub enabled: bool,
    /// Logical geometry last reported to the window manager.
    pub reported: Option<(i32, i32, i32, i32)>,
    /// Non-exclusive area last reported to the window manager.
    pub reported_usable: Option<(i32, i32, i32, i32)>,
    pub lock_surface: Option<LockSurface>,
    /// The wl_output global, while the output is enabled.
    pub global: Option<smithay::reexports::wayland_server::backend::GlobalId>,
    /// Not turned off through output power management. Only meaningful
    /// while enabled; a disabled output is powered on again when enabled.
    pub powered: bool,
    pub redraw_needed: bool,
    pub frame_pending: bool,
}

impl ManagedOutput {
    /// Whether the output is drawn: part of the layout and powered on.
    /// Clients on an output that isn't drawn get no frame callbacks.
    pub fn displayed(&self) -> bool {
        self.enabled && self.powered
    }
}

/// A pointer operation in progress (an interactive move or resize the
/// window manager drives).
pub struct PointerOp {
    pub start: Point<f64, Logical>,
    pub released: bool,
}

/// A drag a client is doing, from `started` to `dropped`.
pub struct Dnd {
    /// The icon it carries along under the pointer, if it gave one.
    pub icon: Option<DndIcon>,
    /// The surface under the pointer, which the drag is offered to.
    pub target: Option<WlSurface>,
    /// What a drop on `target` would do, as it chose: empty until it chooses,
    /// and when it won't take the drag.
    pub action: DndAction,
}

impl Dnd {
    /// The cursor showing what a drop would do: the drag-and-drop cursors
    /// themes have, fallen back on as `cursor` does.
    pub fn cursor_name(&self) -> &'static str {
        if self.action == DndAction::Copy {
            "dnd-copy"
        } else if self.action == DndAction::Move {
            "dnd-move"
        } else if self.action == DndAction::Ask {
            "dnd-ask"
        } else {
            "dnd-no-drop"
        }
    }
}

/// The icon a client's drag carries along under the pointer.
pub struct DndIcon {
    pub surface: WlSurface,
    /// Where its top left corner is from the pointer: the offsets of the
    /// buffers the client attached to it, added up.
    pub offset: Point<i32, Logical>,
}

/// Where a key press went, so its release goes there too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyConsumer {
    /// A binding of the window manager's: the seat's binding index, or
    /// `None` if it already fired a press without a release.
    Binding(Option<usize>),
    Client,
}

/// Lock state of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    Unlocked,
    /// A locker asked to lock; locked once every output shows a lock surface
    /// (or a short timeout passes).
    Locking,
    Locked,
}

#[allow(dead_code)] // protocol states are kept for their globals
pub struct State {
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, State>,
    pub loop_signal: LoopSignal,
    pub socket_name: OsString,
    pub start_time: Instant,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    /// KDE's server decoration protocol, through which GTK asks to be
    /// decorated; it doesn't use xdg-decoration.
    pub kde_decoration_state: KdeDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    pub output_manager_state: OutputManagerState,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub cursor_shape_state: CursorShapeManagerState,
    pub xdg_activation_state: XdgActivationState,
    pub session_lock_state: SessionLockManagerState,
    pub foreign_toplevel_state: ForeignToplevelListState,
    pub foreign_toplevel_manager_state: protocols::foreign_toplevel::ForeignToplevelManagerState,
    pub presentation_state: PresentationState,
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub idle_notifier_state: IdleNotifierState<State>,
    pub idle_inhibit_state: IdleInhibitManagerState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub relative_pointer_state: RelativePointerManagerState,
    pub screencopy_state: protocols::screencopy::ScreencopyManagerState,
    pub image_capture_state: protocols::image_capture::ImageCopyCaptureState,
    pub output_management_state: protocols::output_management::OutputManagementState,
    pub output_power_state: protocols::output_power::OutputPowerManagerState,
    pub gamma_control_state: protocols::gamma_control::GammaControlManagerState,
    pub virtual_pointer_state: protocols::virtual_pointer::VirtualPointerManagerState,
    pub xdg_foreign_state: protocols::xdg_foreign::XdgForeignState,
    pub toplevel_icon_state: protocols::toplevel_icon::ToplevelIconManagerState,
    pub plasma_shell_state: protocols::plasma_shell::PlasmaShellState,
    pub primary_output_state: protocols::primary_output::PrimaryOutputState,
    /// Output layouts remembered per set of monitors.
    pub output_profiles: output_profiles::OutputProfiles,
    pub dmabuf: Option<handlers::DmabufGlobal>,
    pub popups: PopupManager,
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// The window manager of the Xwayland server, once it is up.
    pub xwm: Option<smithay::xwayland::X11Wm>,
    /// X11 windows mapped but still waiting for Xwayland to associate their
    /// surface.
    pub x11_pending: Vec<smithay::xwayland::X11Surface>,
    /// X11 windows not managed by the window manager (menus, tooltips and
    /// other override-redirect windows), bottom first.
    pub x11_unmanaged: Vec<smithay::xwayland::X11Surface>,
    /// The stacking order last given to the X server, bottom first.
    pub x11_stacking: Vec<smithay::xwayland::xwm::X11Window>,
    /// The Xwayland server's Wayland client, once it is started.
    pub xwayland_client: Option<smithay::reexports::wayland_server::Client>,
    /// What X11 programs were last told about scale and cursor.
    pub x11_settings: Option<xwayland::X11Settings>,
    /// Writes the X resources, once Xwayland is up.
    pub x11_resources: Option<xwayland::ResourceWriter>,

    pub seat: Seat<State>,

    /// The window manager.
    pub context: Rc<RefCell<Context>>,
    /// What the window manager asked for, still to be done.
    pub requests: Requests,
    pub next_key: u64,
    /// Windows in rendering order, bottom first.
    pub windows: Vec<ManagedWindow>,
    pub outputs: Vec<ManagedOutput>,
    /// Layer surfaces without an output open here.
    pub default_layer_output: Option<OutputKey>,

    /// A manage sequence is due.
    pub wm_dirty: bool,
    pub wm_scheduled: bool,
    /// The window manager asked for keyboard focus on this window.
    pub wm_focus: Option<WindowKey>,
    pub pointer_op: Option<PointerOp>,
    pub key_consumers: HashMap<Keycode, KeyConsumer>,
    /// Pointer buttons bound by the window manager and held, with the
    /// binding they pressed.
    pub button_consumers: HashMap<u32, usize>,
    pub pressed_buttons: Vec<u32>,
    /// Modifiers held, as River's modifier bits, as of the last key event.
    pub key_modifiers: u32,
    /// A button held on one of the window manager's surfaces, which gets
    /// the pointer until it is released (an implicit grab).
    pub canoe_grab: Option<(canoe::PointerTarget, Option<canoe::WindowId>)>,
    /// What the pointer is over, as last reported to the window manager.
    pub pointer_window: Option<canoe::WindowId>,
    pub pointer_canoe_target: canoe::PointerTarget,
    pub pointer_location: Point<f64, Logical>,
    /// The cursor the window manager asked for over its own surfaces and
    /// during pointer operations; `None` hides it.
    pub wm_cursor: Option<CursorShape>,
    pub client_cursor: smithay::input::pointer::CursorImageStatus,
    /// The drag a client is doing, while it lasts.
    pub dnd: Option<Dnd>,
    pub cursor: cursor::CursorTheme,
    /// Keyboard focus held by a layer surface (exclusive or on demand).
    pub layer_focus: Option<(WlSurface, bool)>,

    pub lock_state: LockState,
    pub locker: Option<smithay::wayland::session_lock::SessionLocker>,
    /// The lock that is locking or holds the session locked. Another locker
    /// is refused while its client is around.
    pub session_lock: Option<
        smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
    >,

    /// Where `canoe --share-picker` requests arrive.
    pub share_server: Option<crate::ipc::ShareServer>,
    /// KWin's screenshot interface, while emulated (`emulate_kwin`).
    pub kwin_screenshot: Option<kwin_screenshot::KwinScreenshot>,
    /// sway's IPC socket, while emulated (`emulate_sway`).
    pub sway_ipc: Option<sway_ipc::SwayIpc>,
    pub share_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Trims the heap after windows closed; see [`memory`].
    pub trim_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// The startup commands are still to be launched.
    pub startup_pending: bool,

    /// Black backdrops behind fullscreen windows, per output.
    pub backdrop_ids:
        HashMap<OutputKey, smithay::backend::renderer::element::solid::SolidColorBuffer>,
    /// The backend draws the cursor itself (a nested window's host cursor).
    pub cursor_hidden_by_backend: bool,
    /// Leave the cursor out (for screen captures without it).
    pub suppress_cursor: bool,

    pub backend: backend::Backend,
}

/// Per-client data.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl State {
    pub fn new(
        display: Display<State>,
        loop_handle: LoopHandle<'static, State>,
        loop_signal: LoopSignal,
        skip_config: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<State>(&dh);
        let xdg_shell_state = XdgShellState::new::<State>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<State>(&dh);
        let kde_decoration_state = KdeDecorationState::new::<State>(&dh, KdeDefaultMode::Server);
        let layer_shell_state = WlrLayerShellState::new::<State>(&dh);
        let shm_state = ShmState::new::<State>(&dh, vec![]);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<State>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<State>(&dh);
        let data_control_state =
            DataControlState::new::<State, _>(&dh, Some(&primary_selection_state), |_| true);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<State>(&dh);
        let viewporter_state = ViewporterState::new::<State>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<State>(&dh);
        let cursor_shape_state = CursorShapeManagerState::new::<State>(&dh);
        let xdg_activation_state = XdgActivationState::new::<State>(&dh);
        let session_lock_state = SessionLockManagerState::new::<State, _>(&dh, |_| true);
        let foreign_toplevel_state = ForeignToplevelListState::new::<State>(&dh);
        let foreign_toplevel_manager_state =
            protocols::foreign_toplevel::ForeignToplevelManagerState::new::<State>(&dh);
        let presentation_state = PresentationState::new::<State>(&dh, libc_clock_monotonic());
        let single_pixel_buffer_state = SinglePixelBufferState::new::<State>(&dh);
        let idle_notifier_state = IdleNotifierState::new(&dh, loop_handle.clone());
        let idle_inhibit_state = IdleInhibitManagerState::new::<State>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<State>(&dh);
        let relative_pointer_state = RelativePointerManagerState::new::<State>(&dh);
        let screencopy_state = protocols::screencopy::ScreencopyManagerState::new::<State>(&dh);
        let image_capture_state =
            protocols::image_capture::ImageCopyCaptureState::new::<State>(&dh);
        let output_management_state =
            protocols::output_management::OutputManagementState::new::<State>(&dh);
        let output_power_state =
            protocols::output_power::OutputPowerManagerState::new::<State>(&dh);
        let gamma_control_state =
            protocols::gamma_control::GammaControlManagerState::new::<State>(&dh);
        let virtual_pointer_state =
            protocols::virtual_pointer::VirtualPointerManagerState::new::<State, _>(&dh, |_| true);
        protocols::virtual_keyboard::VirtualKeyboardManagerState::new::<State>(&dh);
        smithay::wayland::xdg_toplevel_tag::XdgToplevelTagManager::new::<State>(&dh);
        let xdg_foreign_state = protocols::xdg_foreign::XdgForeignState::new::<State>(&dh);
        let toplevel_icon_state =
            protocols::toplevel_icon::ToplevelIconManagerState::new::<State>(&dh);
        let plasma_shell_state = protocols::plasma_shell::PlasmaShellState::new::<State>(&dh);
        let primary_output_state = protocols::primary_output::PrimaryOutputState::new::<State>(&dh);
        let xwayland_shell_state =
            smithay::wayland::xwayland_shell::XWaylandShellState::new::<State>(&dh);

        let context = Rc::new(RefCell::new(Context::new(skip_config)));
        let requests = Requests::default();
        context.borrow_mut().rwm = Some(wm::WindowManager::new(requests.clone()));

        let mut seat: Seat<State> = seat_state.new_wl_seat(&dh, "seat0");
        {
            let config = &context.borrow().config;
            seat.add_keyboard(
                XkbConfig::default(),
                config.repeat_delay,
                config.repeat_rate,
            )?;
        }
        seat.add_pointer();

        // Listen for clients.
        let source = ListeningSocketSource::new_auto()?;
        let socket_name = source.socket_name().to_os_string();
        loop_handle.insert_source(source, |stream, _, state: &mut State| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                eprintln!("canoe: failed to add client: {err}");
            }
        })?;
        loop_handle.insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state: &mut State| {
                // Safety: the display is not dropped while dispatching.
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            },
        )?;

        let theme = context.borrow().config.xcursor_theme_resolved();
        let cursor = cursor::CursorTheme::new(theme.name_or_default(), theme.size_or_default());

        let state = State {
            display_handle: dh,
            loop_handle,
            loop_signal,
            socket_name,
            start_time: Instant::now(),
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            kde_decoration_state,
            layer_shell_state,
            shm_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            data_control_state,
            output_manager_state,
            viewporter_state,
            fractional_scale_state,
            cursor_shape_state,
            xdg_activation_state,
            session_lock_state,
            foreign_toplevel_state,
            foreign_toplevel_manager_state,
            presentation_state,
            single_pixel_buffer_state,
            idle_notifier_state,
            idle_inhibit_state,
            pointer_constraints_state,
            relative_pointer_state,
            screencopy_state,
            image_capture_state,
            output_management_state,
            output_power_state,
            gamma_control_state,
            virtual_pointer_state,
            xdg_foreign_state,
            toplevel_icon_state,
            plasma_shell_state,
            primary_output_state,
            output_profiles: Default::default(),
            dmabuf: None,
            popups: PopupManager::default(),
            xwayland_shell_state,
            xwm: None,
            x11_pending: Vec::new(),
            x11_unmanaged: Vec::new(),
            x11_stacking: Vec::new(),
            xwayland_client: None,
            x11_settings: None,
            x11_resources: None,
            seat,
            context,
            requests,
            next_key: 1,
            windows: Vec::new(),
            outputs: Vec::new(),
            default_layer_output: None,
            wm_dirty: false,
            wm_scheduled: false,
            wm_focus: None,
            pointer_op: None,
            key_consumers: HashMap::new(),
            button_consumers: HashMap::new(),
            pressed_buttons: Vec::new(),
            key_modifiers: 0,
            canoe_grab: None,
            pointer_window: None,
            pointer_canoe_target: canoe::PointerTarget::None,
            pointer_location: (0.0, 0.0).into(),
            wm_cursor: Some(CursorShape::Default),
            client_cursor: smithay::input::pointer::CursorImageStatus::default_named(),
            dnd: None,
            cursor,
            layer_focus: None,
            lock_state: LockState::Unlocked,
            locker: None,
            session_lock: None,
            share_server: None,
            kwin_screenshot: None,
            sway_ipc: None,
            share_timer: None,
            trim_timer: None,
            startup_pending: true,
            backdrop_ids: HashMap::new(),
            cursor_hidden_by_backend: false,
            suppress_cursor: false,
            backend: backend::Backend::None,
        };

        // The window manager's seat.
        let seat_handle = wm::SeatHandle::new(0, state.requests.clone());
        let seat = state.context.borrow_mut().create_seat(seat_handle);
        {
            let theme = state.context.borrow().config.xcursor_theme_resolved();
            seat.borrow().set_xcursor_theme(&theme);
        }

        Ok(state)
    }

    pub fn next_key(&mut self) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        key
    }

    pub fn window_by_key(&self, key: WindowKey) -> Option<&ManagedWindow> {
        self.windows.iter().find(|w| w.key == key)
    }

    pub fn window_by_key_mut(&mut self, key: WindowKey) -> Option<&mut ManagedWindow> {
        self.windows.iter_mut().find(|w| w.key == key)
    }

    pub fn window_by_id(&self, id: canoe::WindowId) -> Option<&ManagedWindow> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<&ManagedWindow> {
        self.windows.iter().find(|w| w.wl_surface() == surface)
    }

    pub fn output_by_key(&self, key: OutputKey) -> Option<&ManagedOutput> {
        self.outputs.iter().find(|o| o.key == key)
    }

    pub fn output_by_id(&self, id: canoe::OutputId) -> Option<&ManagedOutput> {
        self.outputs.iter().find(|o| o.id == id)
    }

    pub fn managed_output(&self, output: &Output) -> Option<&ManagedOutput> {
        self.outputs.iter().find(|o| &o.output == output)
    }

    /// Outputs that are shown, i.e. part of the layout.
    pub fn active_outputs(&self) -> impl Iterator<Item = &ManagedOutput> {
        self.outputs.iter().filter(|o| o.enabled)
    }

    /// Ask for a frame on every output.
    pub fn queue_redraw_all(&mut self) {
        for output in &mut self.outputs {
            output.redraw_needed = true;
        }
        backend::Backend::schedule_redraws(self);
    }

    /// Make sure a manage sequence runs soon, once the current batch of
    /// events is handled.
    pub fn schedule_wm(&mut self) {
        self.wm_dirty = true;
        if self.wm_scheduled {
            return;
        }
        self.wm_scheduled = true;
        self.loop_handle.insert_idle(|state| {
            state.wm_scheduled = false;
            state.run_wm();
        });
    }

    /// Send clients the frame callbacks and other per-frame state after
    /// rendering `output`.
    pub fn post_render(&mut self, output: &Output) {
        self.advance_warps();
        let time = self.start_time.elapsed();
        for window in &self.windows {
            if window.hidden {
                continue;
            }
            window
                .window
                .send_frame(output, time, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
        }
        let map = smithay::desktop::layer_map_for_output(output);
        for layer in map.layers() {
            layer.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        drop(map);
        if let Some(managed) = self.managed_output(output) {
            if let Some(lock) = managed.lock_surface.as_ref() {
                smithay::desktop::utils::send_frames_surface_tree(
                    lock.wl_surface(),
                    output,
                    time,
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
        }
        for (surface, _) in self.x11_unmanaged_surfaces() {
            smithay::desktop::utils::send_frames_surface_tree(
                &surface,
                output,
                time,
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
        if let smithay::input::pointer::CursorImageStatus::Surface(ref surface) = self.client_cursor
        {
            smithay::desktop::utils::send_frames_surface_tree(
                surface,
                output,
                time,
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
        if let Some(icon) = self.dnd.as_ref().and_then(|d| d.icon.as_ref()) {
            smithay::desktop::utils::send_frames_surface_tree(
                &icon.surface,
                output,
                time,
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }

    /// Housekeeping once per loop iteration.
    pub fn refresh(&mut self) {
        self.popups.cleanup();
        for output in self
            .outputs
            .iter()
            .map(|o| o.output.clone())
            .collect::<Vec<_>>()
        {
            smithay::desktop::layer_map_for_output(&output).cleanup();
        }
        self.foreign_toplevel_state.cleanup_closed_handles();
        // Windows and outputs move under a pointer that stays put.
        self.update_capture_cursors();
        let _ = self.display_handle.flush_clients();
    }
}

fn libc_clock_monotonic() -> u32 {
    // CLOCK_MONOTONIC
    1
}
