//! X11 programs, through an Xwayland server canoe starts and is the window
//! manager of.
//!
//! X11 windows the window manager manages become windows like any other
//! (`super::Shell::X11`), once Xwayland associated the surface it draws
//! them with. Menus, tooltips and other override-redirect windows place
//! themselves and are drawn above all windows (`State::x11_unmanaged`).
//!
//! On scaled outputs X11 programs draw at the outputs' scale, rounded up
//! (unless `xwayland_hidpi` is off): Xwayland's client scale makes the
//! outputs as many X pixels wide, and X resources and XSETTINGS tell
//! programs to draw that much bigger. Smithay converts between X pixels and
//! logical coordinates for everything else.

use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::sync::mpsc;

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, current_data_device_selection_userdata,
    request_data_device_client_selection, set_data_device_selection,
};
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, current_primary_selection_userdata, request_primary_client_selection,
    set_primary_selection,
};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::settings::Value;
use smithay::xwayland::xwm::{Reorder, ResizeEdge, WmWindowProperty, WmWindowType, XwmId};
use smithay::xwayland::{
    X11Surface, X11Wm, XWayland, XWaylandClientData, XWaylandEvent, XwmHandler,
};

use super::focus::KeyboardFocus;
use super::{ManagedWindow, Shell, State};
use crate::canoe;

/// The decoration hint (as River reports them, see
/// `State::decoration_hint_changed`) of an X11 window: the window manager
/// decorates it unless the client turned that off through its Motif hints,
/// to draw its own decorations or none. Unlike a Wayland client's, that
/// choice can't be renegotiated, so it counts as only supporting CSD.
pub fn decoration_hint(window: &X11Surface) -> u32 {
    if window.is_decorated() {
        0
    } else {
        2
    }
}

/// Windows of these types place themselves, like override-redirect windows.
fn is_unmanaged_type(window: &X11Surface) -> bool {
    matches!(
        window.window_type(),
        Some(
            WmWindowType::DropdownMenu
                | WmWindowType::PopupMenu
                | WmWindowType::Menu
                | WmWindowType::Tooltip
                | WmWindowType::Notification
        )
    )
}

/// Whether two handles are the same X11 window (unlike `==`, also once the
/// window is destroyed).
fn same_window(a: &X11Surface, b: &X11Surface) -> bool {
    a.xwm_id() == b.xwm_id() && a.window_id() == b.window_id()
}

/// Let root's X11 clients in as well, so `sudo xterm` runs. Xwayland only
/// admits clients of the user it runs as; root could get in as that user
/// anyway, so this gives nobody anything new. On a thread of its own, so a
/// busy Xwayland doesn't hold up the compositor.
fn let_root_connect(display_number: u32) {
    use smithay::reexports::x11rb::protocol::xproto::{ConnectionExt, Family, HostMode};
    use smithay::reexports::x11rb::rust_connection::RustConnection;

    let spawned = std::thread::Builder::new()
        .name("xwayland-access".into())
        .spawn(move || {
            let result = RustConnection::connect(Some(&format!(":{display_number}")))
                .map_err(|err| err.to_string())
                .and_then(|(conn, _)| {
                    // "localuser\0root" is what `xhost +si:localuser:root` sends.
                    conn.change_hosts(
                        HostMode::INSERT,
                        Family::SERVER_INTERPRETED,
                        b"localuser\0root",
                    )
                    .map_err(|err| err.to_string())?
                    .check()
                    .map_err(|err| err.to_string())
                });
            if let Err(err) = result {
                eprintln!("canoe: failed to let root's X11 programs connect: {err}");
            }
        });
    if let Err(err) = spawned {
        eprintln!("canoe: failed to let root's X11 programs connect: {err}");
    }
}

/// DPI at scale 1, as X11 programs assume it.
const X11_DPI: i32 = 96;

/// What X11 programs are told: the scale they draw at and the cursor they
/// show.
#[derive(Clone, Debug, PartialEq)]
pub struct X11Settings {
    pub scale: i32,
    pub cursor_theme: String,
    /// At scale 1.
    pub cursor_size: u32,
}

