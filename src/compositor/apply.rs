//! Carry out what the window manager asked for (see `crate::wm::Request`).

use std::collections::HashMap;
use std::time::Instant;

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::{
    State as XdgState, WmCapabilities,
};
use smithay::utils::{Logical, Size};

use super::{PendingPosition, PointerOp, Shell, State};
use crate::wm::{Capabilities, Edges, Request, WindowKey};

impl State {
    /// Apply every queued request, in order.
    pub fn apply_requests(&mut self) {
        let requests = self.requests.drain();
        // Windows asked to take a new size in this batch, and the last size
        // asked for: a position set along with it waits for the client
        // (see set_window_position).
        let resized: HashMap<WindowKey, Size<i32, Logical>> = requests
            .iter()
            .filter_map(|request| match request {
                Request::ProposeDimensions(key, w, h) if *w > 0 && *h > 0 => {
                    Some((*key, Size::from((*w, *h))))
                }
                _ => None,
            })
            .collect();
        for request in requests {
            self.apply_request(request, &resized);
        }
    }

    /// Apply positions that waited too long for their client.
    pub fn apply_expired_positions(&mut self) {
        let waiting: Vec<WindowKey> = self
            .windows
            .iter()
            .filter(|window| !window.pending_positions.is_empty())
            .map(|window| window.key)
            .collect();
        for key in waiting {
            self.settle_position(key);
        }
    }

    /// Move a window to where it waits to go, if its client is ready (see
    /// [`super::ManagedWindow::settle_position`]), and tell the window
    /// manager if that isn't quite where it put the window.
    pub fn settle_position(&mut self, key: WindowKey) {
        let Some(window) = self.window_by_key_mut(key) else {
            return;
        };
        if !window.settle_position() {
            return;
        }
        let (id, pos) = (window.id, window.pos);
        if let Some(w) = self.context.borrow().windows.get(&id) {
            w.borrow_mut().update_position(pos.x, pos.y);
        }
        self.schedule_wm();
    }

    fn apply_request(
        &mut self,
        request: Request,
        resized: &HashMap<WindowKey, Size<i32, Logical>>,
    ) {
        match request {
            Request::ManageDirty => self.wm_dirty = true,

            Request::ProposeDimensions(key, width, height) => {
                self.with_toplevel(key, |window| {
                    let size = (width > 0 && height > 0).then(|| Size::from((width, height)));
                    match window.toplevel() {
                        Some(toplevel) => toplevel.with_pending_state(|state| state.size = size),
                        // No size leaves an X11 window the size it has.
                        None => window.x11_size = size,
                    }
                });
            }
            Request::SetDimensionBounds(key, width, height) => {
                self.with_toplevel(key, |window| {
                    let bounds = (width > 0 && height > 0).then(|| Size::from((width, height)));
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|state| state.bounds = bounds);
                    }
                });
            }
            Request::Hide(key) => {
                if let Some(window) = self.window_by_key_mut(key) {
                    window.hidden = true;
                }
            }
            Request::Show(key) => {
                if let Some(window) = self.window_by_key_mut(key) {
                    window.hidden = false;
                }
            }
            Request::Close(key) => {
                if let Some(window) = self.window_by_key(key) {
                    match &window.shell {
                        Shell::Xdg(toplevel) => toplevel.send_close(),
                        Shell::X11(x11) => {
                            let _ = x11.close();
                        }
                    }
                }
            }
            Request::SetTiled(key, edges) => {
                self.with_xdg_state(key, |state| {
                    for (edge, xdg) in [
                        (Edges::Top, XdgState::TiledTop),
                        (Edges::Bottom, XdgState::TiledBottom),
                        (Edges::Left, XdgState::TiledLeft),
                        (Edges::Right, XdgState::TiledRight),
                    ] {
                        if edges.contains(edge) {
                            state.states.set(xdg);
                        } else {
                            state.states.unset(xdg);
                        }
                    }
                });
            }
            Request::Fullscreen(key, output_key) => {
                let size = self
                    .output_by_key(output_key)
                    .and_then(|o| self.output_geometry(&o.output))
                    .map(|geo| geo.size);
                self.with_toplevel(key, |window| {
                    window.fullscreen = Some(output_key);
                    if let Some(size) = size {
                        match window.toplevel() {
                            Some(toplevel) => {
                                toplevel.with_pending_state(|state| state.size = Some(size))
                            }
                            None => window.x11_size = Some(size),
                        }
                    }
                });
            }
            Request::ExitFullscreen(key) => {
                if let Some(window) = self.window_by_key_mut(key) {
                    window.fullscreen = None;
                }
            }
            Request::InformFullscreen(key, on) => {
                if let Some(x11) = self.window_by_key(key).and_then(|w| w.x11()) {
                    let _ = x11.set_fullscreen(on);
                }
                self.set_xdg_state(key, XdgState::Fullscreen, on);
            }
            Request::InformMaximized(key, on) => {
                if let Some(window) = self.window_by_key_mut(key) {
                    window.maximized = on;
                }
                if let Some(x11) = self.window_by_key(key).and_then(|w| w.x11()) {
                    let _ = x11.set_maximized(on);
                }
                self.set_xdg_state(key, XdgState::Maximized, on);
            }
            Request::InformResizing(key, on) => {
                self.set_xdg_state(key, XdgState::Resizing, on);
            }
            Request::UseSsd(key, ssd) => {
                self.with_toplevel(key, |window| {
                    window.ssd = ssd;
                    window.decoration_mode_dirty = true;
                });
            }
            Request::SetCapabilities(key, caps) => {
                self.with_xdg_state(key, |state| {
                    for (cap, xdg) in [
                        (Capabilities::WindowMenu, WmCapabilities::WindowMenu),
                        (Capabilities::Maximize, WmCapabilities::Maximize),
                        (Capabilities::Fullscreen, WmCapabilities::Fullscreen),
                        (Capabilities::Minimize, WmCapabilities::Minimize),
                    ] {
                        if caps.contains(cap) {
                            state.capabilities.set(xdg);
                        } else {
                            state.capabilities.unset(xdg);
                        }
                    }
                });
            }
            Request::SetClipBox(key, clip) => {
                if let Some(window) = self.window_by_key_mut(key) {
                    window.clip = clip;
                }
            }
            Request::SetContentClipBox(key, clip) => {
                if let Some(window) = self.window_by_key_mut(key) {
                    window.content_clip = clip;
                }
            }