impl X11Settings {
    fn cursor_pixels(&self) -> i32 {
        self.cursor_size as i32 * self.scale
    }

    /// X resources, as `xrdb` sets them. Programs read them when they
    /// start: Xft fonts and Qt scale by `Xft.dpi`.
    fn resources(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Xft.dpi", (X11_DPI * self.scale).to_string()),
            ("Xcursor.theme", self.cursor_theme.clone()),
            ("Xcursor.size", self.cursor_pixels().to_string()),
        ]
    }

    /// XSETTINGS, which GTK programs follow while they run. GTK scales its
    /// windows by `Gdk/WindowScalingFactor` and sizes text by
    /// `Gdk/UnscaledDPI`; `Xft/DPI` is the full DPI for everybody else.
    /// DPIs are in 1024ths.
    fn xsettings(&self) -> Vec<(String, Value)> {
        vec![
            (
                "Xft/DPI".into(),
                Value::Integer(X11_DPI * 1024 * self.scale),
            ),
            ("Gdk/WindowScalingFactor".into(), Value::Integer(self.scale)),
            ("Gdk/UnscaledDPI".into(), Value::Integer(X11_DPI * 1024)),
            (
                "Gtk/CursorThemeName".into(),
                Value::String(self.cursor_theme.clone()),
            ),
            (
                "Gtk/CursorThemeSize".into(),
                Value::Integer(self.cursor_pixels()),
            ),
        ]
    }
}

/// `RESOURCE_MANAGER` text with `ours` set, keeping everything else.
fn merge_resources(existing: &str, ours: &[(&str, String)]) -> String {
    let mut merged = String::new();
    for line in existing.lines() {
        let name = line.split(':').next().unwrap_or_default().trim();
        if name.is_empty() || ours.iter().any(|(ours, _)| *ours == name) {
            continue;
        }
        merged.push_str(line);
        merged.push('\n');
    }
    for (name, value) in ours {
        merged.push_str(&format!("{name}:\t{value}\n"));
    }
    merged
}

/// Sets X resources on the root window, like `xrdb -merge`. On a thread
/// of its own: Xwayland may be waiting for the compositor while it is
/// asked.
pub struct ResourceWriter {
    sender: mpsc::Sender<Vec<(&'static str, String)>>,
}

impl ResourceWriter {
    fn start(display_number: u32) -> Option<Self> {
        use smithay::reexports::x11rb::connection::Connection;
        use smithay::reexports::x11rb::rust_connection::RustConnection;

        let (sender, receiver) = mpsc::channel::<Vec<(&'static str, String)>>();
        let spawned = std::thread::Builder::new()
            .name("xwayland-resources".into())
            .spawn(move || {
                let connected = RustConnection::connect(Some(&format!(":{display_number}")));
                let (conn, screen) = match connected {
                    Ok(connected) => connected,
                    Err(err) => {
                        eprintln!("canoe: failed to set X resources: {err}");
                        return;
                    }
                };
                let root = conn.setup().roots[screen].root;
                // Until the compositor lets go, the latest resources each time.
                while let Ok(mut resources) = receiver.recv() {
                    while let Ok(newer) = receiver.try_recv() {
                        resources = newer;
                    }
                    if let Err(err) = write_resources(&conn, root, &resources) {
                        eprintln!("canoe: failed to set X resources: {err}");
                    }
                }
            });
        match spawned {
            Ok(_) => Some(Self { sender }),
            Err(err) => {
                eprintln!("canoe: failed to set X resources: {err}");
                None
            }
        }
    }

    fn write(&self, resources: Vec<(&'static str, String)>) {
        let _ = self.sender.send(resources);
    }
}

fn write_resources(
    conn: &smithay::reexports::x11rb::rust_connection::RustConnection,
    root: u32,
    resources: &[(&str, String)],
) -> Result<(), Box<dyn std::error::Error>> {
    use smithay::reexports::x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
    use smithay::reexports::x11rb::wrapper::ConnectionExt as _;

    let existing = conn
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            u32::MAX / 4,
        )?
        .reply()?;
    let merged = merge_resources(&String::from_utf8_lossy(&existing.value), resources);
    conn.change_property8(
        PropMode::REPLACE,
        root,
        AtomEnum::RESOURCE_MANAGER,
        AtomEnum::STRING,
        merged.as_bytes(),
    )?
    .check()?;
    Ok(())
}

impl State {
    /// Start Xwayland. Programs canoe launches reach it through `DISPLAY`,
    /// which is set right away: X11 clients connecting before the server is
    /// ready wait for it.
    pub fn start_xwayland(&mut self) {
        let spawned = XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            Stdio::null(),
            Stdio::null(),
            |_| (),
        );
        let (xwayland, client) = match spawned {
            Ok(spawned) => spawned,
            Err(err) => {
                eprintln!("canoe: failed to start Xwayland, X11 programs won't run: {err}");
                return;
            }
        };
        let display = xwayland.display_number();
        let client_for_scale = client.clone();
        let handle = self.loop_handle.clone();
        let inserted =
            self.loop_handle
                .insert_source(xwayland, move |event, _, state| match event {
                    XWaylandEvent::Ready {
                        x11_socket,
                        display_number,
                    } => {
                        let_root_connect(display_number);
                        state.x11_resources = ResourceWriter::start(display_number);
                        match X11Wm::start_wm(handle.clone(), x11_socket, client.clone()) {
                            Ok(xwm) => state.xwayland_ready(xwm),
                            Err(err) => eprintln!("canoe: failed to manage X11 windows: {err}"),
                        }
                    }
                    XWaylandEvent::Error => eprintln!("canoe: Xwayland exited during startup"),
                });
        if let Err(err) = inserted {
            eprintln!("canoe: failed to watch Xwayland: {err}");
            return;
        }
        // Before Xwayland learns about the outputs.
        self.xwayland_client = Some(client_for_scale);
        self.update_x11_settings();
        std::env::set_var("DISPLAY", format!(":{display}"));
        eprintln!("canoe: X11 programs use DISPLAY=:{display}");
    }

    fn xwayland_ready(&mut self, xwm: X11Wm) {
        self.xwm = Some(xwm);
        self.x11_settings = None;
        self.update_x11_settings();
    }

    /// The scale X11 programs should draw at: that of the output with the
    /// largest, rounded up, so they are shrunk where they are shown, never
    /// stretched. `None` without outputs.
    fn wanted_x11_scale(&self) -> Option<i32> {
        if !self.context.borrow().config.xwayland_hidpi {
            return Some(1);
        }
        self.outputs
            .iter()
            .filter(|o| o.enabled)
            .map(|o| o.output.current_scale().fractional_scale().ceil() as i32)
            .max()
            .map(|scale| scale.max(1))
    }

    /// Let X11 programs draw at the outputs' scale, and tell them and the
    /// X server that and which cursor to show. For whenever outputs, their
    /// scales, the cursor theme or the configuration change.
    pub fn update_x11_settings(&mut self) {
        let Some(client) = self.xwayland_client.clone() else {
            return;
        };
        let Some(data) = client.get_data::<XWaylandClientData>() else {
            return;
        };
        let current = data.compositor_state.client_scale();
        let scale = self.wanted_x11_scale().unwrap_or(current.round() as i32);
        let rescaled = scale as f64 != current;
        if rescaled {
            data.compositor_state.set_client_scale(scale as f64);
            // Xwayland hears of the outputs' positions and sizes in X
            // pixels of the new scale.
            for output in self.outputs.iter().filter(|o| o.enabled) {
                output.output.change_current_state(None, None, None, None);
            }
            eprintln!("canoe: X11 programs draw at scale {scale}");
        }
        let Some(xwm) = self.xwm.as_mut() else {
            return;
        };
        let settings = X11Settings {
            scale,
            cursor_theme: self.cursor.name().to_string(),
            cursor_size: self.cursor.size(),
        };
        if self.x11_settings.as_ref() != Some(&settings) {
            // X11 programs show the cursor theme's arrow where they don't
            // pick a cursor of their own.
            if let Some((pixels, size, hotspot)) = self.cursor.default_image(scale as u32) {
                if let Err(err) = xwm.set_cursor(&pixels, Size::from(size), Point::from(hotspot)) {
                    eprintln!("canoe: failed to set the X11 cursor: {err}");
                }
            }
            if let Err(err) = xwm.set_xsettings(settings.xsettings().into_iter()) {
                eprintln!("canoe: failed to set XSETTINGS: {err}");
            }
            if let Some(writer) = &self.x11_resources {
                writer.write(settings.resources());
            }
            self.x11_settings = Some(settings);
        }
        if rescaled {
            // X11 windows keep their place and size, in X pixels of the new
            // scale. Only after XSETTINGS: GTK resizes its windows to its
            // new scale itself, and takes this resize for the user's if it
            // comes first, growing them twice.
            let windows = self.windows.iter().filter_map(|w| w.x11());
            for x11 in windows.chain(&self.x11_pending) {
                let _ = x11.configure(x11.geometry());
            }
        }
    }