            Request::AnimateGeometry(key, duration) => self.start_warp(key, duration),

            Request::SetPosition(key, x, y) => {
                self.set_window_position(key, x, y, resized.get(&key).copied())
            }
            Request::PlaceTop(key) => {
                if let Some(index) = self.windows.iter().position(|w| w.key == key) {
                    let window = self.windows.remove(index);
                    self.windows.push(window);
                }
            }
            Request::PlaceBottom(key) => {
                if let Some(index) = self.windows.iter().position(|w| w.key == key) {
                    let window = self.windows.remove(index);
                    self.windows.insert(0, window);
                }
            }
            Request::PlaceAbove(key, other) => self.restack(key, other, true),
            Request::PlaceBelow(key, other) => self.restack(key, other, false),

            Request::FocusWindow(_, key) => {
                self.wm_focus = Some(key);
                // The window manager took the keyboard back from an
                // on-demand layer surface.
                if matches!(self.layer_focus, Some((_, false))) {
                    self.layer_focus = None;
                }
            }
            Request::ClearFocus(_) => self.wm_focus = None,
            Request::OpStartPointer(_) => {
                if self.pointer_op.is_none() {
                    self.pointer_op = Some(PointerOp {
                        start: self.pointer_location,
                        released: self.pressed_buttons.is_empty(),
                    });
                    // Nothing has pointer focus during the operation, not
                    // even the window manager's own surfaces.
                    self.canoe_grab = None;
                    self.leave_canoe_target();
                    self.clear_client_pointer_focus();
                }
            }
            Request::OpEnd(_) => {
                if self.pointer_op.take().is_some() {
                    self.refresh_pointer_focus();
                }
            }
            Request::SetXcursorTheme(_, name, size) => {
                self.cursor.set_theme(&name, size);
                self.queue_redraw_all();
            }
            Request::SetCursorShape(_, shape) => {
                self.wm_cursor = shape;
                self.queue_redraw_all();
            }
            Request::SetDefaultLayerOutput(output_key) => {
                self.default_layer_output = Some(output_key);
            }
        }
    }

    fn with_toplevel(&mut self, key: WindowKey, f: impl FnOnce(&mut super::ManagedWindow)) {
        if let Some(window) = self.window_by_key_mut(key) {
            f(window);
            window.needs_configure = true;
        }
    }

    /// Change the pending state of an xdg toplevel (X11 windows have none).
    fn with_xdg_state(
        &mut self,
        key: WindowKey,
        f: impl FnOnce(&mut smithay::wayland::shell::xdg::ToplevelState),
    ) {
        self.with_toplevel(key, |window| {
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(f);
            }
        });
    }

    fn set_xdg_state(&mut self, key: WindowKey, xdg: XdgState, on: bool) {
        self.with_xdg_state(key, |state| {
            if on {
                state.states.set(xdg);
            } else {
                state.states.unset(xdg);
            }
        });
    }

    /// Move a window. When the window manager proposed a new size along
    /// with the position, the move waits until the client draws that size,
    /// so a resize from the left or top edge doesn't make the content jump.
    fn set_window_position(
        &mut self,
        key: WindowKey,
        x: i32,
        y: i32,
        proposed: Option<Size<i32, Logical>>,
    ) {
        let Some(window) = self.window_by_key_mut(key) else {
            return;
        };
        let target = (x, y).into();
        // An X11 window takes position and size at once.
        let proposed = proposed.filter(|_| window.toplevel().is_some() && window.configurable());
        let current = window.window.geometry().size;
        match proposed {
            Some(size) if size != current && window.reported_size.is_some() => {
                // Dragging the left or top edge moves it, and only it.
                let resizing = window.toplevel().is_some_and(|toplevel| {
                    toplevel.with_pending_state(|state| state.states.contains(XdgState::Resizing))
                });
                let last = window.target_position();
                window.pending_positions.push(PendingPosition {
                    pos: target,
                    size,
                    anchor: (resizing && x != last.x, resizing && y != last.y),
                    since: Instant::now(),
                });
            }
            _ => {
                window.pos = target;
                window.pending_positions.clear();
            }
        }
    }

    fn restack(&mut self, key: WindowKey, other: WindowKey, above: bool) {
        if key == other {
            return;
        }
        let Some(index) = self.windows.iter().position(|w| w.key == key) else {
            return;
        };
        let window = self.windows.remove(index);
        match self.windows.iter().position(|w| w.key == other) {
            Some(other_index) => {
                let at = if above { other_index + 1 } else { other_index };
                self.windows.insert(at, window);
            }
            None => self.windows.insert(index.min(self.windows.len()), window),
        }
    }
}