    /// The managed window showing `x11`.
    fn window_for_x11(&self, x11: &X11Surface) -> Option<&ManagedWindow> {
        self.windows
            .iter()
            .find(|w| w.x11().is_some_and(|w| same_window(w, x11)))
    }

    /// Manage a mapped X11 window once Xwayland gave it a surface.
    fn manage_x11_window(&mut self, x11: X11Surface) {
        let Some(index) = self.x11_pending.iter().position(|w| same_window(w, &x11)) else {
            return;
        };
        let Some(surface) = x11.wl_surface() else {
            return;
        };
        self.x11_pending.remove(index);
        if self.window_for_surface(&surface).is_some() {
            return;
        }
        let key = self.add_window(
            Shell::X11(x11.clone()),
            Window::new_x11_window(x11.clone()),
            surface.clone(),
        );
        self.x11_parent_changed(&x11);
        self.report_size_hints(key);
        // The surface may already show something.
        self.toplevel_committed(&surface);
        self.run_wm();
    }

    /// Forget an X11 window that was unmapped or destroyed.
    fn forget_x11_window(&mut self, x11: &X11Surface) {
        self.x11_pending.retain(|w| !same_window(w, x11));
        self.x11_unmanaged.retain(|w| !same_window(w, x11));
        if let Some(surface) = self.window_for_x11(x11).map(|w| w.wl_surface().clone()) {
            self.remove_toplevel(&surface);
        }
        self.queue_redraw_all();
    }

    fn x11_parent_changed(&mut self, x11: &X11Surface) {
        let Some(id) = self.window_for_x11(x11).map(|w| w.id) else {
            return;
        };
        let parent_id = x11.is_transient_for().and_then(|parent| {
            self.windows
                .iter()
                .find(|w| w.x11().is_some_and(|w| w.window_id() == parent))
                .map(|w| w.id)
        });
        self.set_window_parent(id, parent_id);
    }

    fn x11_decoration_changed(&mut self, x11: &X11Surface) {
        let Some(id) = self.window_for_x11(x11).map(|w| w.id) else {
            return;
        };
        {
            let context = self.context.borrow();
            if let Some(window) = context.windows.get(&id) {
                let mut w = window.borrow_mut();
                w.decoration_hint = Some(decoration_hint(x11));
                context.apply_rules_to_window(&mut w);
            }
        }
        self.schedule_wm();
    }

    fn x11_window_request(&mut self, x11: &X11Surface, event: canoe::WindowEvent) {
        if let Some(surface) = self.window_for_x11(x11).map(|w| w.wl_surface().clone()) {
            self.window_request(&surface, event);
        }
    }

    /// Give X11 windows the position and size the window manager wants for
    /// them. X11 windows are placed in the same coordinates as everything
    /// else, so their menus open in the right place.
    pub(super) fn configure_x11_windows(&mut self) {
        let context = self.context.borrow();
        for window in &self.windows {
            let Some(x11) = window.x11() else {
                continue;
            };
            let current = x11.geometry();
            let placed = context
                .windows
                .get(&window.id)
                .is_some_and(|w| !w.borrow().position_undefined);
            let loc = match window.fullscreen {
                Some(output) => self
                    .output_by_key(output)
                    .and_then(|o| self.output_geometry(&o.output))
                    .map_or(window.pos, |geo| geo.loc),
                None if placed => window.pos,
                None => current.loc,
            };
            let size = window.x11_size.unwrap_or(current.size);
            let wanted = Rectangle::new(loc, size);
            if wanted != current {
                let _ = x11.configure(wanted);
            }
        }
    }

    /// Stack X11 windows in the X server the way they are shown, hidden
    /// ones at the bottom, so it sends input to the window that's visible.
    pub(super) fn restack_x11_windows(&mut self) {
        let Some(xwm) = self.xwm.as_mut() else {
            return;
        };
        let hidden = self.windows.iter().filter(|w| w.hidden);
        let shown = self.windows.iter().filter(|w| !w.hidden);
        let order: Vec<X11Surface> = hidden
            .chain(shown)
            .filter_map(|w| w.x11().cloned())
            .collect();
        let ids: Vec<_> = order.iter().map(|w| w.window_id()).collect();
        if ids.is_empty() || ids == self.x11_stacking {
            return;
        }
        self.x11_stacking = ids;
        if let Err(err) = xwm.update_stacking_order_upwards(order.iter()) {
            eprintln!("canoe: failed to restack X11 windows: {err}");
        }
    }

    /// The unmanaged X11 windows shown, bottom first, with their surface
    /// and position.
    pub fn x11_unmanaged_surfaces(&self) -> Vec<(WlSurface, Point<i32, Logical>)> {
        self.x11_unmanaged
            .iter()
            .filter_map(|w| Some((w.wl_surface()?, w.geometry().loc)))
            .collect()
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _surface: WlSurface, window: X11Surface) {
        // Associations can happen in a commit hook; manage the window once
        // that commit is done.
        if self.x11_pending.iter().any(|w| same_window(w, &window)) {
            self.loop_handle
                .insert_idle(move |state| state.manage_x11_window(window));
        }
        self.queue_redraw_all();
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("X11 event without a window manager")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Err(err) = window.set_mapped(true) {
            eprintln!("canoe: failed to map an X11 window: {err}");
            return;
        }
        if is_unmanaged_type(&window) {
            if !self.x11_unmanaged.iter().any(|w| same_window(w, &window)) {
                self.x11_unmanaged.push(window);
            }
            self.queue_redraw_all();
            return;
        }
        if self.window_for_x11(&window).is_some()
            || self.x11_pending.iter().any(|w| same_window(w, &window))
        {
            return;
        }
        self.x11_pending.push(window.clone());
        if window.wl_surface().is_some() {
            self.manage_x11_window(window);
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if !self.x11_unmanaged.iter().any(|w| same_window(w, &window)) {
            self.x11_unmanaged.push(window);
        }
        self.queue_redraw_all();
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11_window(&window);
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.forget_x11_window(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut geometry = window.geometry();
        if let Some(managed) = self.window_for_x11(&window) {
            // The window manager places managed windows. They may resize
            // themselves, unless it gave them a size of their output.
            let (key, id) = (managed.key, managed.id);
            let constrained = managed.fullscreen.is_some()
                || self.context.borrow().windows.get(&id).is_some_and(|w| {
                    let w = w.borrow();
                    w.maximized || w.snap_state.is_some()
                });
            if !constrained && (w.is_some() || h.is_some()) {
                let size = Size::from((
                    w.map_or(geometry.size.w, |w| w as i32),
                    h.map_or(geometry.size.h, |h| h as i32),
                ));
                if let Some(managed) = self.window_by_key_mut(key) {
                    managed.x11_size = Some(size);
                }
                self.configure_x11_windows();
            }
            // The client hears back either way.
            let _ = window.configure(None);
            return;
        }
        if let Some(x) = x {
            geometry.loc.x = x;
        }
        if let Some(y) = y {
            geometry.loc.y = y;
        }
        if let Some(w) = w {
            geometry.size.w = w as i32;
        }
        if let Some(h) = h {
            geometry.size.h = h as i32;
        }
        let _ = window.configure(geometry);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // Unmanaged windows move themselves.
        if self.x11_unmanaged.iter().any(|w| same_window(w, &window)) {
            self.queue_redraw_all();
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        match property {
            WmWindowProperty::Title | WmWindowProperty::Class => {
                if let Some(surface) = self.window_for_x11(&window).map(|w| w.wl_surface().clone())
                {
                    self.toplevel_properties_changed(&surface);
                }
            }
            WmWindowProperty::NormalHints => {
                if let Some(key) = self.window_for_x11(&window).map(|w| w.key) {
                    self.report_size_hints(key);
                }
            }
            WmWindowProperty::TransientFor => self.x11_parent_changed(&window),
            WmWindowProperty::MotifHints => self.x11_decoration_changed(&window),
            _ => {}
        }
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_window_request(&window, canoe::WindowEvent::Maximize);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_window_request(&window, canoe::WindowEvent::Unmaximize);
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_window_request(&window, canoe::WindowEvent::Fullscreen(None));
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_window_request(&window, canoe::WindowEvent::Unfullscreen);
    }

    fn minimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_window_request(&window, canoe::WindowEvent::Minimize);
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32, edge: ResizeEdge) {
        // As xdg-shell numbers the edges.
        let edges = match edge {
            ResizeEdge::Top => 1,
            ResizeEdge::Bottom => 2,
            ResizeEdge::Left => 4,
            ResizeEdge::TopLeft => 5,
            ResizeEdge::BottomLeft => 6,
            ResizeEdge::Right => 8,
            ResizeEdge::TopRight => 9,
            ResizeEdge::BottomRight => 10,
        };
        let seat = self.wm_seat_weak();
        self.x11_window_request(&window, canoe::WindowEvent::Resize(seat, edges));
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        let seat = self.wm_seat_weak();
        self.x11_window_request(&window, canoe::WindowEvent::Move(seat));
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        // Only the X11 program the keyboard is on may read the selection.
        matches!(
            self.seat.get_keyboard().and_then(|k| k.current_focus()),
            Some(KeyboardFocus::X11(..))
        )
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        // An X11 program reads what a Wayland client selected.
        let result = match selection {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime_type, fd)
                    .map_err(|e| e.to_string())
            }
            SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime_type, fd)
                .map_err(|e| e.to_string()),
        };
        if let Err(err) = result {
            eprintln!("canoe: failed to pass the selection to an X11 program: {err}");
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        // An X11 program selected something: offer it to Wayland clients.
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ())
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ())
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        // Only clear what an X11 program had selected (the compositor offers
        // its selections).
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        eprintln!("canoe: Xwayland is gone, X11 programs won't run");
        self.xwm = None;
        self.xwayland_client = None;
        self.x11_settings = None;
        self.x11_resources = None;
        self.x11_pending.clear();
        self.x11_unmanaged.clear();
        let surfaces: Vec<WlSurface> = self
            .windows
            .iter()
            .filter(|w| w.x11().is_some())
            .map(|w| w.wl_surface().clone())
            .collect();
        for surface in surfaces {
            self.remove_toplevel(&surface);
        }
        self.queue_redraw_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_replace_ours_and_keep_the_rest() {
        let existing = "Xft.dpi:\t96\n*background:\tblack\n\nXcursor.size: 24\n";
        let settings = X11Settings {
            scale: 2,
            cursor_theme: "Adwaita".into(),
            cursor_size: 24,
        };
        assert_eq!(
            merge_resources(existing, &settings.resources()),
            "*background:\tblack\n\
             Xft.dpi:\t192\n\
             Xcursor.theme:\tAdwaita\n\
             Xcursor.size:\t48\n"
        );
    }
}
