//! Canoe - River Window Manager in Rust
//!
//! A tiling window manager for the River Wayland compositor.

mod binding;
mod canoe;
mod config;
mod protocol;
mod rule;

use canoe::Context;
use protocol::*;

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use nix::fcntl::OFlag;
use nix::libc;
use nix::poll::{poll, PollFd, PollFlags};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::pipe2;

static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn forward_signal(sig: libc::c_int) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = sig as u8;
    unsafe {
        libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
    }
}

use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::ZwlrLayerShellV1;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

/// Application state for Wayland dispatch
struct AppState {
    context: Rc<RefCell<Context>>,
    globals: Globals,
}

/// Collected Wayland globals
#[derive(Default)]
struct Globals {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    rwm: Option<RiverWindowManagerV1>,
    rwm_xkb_bindings: Option<RiverXkbBindingsV1>,
    rwm_layer_shell: Option<RiverLayerShellV1>,
    rwm_input_manager: Option<RiverInputManagerV1>,
    rwm_libinput_config: Option<RiverLibinputConfigV1>,
    wlr_layer_shell: Option<ZwlrLayerShellV1>,
    cursor_shape_manager: Option<WpCursorShapeManagerV1>,
    wl_seats: HashMap<u32, wl_seat::WlSeat>,
    wl_seat_has_pointer: HashMap<u32, bool>,
    wl_outputs: HashMap<u32, wl_output::WlOutput>,
    wl_output_scales: HashMap<u32, i32>,
}

const CLOSE_DOUBLE_CLICK: Duration = Duration::from_millis(400);

fn attach_wl_seat(
    state: &mut AppState,
    seat_ref: &Rc<RefCell<canoe::Seat>>,
    qh: &QueueHandle<AppState>,
) {
    let wl_seat_name = seat_ref.borrow().wl_seat_name;
    if wl_seat_name == 0 || seat_ref.borrow().wl_seat.is_some() {
        return;
    }
    let wl_seat = match state.globals.wl_seats.get(&wl_seat_name) {
        Some(seat) => seat.clone(),
        None => return,
    };

    {
        let mut seat = seat_ref.borrow_mut();
        seat.wl_seat = Some(wl_seat.clone());
    }

    let has_pointer = state
        .globals
        .wl_seat_has_pointer
        .get(&wl_seat_name)
        .copied()
        .unwrap_or(false);
    if has_pointer {
        if seat_ref.borrow().wl_pointer.is_none() {
            let wl_pointer = wl_seat.get_pointer(qh, seat_ref.borrow().id);
            seat_ref.borrow_mut().wl_pointer = Some(wl_pointer);
        }
        attach_cursor_shape_device(state, seat_ref, qh);
    }
}

fn attach_cursor_shape_device(
    state: &mut AppState,
    seat_ref: &Rc<RefCell<canoe::Seat>>,
    qh: &QueueHandle<AppState>,
) {
    let manager = match state.globals.cursor_shape_manager.as_ref() {
        Some(manager) => manager,
        None => return,
    };
    let wl_pointer = match seat_ref.borrow().wl_pointer.as_ref() {
        Some(pointer) => pointer.clone(),
        None => return,
    };
    if seat_ref.borrow().cursor_shape_device.is_some() {
        return;
    }
    let device: WpCursorShapeDeviceV1 = manager.get_pointer(&wl_pointer, qh, ());
    seat_ref.borrow_mut().cursor_shape_device = Some(device);
}

fn render_window_menu(state: &mut AppState, qh: &QueueHandle<AppState>) {
    let (Some(shm), Some(compositor)) = (
        state.globals.shm.as_ref(),
        state.globals.compositor.as_ref(),
    ) else {
        return;
    };

    let mut context = state.context.borrow_mut();
    let scale = context
        .window_menu
        .as_ref()
        .and_then(|menu| {
            context
                .outputs
                .get(&menu.output_id)
                .map(|output| output.borrow().scale)
        })
        .unwrap_or(1);
    if let Some(menu) = context.window_menu.as_mut() {
        if !menu.configured {
            return;
        }
        menu.ensure_buffer(shm, qh, scale);
        menu.render();
        menu.update_input_region(compositor, qh);
        menu.commit();
    }
}

#[allow(clippy::too_many_arguments)]
fn open_window_menu_with_items(
    state: &mut AppState,
    output_id: canoe::OutputId,
    pointer: (i32, i32),
    centered: bool,
    mode: canoe::WindowMenuMode,
    items: Vec<canoe::MenuItem>,
    header_title: Option<String>,
    qh: &QueueHandle<AppState>,
) {
    let (Some(compositor), Some(layer_shell)) = (
        state.globals.compositor.as_ref(),
        state.globals.wlr_layer_shell.as_ref(),
    ) else {
        return;
    };

    let (output_info, focused_window, menu_theme) = {
        let context = state.context.borrow();
        let focused = context.focused_window;
        let menu_theme = canoe::MenuTheme::from_ui(&context.config.ui);
        let info = context.outputs.get(&output_id).map(|output| {
            let out = output.borrow();
            (out.width, out.height, out.wl_output.clone())
        });
        (info, focused, menu_theme)
    };

    let Some((ow, oh, wl_output)) = output_info else {
        return;
    };

    if items.is_empty() {
        return;
    }

    let surface = compositor.create_surface(qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        wl_output.as_ref(),
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer::Overlay,
        "canoe-window-menu".to_string(),
        qh,
        canoe::LayerSurfaceKind::Menu,
    );

    let mut menu = canoe::WindowMenu::new(
        surface,
        layer_surface,
        output_id,
        items,
        header_title,
        pointer.0,
        pointer.1,
        menu_theme,
    );
    let mut initial_preview = None;
    if mode == canoe::WindowMenuMode::AltTab {
        menu.select_window(focused_window);
        menu.select_next();
        initial_preview = menu
            .hovered
            .and_then(|idx| menu.items.get(idx).map(|item| item.window_id));
    }
    let mut local_x = pointer.0.max(0);
    let mut local_y = pointer.1.max(0);
    if ow > 0 && oh > 0 {
        if centered {
            local_x = ((ow - menu.width) / 2).max(0);
            local_y = ((oh - menu.height) / 2).max(0);
        } else {
            if local_x + menu.width > ow {
                local_x = (ow - menu.width).max(0);
            }
            if local_y + menu.height > oh {
                local_y = (oh - menu.height).max(0);
            }
        }
    }
    menu.origin_x = local_x;
    menu.origin_y = local_y;

    menu.layer_surface.set_anchor(
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Top
            | wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Left,
    );
    menu.layer_surface.set_margin(local_y, 0, 0, local_x);
    menu.layer_surface
        .set_size(menu.width as u32, menu.height as u32);
    menu.layer_surface.set_keyboard_interactivity(
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::KeyboardInteractivity::None,
    );
    menu.layer_surface.set_exclusive_zone(-1);
    menu.surface.commit();

    let mut context = state.context.borrow_mut();
    context.window_menu = Some(menu);
    context.window_menu_mode = Some(mode);
    if mode == canoe::WindowMenuMode::AltTab {
        context.begin_alt_tab();
        if let Some(window_id) = initial_preview {
            context.preview_alt_tab_window(window_id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn open_window_menu(
    state: &mut AppState,
    output_id: canoe::OutputId,
    pointer_x: i32,
    pointer_y: i32,
    centered: bool,
    mode: canoe::WindowMenuMode,
    header_title: Option<String>,
    qh: &QueueHandle<AppState>,
) {
    let items = {
        let context = state.context.borrow();
        context.collect_menu_items(output_id)
    };
    open_window_menu_with_items(
        state,
        output_id,
        (pointer_x, pointer_y),
        centered,
        mode,
        items,
        header_title,
        qh,
    );
}

fn ensure_window_menu_shield(
    state: &mut AppState,
    output_id: canoe::OutputId,
    qh: &QueueHandle<AppState>,
) {
    let (Some(compositor), Some(layer_shell)) = (
        state.globals.compositor.as_ref(),
        state.globals.wlr_layer_shell.as_ref(),
    ) else {
        return;
    };

    let output = {
        let context = state.context.borrow();
        context.outputs.get(&output_id).cloned()
    };
    let Some(output) = output else {
        return;
    };

    {
        let mut context = state.context.borrow_mut();
        if let Some(shield) = context.window_menu_shield.as_ref() {
            if shield.output_id == output_id {
                return;
            }
        }
        context.window_menu_shield = None;
    }

    let wl_output = output.borrow().wl_output.clone();
    if wl_output.is_none() {
        return;
    }
    let surface = compositor.create_surface(qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        wl_output.as_ref(),
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer::Overlay,
        "canoe-window-menu-shield".to_string(),
        qh,
        canoe::LayerSurfaceKind::MenuShield(output_id),
    );

    layer_surface.set_anchor(
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Top
            | wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Bottom
            | wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Left
            | wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Right,
    );
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::KeyboardInteractivity::None,
    );
    layer_surface.set_size(0, 0);
    surface.commit();

    state.context.borrow_mut().window_menu_shield =
        Some(canoe::ShieldSurface::new(surface, layer_surface, output_id));
}

fn request_manage_dirty(state: &AppState) {
    if let Some(ref rwm) = state.context.borrow().rwm {
        rwm.manage_dirty();
    }
}

fn update_menu_hover_from_global(
    state: &mut AppState,
    seat_id: canoe::SeatId,
    qh: &QueueHandle<AppState>,
) {
    let (px, py) = {
        let context = state.context.borrow();
        let Some(seat) = context.seats.get(&seat_id) else {
            return;
        };
        let seat_ref = seat.borrow();
        (seat_ref.pointer_x, seat_ref.pointer_y)
    };

    let changed = {
        let mut context = state.context.borrow_mut();
        if context.window_menu_mode != Some(canoe::WindowMenuMode::Pointer) {
            return;
        }
        let (output_id, origin_x, origin_y) = {
            let Some(menu) = context.window_menu.as_ref() else {
                return;
            };
            (menu.output_id, menu.origin_x, menu.origin_y)
        };
        let (local_x, local_y) = {
            let Some(output) = context.outputs.get(&output_id) else {
                return;
            };
            let out = output.borrow();
            let local_px = px - out.x;
            let local_py = py - out.y;
            (local_px - origin_x, local_py - origin_y)
        };
        let Some(menu) = context.window_menu.as_mut() else {
            return;
        };
        menu.update_hover(local_x, local_y)
    };

    if changed {
        render_window_menu(state, qh);
    }
}

fn update_menu_hover_from_surface(
    state: &mut AppState,
    output_id: canoe::OutputId,
    surface_x: f64,
    surface_y: f64,
    qh: &QueueHandle<AppState>,
) {
    let changed = {
        let mut context = state.context.borrow_mut();
        if context.window_menu_mode != Some(canoe::WindowMenuMode::Pointer) {
            return;
        }
        let Some(menu) = context.window_menu.as_mut() else {
            return;
        };
        if menu.output_id != output_id {
            return;
        }
        let local_x = surface_x.round() as i32 - menu.origin_x;
        let local_y = surface_y.round() as i32 - menu.origin_y;
        menu.update_hover(local_x, local_y)
    };

    if changed {
        render_window_menu(state, qh);
    }
}

fn update_pointer_position_from_output(
    state: &AppState,
    seat: &Rc<RefCell<canoe::Seat>>,
    output_id: canoe::OutputId,
    surface_x: f64,
    surface_y: f64,
) {
    let (ox, oy) = {
        let context = state.context.borrow();
        let Some(output) = context.outputs.get(&output_id) else {
            return;
        };
        let out = output.borrow();
        (out.x, out.y)
    };
    let global_x = ox + surface_x.round() as i32;
    let global_y = oy + surface_y.round() as i32;
    seat.borrow_mut()
        .update_pointer_position(global_x, global_y);
}

fn update_titlebar_hover_from_surface(
    state: &mut AppState,
    window_id: canoe::WindowId,
    surface_x: f64,
    surface_y: f64,
) -> bool {
    let local_x = surface_x.round() as i32;
    let local_y = surface_y.round() as i32;
    let (border_width, titlebar_height) = {
        let ui = &state.context.borrow().config.ui;
        (ui.border_width, canoe::titlebar::titlebar_height(ui))
    };
    let context = state.context.borrow();
    let Some(window) = context.windows.get(&window_id) else {
        return false;
    };
    let mut w = window.borrow_mut();
    let new_hover =
        canoe::titlebar::button_at(w.width, border_width, local_x, local_y, titlebar_height);
    if w.titlebar_hovered == new_hover {
        return false;
    }
    w.titlebar_hovered = new_hover;
    true
}

fn update_titlebar_hover_from_global(
    state: &mut AppState,
    window_id: canoe::WindowId,
    pointer_x: i32,
    pointer_y: i32,
) -> bool {
    let (win_x, win_y, win_w, swallow_top) = {
        let context = state.context.borrow();
        let Some(window) = context.windows.get(&window_id) else {
            return false;
        };
        let w = window.borrow();
        (w.x, w.y, w.width, w.swallow_top)
    };

    let (border_width, titlebar_height) = {
        let ui = &state.context.borrow().config.ui;
        (ui.border_width, canoe::titlebar::titlebar_height(ui))
    };
    let origin_x = win_x - border_width;
    let origin_y = win_y - border_width - titlebar_height + swallow_top;
    let local_x = pointer_x - origin_x;
    let local_y = pointer_y - origin_y;

    let context = state.context.borrow();
    let Some(window) = context.windows.get(&window_id) else {
        return false;
    };
    let mut w = window.borrow_mut();
    let new_hover =
        canoe::titlebar::button_at(win_w, border_width, local_x, local_y, titlebar_height);
    if w.titlebar_hovered == new_hover {
        return false;
    }
    w.titlebar_hovered = new_hover;
    true
}

fn clear_titlebar_state(state: &mut AppState, window_id: canoe::WindowId) -> bool {
    let context = state.context.borrow();
    let Some(window) = context.windows.get(&window_id) else {
        return false;
    };
    let mut w = window.borrow_mut();
    let changed =
        w.titlebar_hovered.is_some() || w.titlebar_pressed.is_some() || w.titlebar_left_down;
    w.titlebar_hovered = None;
    w.titlebar_pressed = None;
    w.titlebar_left_down = false;
    changed
}

fn menu_matches_app_id(context: &canoe::Context, menu: &canoe::WindowMenu, app_id: &str) -> bool {
    if menu.items.is_empty() {
        return false;
    }
    menu.items.iter().all(|item| {
        let Some(window) = context.windows.get(&item.window_id) else {
            return false;
        };
        let w = window.borrow();
        w.app_id.as_deref() == Some(app_id)
    })
}

fn handle_window_menu_cycle(state: &mut AppState, qh: &QueueHandle<AppState>) {
    let mut should_render = false;
    let mut open_new = false;
    let mut ensure_shield = None;
    let mut preview_window = None;

    {
        let mut context = state.context.borrow_mut();
        let is_alt_tab = context.window_menu_mode == Some(canoe::WindowMenuMode::AltTab);
        if let Some(menu) = context.window_menu.as_mut() {
            if is_alt_tab {
                if menu.select_next() {
                    should_render = true;
                    preview_window = menu
                        .hovered
                        .and_then(|idx| menu.items.get(idx).map(|item| item.window_id));
                }
                ensure_shield = Some(menu.output_id);
            } else {
                context.close_window_menu();
                open_new = true;
            }
        } else {
            open_new = true;
        }

        if let Some(window_id) = preview_window {
            context.preview_alt_tab_window(window_id);
        }
    }

    if should_render {
        render_window_menu(state, qh);
    }

    if let Some(output_id) = ensure_shield {
        ensure_window_menu_shield(state, output_id, qh);
    }

    if !open_new {
        return;
    }

    let output_id = {
        let context = state.context.borrow();
        let focused_output = context.focused_window.and_then(|window_id| {
            context.windows.get(&window_id).and_then(|window| {
                let window_ref = window.borrow();
                let center_x = window_ref.x + (window_ref.width / 2);
                let center_y = window_ref.y + (window_ref.height / 2);
                if let Some(output) = window_ref.output.as_ref().and_then(|o| o.upgrade()) {
                    let out_ref = output.borrow();
                    if out_ref.contains_point(center_x, center_y) {
                        return Some(out_ref.id);
                    }
                }
                context.outputs.iter().find_map(|(&id, output)| {
                    if output.borrow().contains_point(center_x, center_y) {
                        Some(id)
                    } else {
                        None
                    }
                })
            })
        });
        focused_output.or(context.current_output)
    };
    let Some(output_id) = output_id else {
        return;
    };

    open_window_menu(
        state,
        output_id,
        0,
        0,
        true,
        canoe::WindowMenuMode::AltTab,
        Some("Windows".to_string()),
        qh,
    );
    ensure_window_menu_shield(state, output_id, qh);
}

fn handle_window_menu_cycle_app(state: &mut AppState, qh: &QueueHandle<AppState>) {
    let (app_id, output_id, items) = {
        let context = state.context.borrow();
        let app_id = context
            .focused_window
            .and_then(|window_id| context.windows.get(&window_id))
            .and_then(|window| window.borrow().app_id.clone())
            .filter(|app_id| !app_id.is_empty());
        let Some(app_id) = app_id else {
            return;
        };
        let output_id = {
            let focused_output = context.focused_window.and_then(|window_id| {
                context.windows.get(&window_id).and_then(|window| {
                    let window_ref = window.borrow();
                    let center_x = window_ref.x + (window_ref.width / 2);
                    let center_y = window_ref.y + (window_ref.height / 2);
                    if let Some(output) = window_ref.output.as_ref().and_then(|o| o.upgrade()) {
                        let out_ref = output.borrow();
                        if out_ref.contains_point(center_x, center_y) {
                            return Some(out_ref.id);
                        }
                    }
                    context.outputs.iter().find_map(|(&id, output)| {
                        if output.borrow().contains_point(center_x, center_y) {
                            Some(id)
                        } else {
                            None
                        }
                    })
                })
            });
            focused_output.or(context.current_output)
        };
        let Some(output_id) = output_id else {
            return;
        };
        let items = context.collect_menu_items_for_app(output_id, &app_id);
        if items.len() < 2 {
            return;
        }
        (app_id, output_id, items)
    };

    let mut should_render = false;
    let mut open_new = false;
    let mut ensure_shield = None;
    let mut preview_window = None;

    {
        let mut context = state.context.borrow_mut();
        let is_alt_tab = context.window_menu_mode == Some(canoe::WindowMenuMode::AltTab);
        let matches_app = if is_alt_tab {
            context
                .window_menu
                .as_ref()
                .map(|menu| menu_matches_app_id(&context, menu, &app_id))
                .unwrap_or(false)
        } else {
            false
        };
        if let Some(menu) = context.window_menu.as_mut() {
            if matches_app {
                if menu.select_next() {
                    should_render = true;
                    preview_window = menu
                        .hovered
                        .and_then(|idx| menu.items.get(idx).map(|item| item.window_id));
                }
                ensure_shield = Some(menu.output_id);
            } else {
                context.close_window_menu();
                open_new = true;
            }
        } else {
            open_new = true;
        }

        if let Some(window_id) = preview_window {
            context.preview_alt_tab_window(window_id);
        }
    }

    if should_render {
        render_window_menu(state, qh);
    }

    if let Some(output_id) = ensure_shield {
        ensure_window_menu_shield(state, output_id, qh);
    }

    if !open_new {
        return;
    }

    open_window_menu_with_items(
        state,
        output_id,
        (0, 0),
        true,
        canoe::WindowMenuMode::AltTab,
        items,
        Some(app_id),
        qh,
    );
    ensure_window_menu_shield(state, output_id, qh);
}

fn handle_window_menu_commit(state: &mut AppState, seat_id: canoe::SeatId) {
    let Some(seat) = state.context.borrow().seats.get(&seat_id).cloned() else {
        return;
    };
    seat.borrow_mut()
        .queue_action(binding::Action::WindowMenuCommit);
}

fn ensure_desktop_surface(
    state: &mut AppState,
    output_id: canoe::OutputId,
    qh: &QueueHandle<AppState>,
) {
    let (Some(compositor), Some(layer_shell)) = (
        state.globals.compositor.as_ref(),
        state.globals.wlr_layer_shell.as_ref(),
    ) else {
        return;
    };

    let output = {
        let context = state.context.borrow();
        context.outputs.get(&output_id).cloned()
    };
    let Some(output) = output else {
        return;
    };

    if output.borrow().desktop_surface.is_some() {
        return;
    }

    let wl_output = output.borrow().wl_output.clone();
    let surface = compositor.create_surface(qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        wl_output.as_ref(),
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer::Background,
        "canoe-desktop".to_string(),
        qh,
        canoe::LayerSurfaceKind::Desktop(output_id),
    );

    layer_surface.set_anchor(
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Top
            | wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Bottom
            | wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Left
            | wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor::Right,
    );
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(
        wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::KeyboardInteractivity::None,
    );
    layer_surface.set_size(0, 0);
    surface.commit();

    output.borrow_mut().desktop_surface = Some(canoe::DesktopSurface::new(
        surface,
        layer_surface,
        output_id,
    ));
}

fn render_desktop_surface(
    state: &mut AppState,
    output_id: canoe::OutputId,
    qh: &QueueHandle<AppState>,
) {
    // Check dirty flag early to skip all work when nothing changed.
    {
        let context = state.context.borrow();
        let is_dirty = context
            .outputs
            .get(&output_id)
            .and_then(|o| o.borrow().desktop_surface.as_ref().map(|d| d.dirty))
            .unwrap_or(false);
        if !is_dirty {
            return;
        }
    }

    let (Some(shm), Some(compositor)) = (
        state.globals.shm.as_ref(),
        state.globals.compositor.as_ref(),
    ) else {
        return;
    };

    let (bg_color, icons, icon_theme, font_name, font_size, label_font_name, label_font_size, scale) = {
        let mut context = state.context.borrow_mut();
        let ui = &context.config.ui;
        let bg_color = ui.desktop_background;
        let icon_theme = canoe::IconTheme {
            text: ui.icons_text.unwrap_or(ui.menu_text),
            highlight_bg: ui.icons_highlight_bg.unwrap_or(ui.menu_highlight_bg),
            highlight_text: ui.icons_highlight_text.unwrap_or(ui.menu_highlight_text),
            titlebar_bg: ui.titlebar_bg_inactive,
            titlebar_text: ui.titlebar_text_inactive,
            border: 0x404040FF,
        };
        let font_name = ui.font_name.clone();
        let font_size = ui.font_size;
        let label_font_name = match ui.icons_font_name.as_deref() {
            Some(name) => name.to_string(),
            None => canoe::regular_weight_font_query(font_name.as_deref()),
        };
        let label_font_size = ui.icons_font_size.unwrap_or(font_size * 0.80);
        let icons_enabled = ui.icons_enabled;
        let scale = context
            .outputs
            .get(&output_id)
            .map(|o| o.borrow().scale)
            .unwrap_or(1);
        let icons = if icons_enabled {
            context.collect_minimized_icons(output_id)
        } else {
            Vec::new()
        };
        (
            bg_color,
            icons,
            icon_theme,
            font_name,
            font_size,
            label_font_name,
            label_font_size,
            scale,
        )
    };

    let output = {
        let context = state.context.borrow();
        context.outputs.get(&output_id).cloned()
    };
    let Some(output) = output else {
        return;
    };

    let mut out = output.borrow_mut();
    let Some(desktop) = out.desktop_surface.as_mut() else {
        return;
    };
    if !desktop.configured {
        return;
    }

    desktop.ensure_buffer(shm, qh, scale);
    desktop.render_with_icons(
        bg_color,
        &icons,
        &icon_theme,
        scale,
        font_name.as_deref(),
        font_size,
        Some(label_font_name.as_str()),
        label_font_size,
    );
    desktop.update_input_region(compositor, qh);
    desktop.commit();
    desktop.dirty = false;
}

fn render_all_desktop_surfaces(state: &mut AppState, qh: &QueueHandle<AppState>) {
    let output_ids: Vec<canoe::OutputId> = {
        let context = state.context.borrow();
        context.outputs.keys().copied().collect()
    };
    for output_id in output_ids {
        render_desktop_surface(state, output_id, qh);
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    let compositor: wl_compositor::WlCompositor =
                        registry.bind(name, version.min(4), qh, ());
                    state.globals.compositor = Some(compositor);
                }
                "wl_shm" => {
                    let shm: wl_shm::WlShm = registry.bind(name, version.min(1), qh, ());
                    state.globals.shm = Some(shm);
                }
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.globals.wl_outputs.insert(name, output.clone());
                    state.globals.wl_output_scales.entry(name).or_insert(1);
                    let outputs: Vec<_> =
                        state.context.borrow().outputs.values().cloned().collect();
                    for output_ref in outputs {
                        let mut out = output_ref.borrow_mut();
                        if out.wl_output_name != name {
                            continue;
                        }
                        out.wl_output = Some(output.clone());
                        out.scale = state
                            .globals
                            .wl_output_scales
                            .get(&name)
                            .copied()
                            .unwrap_or(1);
                        let output_id = out.id;
                        let had_desktop = out.desktop_surface.is_some();
                        if had_desktop {
                            out.desktop_surface = None;
                        }
                        drop(out);
                        if had_desktop || state.globals.wlr_layer_shell.is_some() {
                            ensure_desktop_surface(state, output_id, qh);
                        }
                    }
                }
                "wl_seat" => {
                    let seat: wl_seat::WlSeat = registry.bind(name, version.min(7), qh, name);
                    state.globals.wl_seats.insert(name, seat);
                    state.globals.wl_seat_has_pointer.insert(name, false);

                    let seats: Vec<_> = state.context.borrow().seats.values().cloned().collect();
                    for seat_ref in seats {
                        if seat_ref.borrow().wl_seat_name == name {
                            attach_wl_seat(state, &seat_ref, qh);
                            break;
                        }
                    }
                }
                "river_window_manager_v1" => {
                    let rwm: RiverWindowManagerV1 = registry.bind(name, version.min(3), qh, ());
                    state.globals.rwm = Some(rwm.clone());
                    state.context.borrow_mut().rwm = Some(rwm);
                }
                "river_xkb_bindings_v1" => {
                    let xkb: RiverXkbBindingsV1 = registry.bind(name, version.min(1), qh, ());
                    state.globals.rwm_xkb_bindings = Some(xkb.clone());
                    state.context.borrow_mut().rwm_xkb_bindings = Some(xkb);
                }
                "river_layer_shell_v1" => {
                    let ls: RiverLayerShellV1 = registry.bind(name, version.min(1), qh, ());
                    state.globals.rwm_layer_shell = Some(ls.clone());
                    state.context.borrow_mut().rwm_layer_shell = Some(ls);
                }
                "zwlr_layer_shell_v1" => {
                    let layer_shell: ZwlrLayerShellV1 = registry.bind(name, version.min(4), qh, ());
                    state.globals.wlr_layer_shell = Some(layer_shell);
                    let outputs: Vec<_> = state.context.borrow().outputs.keys().copied().collect();
                    for output_id in outputs {
                        ensure_desktop_surface(state, output_id, qh);
                    }
                }
                "river_input_manager_v1" => {
                    let im: RiverInputManagerV1 = registry.bind(name, version.min(1), qh, ());
                    state.globals.rwm_input_manager = Some(im.clone());
                    state.context.borrow_mut().rwm_input_manager = Some(im);
                }
                "river_libinput_config_v1" => {
                    let lc: RiverLibinputConfigV1 = registry.bind(name, version.min(1), qh, ());
                    state.globals.rwm_libinput_config = Some(lc.clone());
                    state.context.borrow_mut().rwm_libinput_config = Some(lc);
                }
                "wp_cursor_shape_manager_v1" => {
                    let manager: WpCursorShapeManagerV1 =
                        registry.bind(name, version.min(2), qh, ());
                    state.globals.cursor_shape_manager = Some(manager);
                    let seats: Vec<_> = state.context.borrow().seats.values().cloned().collect();
                    for seat_ref in seats {
                        attach_cursor_shape_device(state, &seat_ref, qh);
                    }
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrLayerShellV1,
        _event: wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // zwlr_layer_shell_v1 has no events.
    }
}

// Implement dispatch for River Window Manager protocol
impl Dispatch<RiverWindowManagerV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &RiverWindowManagerV1,
        event: river_window_management_v1::client::river_window_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_window_management_v1::client::river_window_manager_v1::Event;

        match event {
            Event::Unavailable => {
                state.context.borrow_mut().running = false;
            }
            Event::Finished => {
                state.context.borrow_mut().running = false;
            }
            Event::ManageStart => {
                state.context.borrow_mut().handle_manage_start();
                // If in icon focus mode, validate that the selected icon still exists
                {
                    let mut context = state.context.borrow_mut();
                    if let Some(output_id) = context.icon_focus_output {
                        let selected_still_valid = context
                            .outputs
                            .get(&output_id)
                            .and_then(|o| {
                                o.borrow()
                                    .desktop_surface
                                    .as_ref()
                                    .and_then(|d| d.selected_icon)
                            })
                            .map(|wid| {
                                context
                                    .windows
                                    .get(&wid)
                                    .map(|w| w.borrow().hidden)
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        if !selected_still_valid {
                            if let Some(seat_id) = context.current_seat {
                                context.exit_icon_focus(seat_id);
                            }
                        }
                    }
                }
                state.context.borrow().finish_manage();
                render_all_desktop_surfaces(state, qh);
            }
            Event::RenderStart => {
                state.context.borrow_mut().handle_render_start();

                // Update titlebars
                if let Some(ref shm) = state.globals.shm {
                    let context = state.context.borrow();
                    let focused_window = context.focused_window;
                    let ui = &context.config.ui;

                    for (&window_id, window) in &context.windows {
                        let mut w = window.borrow_mut();

                        let hide_titlebar = w.hidden
                            || w.decoration == Some(crate::config::WindowDecoration::Csd);
                        if hide_titlebar {
                            if let Some(ref mut titlebar) = w.titlebar {
                                if titlebar.mapped {
                                    titlebar.sync_next_commit();
                                    titlebar.unmap();
                                }
                            }
                            continue;
                        }

                        // Extract window data before borrowing titlebar
                        let width = w.width;
                        let title = w.title.clone();
                        let is_focused = focused_window == Some(window_id);
                        let is_maximized = w.maximized;
                        let height = w.height;
                        let hovered_button = w.titlebar_hovered;
                        let titlebar_left_down = w.titlebar_left_down;
                        let swallow_top = w.swallow_top;

                        // Update titlebar if it exists and window has valid dimensions
                        let output_scale = w
                            .output
                            .as_ref()
                            .and_then(|o| o.upgrade())
                            .map(|o| o.borrow().scale);
                        let titlebar_output_names = w
                            .titlebar
                            .as_ref()
                            .map(|t| t.output_names.clone())
                            .unwrap_or_default();
                        let titlebar_surface_scale = if titlebar_output_names.is_empty() {
                            None
                        } else {
                            let mut max_scale = 1;
                            for name in &titlebar_output_names {
                                if let Some(scale) = state.globals.wl_output_scales.get(name) {
                                    max_scale = max_scale.max(*scale);
                                }
                            }
                            Some(max_scale)
                        };
                        let scale = titlebar_surface_scale.or(output_scale).unwrap_or(1);
                        if let Some(ref mut titlebar) = w.titlebar {
                            if width > 0 && height > 0 {
                                let content_height = (height - swallow_top).max(1);
                                // Ensure buffer is allocated
                                titlebar.ensure_buffer(width, content_height, shm, qh, scale, ui);
                                if let Some(ref compositor) = state.globals.compositor {
                                    titlebar.update_input_region(compositor, qh, ui);
                                }

                                // Render titlebar content
                                let did_render = titlebar.render(
                                    title.as_deref(),
                                    is_focused,
                                    is_maximized,
                                    hovered_button,
                                    titlebar_left_down,
                                    ui,
                                );

                                // Position decoration so it sits above content with borders
                                let border_width = ui.border_width;
                                let titlebar_height = canoe::titlebar::titlebar_height(ui);
                                titlebar.set_offset(
                                    -border_width,
                                    -border_width - titlebar_height + swallow_top,
                                );

                                // Sync and commit (only if we have a buffer)
                                if did_render && titlebar.buffer.is_some() {
                                    titlebar.sync_next_commit();
                                    titlebar.commit();
                                }
                            }
                        }
                    }
                }

                state.context.borrow().finish_render();
            }
            Event::Window { id } => {
                let window = state.context.borrow_mut().create_window(id.clone());
                let window_id = window.borrow().id;

                // Get the node for this window
                let node: RiverNodeV1 = id.get_node(qh, window_id);
                window.borrow_mut().rwm_node = Some(node);

                // Create titlebar if compositor is available
                if let Some(ref compositor) = state.globals.compositor {
                    // Create surface for titlebar
                    let surface = compositor.create_surface(qh, TitlebarSurfaceData { window_id });

                    // Create decoration for titlebar (above window content)
                    let decoration: RiverDecorationV1 =
                        id.get_decoration_above(&surface, qh, window_id);

                    // Create titlebar
                    let titlebar = canoe::Titlebar::new(surface, decoration);
                    window.borrow_mut().titlebar = Some(titlebar);
                }

                // Queue init event
                window.borrow_mut().queue_event(canoe::WindowEvent::Init);
            }
            Event::Output { id } => {
                let output = state.context.borrow_mut().create_output(id.clone());

                // Get layer shell output if available
                if let Some(ref layer_shell) = state.globals.rwm_layer_shell {
                    let ls_output: RiverLayerShellOutputV1 =
                        layer_shell.get_output(&id, qh, output.borrow().id);
                    output.borrow_mut().layer_shell_output = Some(ls_output);
                }
            }
            Event::Seat { id } => {
                let seat = state.context.borrow_mut().create_seat(id.clone());
                let seat_id = seat.borrow().id;

                // Get layer shell seat if available
                if let Some(ref layer_shell) = state.globals.rwm_layer_shell {
                    let ls_seat: RiverLayerShellSeatV1 = layer_shell.get_seat(&id, qh, seat_id);
                    seat.borrow_mut().layer_shell_seat = Some(ls_seat);
                }

                // Register XKB bindings with the compositor
                if let Some(ref xkb_bindings_global) = state.globals.rwm_xkb_bindings {
                    let mut seat_ref = seat.borrow_mut();

                    for (idx, (binding, rwm_binding_slot)) in
                        seat_ref.xkb_bindings.iter_mut().enumerate()
                    {
                        // Protocol: get_xkb_binding(seat, keysym, modifiers) -> new_id
                        // wayland-client adds qh and user_data at the end
                        use river_window_management_v1::client::river_seat_v1::Modifiers;
                        let mods = Modifiers::from_bits_truncate(binding.modifiers);
                        let rwm_binding: RiverXkbBindingV1 = xkb_bindings_global.get_xkb_binding(
                            &id,
                            binding.keysym,
                            mods,
                            qh,
                            (seat_id, idx),
                        );

                        // Enable binding if it's for the current mode
                        if binding.enabled {
                            rwm_binding.enable();
                        }

                        *rwm_binding_slot = Some(rwm_binding);
                    }
                }

                // Register pointer bindings with the compositor
                {
                    let mut seat_ref = seat.borrow_mut();
                    let rwm_seat = seat_ref.rwm_seat.clone();
                    if let Some(rwm_seat) = rwm_seat {
                        for (idx, (binding, rwm_binding_slot)) in
                            seat_ref.pointer_bindings.iter_mut().enumerate()
                        {
                            // Protocol: get_pointer_binding(button, modifiers) -> new_id on river_seat_v1
                            use river_window_management_v1::client::river_seat_v1::Modifiers;
                            let mods = Modifiers::from_bits_truncate(binding.modifiers);
                            let rwm_binding: RiverPointerBindingV1 = rwm_seat.get_pointer_binding(
                                binding.button,
                                mods,
                                qh,
                                (seat_id, idx),
                            );

                            // Enable binding if it's for the current mode
                            if binding.enabled {
                                rwm_binding.enable();
                            }

                            *rwm_binding_slot = Some(rwm_binding);
                        }
                    }
                }
            }
            Event::SessionLocked => {
                state.context.borrow_mut().session_locked = true;
                // Switch to lock mode
                if let Some(seat_id) = state.context.borrow().current_seat {
                    if let Some(seat) = state.context.borrow().seats.get(&seat_id) {
                        seat.borrow_mut().switch_mode(config::Mode::Lock);
                    }
                }
            }
            Event::SessionUnlocked => {
                state.context.borrow_mut().session_locked = false;
                // Switch back to default mode
                if let Some(seat_id) = state.context.borrow().current_seat {
                    if let Some(seat) = state.context.borrow().seats.get(&seat_id) {
                        seat.borrow_mut().switch_mode(config::Mode::Default);
                    }
                }
            }
        }
    }

    fn event_created_child(
        opcode: u16,
        qhandle: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            // window event (opcode 6) creates a river_window_v1
            6 => qhandle.make_data::<RiverWindowV1, _>(0usize), // placeholder window id
            // output event (opcode 7) creates a river_output_v1
            7 => qhandle.make_data::<RiverOutputV1, _>(0usize), // placeholder output id
            // seat event (opcode 8) creates a river_seat_v1
            8 => qhandle.make_data::<RiverSeatV1, _>(0usize), // placeholder seat id
            _ => unreachable!("unknown event opcode {}", opcode),
        }
    }
}

// Implement dispatch for River Window
impl Dispatch<RiverWindowV1, canoe::WindowId> for AppState {
    fn event(
        state: &mut Self,
        proxy: &RiverWindowV1,
        event: river_window_management_v1::client::river_window_v1::Event,
        _window_id: &canoe::WindowId, // Don't use this - it's always 0 from event_created_child
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use river_window_management_v1::client::river_window_v1::Event;

        // Find window by matching the RiverWindowV1 object, not by user data
        let context = state.context.borrow();
        let found = context.windows.iter().find_map(|(&id, w)| {
            if w.borrow()
                .rwm_window
                .as_ref()
                .map(|rw| rw == proxy)
                .unwrap_or(false)
            {
                Some((id, w.clone()))
            } else {
                None
            }
        });
        let (window_id, window) = match found {
            Some(f) => f,
            None => return,
        };
        drop(context);

        match event {
            Event::Closed => {
                state.context.borrow_mut().destroy_window(window_id);
            }
            Event::DimensionsHint {
                min_width,
                min_height,
                max_width: _,
                max_height: _,
            } => {
                let mut w = window.borrow_mut();
                w.min_width = min_width;
                w.min_height = min_height;
            }
            Event::Dimensions { width, height } => {
                window.borrow_mut().update_dimensions(width, height);
            }
            Event::AppId { app_id } => {
                let mut w = window.borrow_mut();
                w.app_id = app_id;
                state.context.borrow().apply_rules_to_window(&mut w);
            }
            Event::Title { title } => {
                let mut w = window.borrow_mut();
                w.title = title;
                state.context.borrow().apply_rules_to_window(&mut w);
            }
            Event::Parent { parent } => {
                let parent_id = parent.and_then(|parent_proxy| {
                    let context = state.context.borrow();
                    context.windows.iter().find_map(|(&id, w)| {
                        if w.borrow()
                            .rwm_window
                            .as_ref()
                            .map(|rw| rw == &parent_proxy)
                            .unwrap_or(false)
                        {
                            Some(id)
                        } else {
                            None
                        }
                    })
                });
                {
                    let mut w = window.borrow_mut();
                    w.parent = parent_id;
                    state.context.borrow().apply_rules_to_window(&mut w);
                }
                state
                    .context
                    .borrow_mut()
                    .assign_output_for_window(window_id);
            }
            Event::DecorationHint {
                hint: wayland_client::WEnum::Value(h),
            } => {
                let mut w = window.borrow_mut();
                w.decoration_hint = Some(h as u32);
                state.context.borrow().apply_rules_to_window(&mut w);
            }
            Event::UnreliablePid { unreliable_pid } => {
                let mut w = window.borrow_mut();
                w.pid = unreliable_pid;
            }
            Event::PointerMoveRequested { seat } => {
                // Find the seat and queue move action
                let context = state.context.borrow();
                if let Some((_seat_id, seat_rc)) = context.seats.iter().find(|(_, s)| {
                    s.borrow()
                        .rwm_seat
                        .as_ref()
                        .map(|rs| rs == &seat)
                        .unwrap_or(false)
                }) {
                    window
                        .borrow_mut()
                        .queue_event(canoe::WindowEvent::Move(Rc::downgrade(seat_rc)));
                }
            }
            Event::PointerResizeRequested { seat, edges } => {
                let context = state.context.borrow();
                if let Some((_, seat_rc)) = context.seats.iter().find(|(_, s)| {
                    s.borrow()
                        .rwm_seat
                        .as_ref()
                        .map(|rs| rs == &seat)
                        .unwrap_or(false)
                }) {
                    // Convert WEnum<Edges> to u32
                    let edges_u32 = if let wayland_client::WEnum::Value(e) = edges {
                        e.bits()
                    } else {
                        0
                    };
                    window.borrow_mut().queue_event(canoe::WindowEvent::Resize(
                        Rc::downgrade(seat_rc),
                        edges_u32,
                    ));
                }
            }
            Event::FullscreenRequested { output } => {
                let output_weak = output.and_then(|o| {
                    let context = state.context.borrow();
                    context.outputs.iter().find_map(|(_, out)| {
                        if out
                            .borrow()
                            .rwm_output
                            .as_ref()
                            .map(|ro| ro == &o)
                            .unwrap_or(false)
                        {
                            Some(Rc::downgrade(out))
                        } else {
                            None
                        }
                    })
                });
                window
                    .borrow_mut()
                    .queue_event(canoe::WindowEvent::Fullscreen(output_weak));
            }
            Event::ExitFullscreenRequested => {
                window
                    .borrow_mut()
                    .queue_event(canoe::WindowEvent::Unfullscreen);
            }
            Event::MaximizeRequested => {
                window
                    .borrow_mut()
                    .queue_event(canoe::WindowEvent::Maximize);
            }
            Event::UnmaximizeRequested => {
                window
                    .borrow_mut()
                    .queue_event(canoe::WindowEvent::Unmaximize);
            }
            Event::MinimizeRequested => {
                window
                    .borrow_mut()
                    .queue_event(canoe::WindowEvent::Minimize);
            }
            _ => {}
        }
    }
}

// Implement dispatch for River Node
impl Dispatch<RiverNodeV1, canoe::WindowId> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &RiverNodeV1,
        _event: river_window_management_v1::client::river_node_v1::Event,
        _data: &canoe::WindowId,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Node events would be handled here if needed
    }
}

// Implement dispatch for River Output
impl Dispatch<RiverOutputV1, canoe::OutputId> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &RiverOutputV1,
        event: river_window_management_v1::client::river_output_v1::Event,
        _output_id: &canoe::OutputId,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_window_management_v1::client::river_output_v1::Event;

        let (output_id, output) = {
            let context = state.context.borrow();
            let found = context.outputs.iter().find_map(|(id, output)| {
                if output
                    .borrow()
                    .rwm_output
                    .as_ref()
                    .map(|rwm| rwm == _proxy)
                    .unwrap_or(false)
                {
                    Some((*id, output.clone()))
                } else {
                    None
                }
            });
            let Some(found) = found else {
                return;
            };
            found
        };

        match event {
            Event::Removed => {
                state.context.borrow_mut().destroy_output(output_id);
            }
            Event::WlOutput { name } => {
                let mut out = output.borrow_mut();
                out.wl_output_name = name;
                out.wl_output = state.globals.wl_outputs.get(&name).cloned();
                out.scale = state
                    .globals
                    .wl_output_scales
                    .get(&name)
                    .copied()
                    .unwrap_or(1);
                let had_desktop = out.desktop_surface.is_some();
                if had_desktop {
                    out.desktop_surface = None;
                }
                drop(out);
                ensure_desktop_surface(state, output_id, qh);
            }
            Event::Position { x, y } => {
                output.borrow_mut().update_position(x, y);
            }
            Event::Dimensions { width, height } => {
                output.borrow_mut().update_dimensions(width, height);
            }
        }
    }
}

// Implement dispatch for River Seat
impl Dispatch<RiverSeatV1, canoe::SeatId> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &RiverSeatV1,
        event: river_window_management_v1::client::river_seat_v1::Event,
        seat_id: &canoe::SeatId,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_window_management_v1::client::river_seat_v1::Event;

        let context = state.context.borrow();
        let seat = match context.seats.get(seat_id) {
            Some(s) => s.clone(),
            None => return,
        };
        drop(context);

        match event {
            Event::Removed => {
                state.context.borrow_mut().destroy_seat(*seat_id);
            }
            Event::WlSeat { name } => {
                seat.borrow_mut().wl_seat_name = name;
                attach_wl_seat(state, &seat, qh);
            }
            Event::PointerEnter { window } => {
                // Find the window
                let context = state.context.borrow();
                let found = context.windows.iter().find_map(|(id, w)| {
                    if w.borrow()
                        .rwm_window
                        .as_ref()
                        .map(|rw| rw == &window)
                        .unwrap_or(false)
                    {
                        Some((*id, Rc::downgrade(w)))
                    } else {
                        None
                    }
                });
                let sloppy_focus = context.config.sloppy_focus;
                drop(context);

                if let Some((wid, weak)) = found {
                    seat.borrow_mut().window_below_pointer = Some(weak);

                    // Focus on hover if sloppy focus is enabled
                    if sloppy_focus {
                        state.context.borrow_mut().focus(wid);
                    }
                    state.context.borrow_mut().update_cursor_for_seat(*seat_id);
                }
            }
            Event::PointerLeave => {
                seat.borrow_mut().window_below_pointer = None;
                state.context.borrow_mut().update_cursor_for_seat(*seat_id);
            }
            Event::WindowInteraction { window } => {
                // Always focus the window on click/interaction
                let context = state.context.borrow();
                if let Some((&wid, _)) = context.windows.iter().find(|(_, w)| {
                    w.borrow()
                        .rwm_window
                        .as_ref()
                        .map(|rw| rw == &window)
                        .unwrap_or(false)
                }) {
                    drop(context);
                    state.context.borrow_mut().close_window_menu();
                    let mut context = state.context.borrow_mut();
                    if context.icon_focus_output.is_some() {
                        context.exit_icon_focus(*seat_id);
                    }
                    context.focus(wid);
                    context.handle_window_interaction(*seat_id, wid);
                }
            }
            Event::OpDelta { dx, dy } => {
                // Apply delta to window being operated on
                let context = state.context.borrow();
                if let Some(wid) = context.focused_window {
                    if let Some(window) = context.windows.get(&wid) {
                        window.borrow_mut().apply_op_delta(dx, dy);
                    }
                }
            }
            Event::OpRelease => {
                // End operation
                let (window_id, was_move) = {
                    let context = state.context.borrow();
                    if let Some(wid) = context.focused_window {
                        if let Some(window) = context.windows.get(&wid) {
                            let mut w = window.borrow_mut();
                            let was_move =
                                matches!(w.operator, canoe::window::Operator::Move { .. });
                            w.end_operation();
                            (Some(wid), was_move)
                        } else {
                            (None, false)
                        }
                    } else {
                        (None, false)
                    }
                };
                seat.borrow().end_pointer_op();
                if was_move {
                    if let Some(window_id) = window_id {
                        state
                            .context
                            .borrow_mut()
                            .update_window_output_from_position(window_id);
                    }
                }
                state.context.borrow_mut().update_cursor_for_seat(*seat_id);
            }
            Event::PointerPosition { x, y } => {
                seat.borrow_mut().update_pointer_position(x, y);
                state
                    .context
                    .borrow_mut()
                    .update_window_output_from_pointer(*seat_id, x, y);
                state.context.borrow_mut().update_cursor_for_seat(*seat_id);
                update_menu_hover_from_global(state, *seat_id, qh);
                let titlebar_window =
                    state
                        .context
                        .borrow()
                        .windows
                        .iter()
                        .find_map(|(&window_id, window)| {
                            if window.borrow().titlebar_left_down {
                                Some(window_id)
                            } else {
                                None
                            }
                        });
                if let Some(window_id) = titlebar_window {
                    if update_titlebar_hover_from_global(state, window_id, x, y) {
                        request_manage_dirty(state);
                    }
                }
            }
            _ => {}
        }
    }
}

// Implement dispatch for wlr layer surfaces (desktop/menu)
impl Dispatch<ZwlrLayerSurfaceV1, canoe::LayerSurfaceKind> for AppState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Event,
        kind: &canoe::LayerSurfaceKind,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Event;

        match event {
            Event::Configure {
                serial,
                width,
                height,
            } => {
                proxy.ack_configure(serial);
                match kind {
                    canoe::LayerSurfaceKind::Desktop(output_id) => {
                        let output = {
                            let context = state.context.borrow();
                            context.outputs.get(output_id).cloned()
                        };
                        let Some(output) = output else {
                            return;
                        };
                        {
                            let mut out = output.borrow_mut();
                            let Some(desktop) = out.desktop_surface.as_mut() else {
                                return;
                            };
                            if (desktop.width, desktop.height) != (width as i32, height as i32) {
                                desktop.reset_buffer();
                            }
                            desktop.configure(width as i32, height as i32);
                        }
                        render_desktop_surface(state, *output_id, qh);
                    }
                    canoe::LayerSurfaceKind::Menu => {
                        let mut context = state.context.borrow_mut();
                        let Some(menu) = context.window_menu.as_mut() else {
                            return;
                        };
                        if width > 0
                            && height > 0
                            && (menu.width, menu.height) != (width as i32, height as i32)
                        {
                            menu.reset_buffer();
                            menu.width = width as i32;
                            menu.height = height as i32;
                        }
                        menu.configured = true;
                        drop(context);
                        render_window_menu(state, qh);
                    }
                    canoe::LayerSurfaceKind::MenuShield(output_id) => {
                        let mut context = state.context.borrow_mut();
                        let Some(shield) = context.window_menu_shield.as_mut() else {
                            return;
                        };
                        if shield.output_id != *output_id {
                            return;
                        }
                        if width > 0
                            && height > 0
                            && (shield.width, shield.height) != (width as i32, height as i32)
                        {
                            shield.reset_buffer();
                            shield.width = width as i32;
                            shield.height = height as i32;
                        }
                        shield.configured = true;
                        if let (Some(shm), Some(compositor)) = (
                            state.globals.shm.as_ref(),
                            state.globals.compositor.as_ref(),
                        ) {
                            shield.ensure_buffer(shm, qh);
                            shield.render();
                            shield.update_input_region(compositor, qh);
                            shield.commit();
                        }
                    }
                }
            }
            Event::Closed => match kind {
                canoe::LayerSurfaceKind::Desktop(output_id) => {
                    if let Some(output) = state.context.borrow().outputs.get(output_id) {
                        output.borrow_mut().desktop_surface = None;
                    }
                }
                canoe::LayerSurfaceKind::Menu => {
                    state.context.borrow_mut().close_window_menu();
                }
                canoe::LayerSurfaceKind::MenuShield(output_id) => {
                    if let Some(shield) = state.context.borrow().window_menu_shield.as_ref() {
                        if shield.output_id == *output_id {
                            state.context.borrow_mut().window_menu_shield = None;
                        }
                    }
                }
            },
            _ => {}
        }
    }
}

// Implement dispatch for Layer Shell
impl Dispatch<RiverLayerShellV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &RiverLayerShellV1,
        _event: river_layer_shell_v1::client::river_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Layer shell global events
    }
}

impl Dispatch<RiverLayerShellOutputV1, canoe::OutputId> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &RiverLayerShellOutputV1,
        event: river_layer_shell_v1::client::river_layer_shell_output_v1::Event,
        output_id: &canoe::OutputId,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use river_layer_shell_v1::client::river_layer_shell_output_v1::Event;

        let Event::NonExclusiveArea {
            x,
            y,
            width,
            height,
        } = event;
        {
            if let Some(output) = state.context.borrow().outputs.get(output_id) {
                let mut out = output.borrow_mut();
                let (global_x, global_y) = if out.contains_point(x, y) {
                    (x, y)
                } else {
                    (x + out.x, y + out.y)
                };
                out.update_exclusive_area(global_x, global_y, width, height);
            }
        }
    }
}

impl Dispatch<RiverLayerShellSeatV1, canoe::SeatId> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &RiverLayerShellSeatV1,
        event: river_layer_shell_v1::client::river_layer_shell_seat_v1::Event,
        seat_id: &canoe::SeatId,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use river_layer_shell_v1::client::river_layer_shell_seat_v1::Event;

        if let Some(seat) = state.context.borrow().seats.get(seat_id) {
            match event {
                Event::FocusExclusive => {
                    let mut seat_ref = seat.borrow_mut();
                    seat_ref.focus_exclusive = true;
                    seat_ref.queue_action(binding::Action::ClearFocus);
                }
                Event::FocusNonExclusive => {
                    seat.borrow_mut().focus_exclusive = false;
                }
                Event::FocusNone => {
                    let mut seat_ref = seat.borrow_mut();
                    let was_exclusive = seat_ref.focus_exclusive;
                    seat_ref.focus_exclusive = false;
                    if was_exclusive {
                        seat_ref.queue_action(binding::Action::RestoreFocus);
                    }
                }
            }
        }
    }
}

// Implement dispatch for XKB bindings
impl Dispatch<RiverXkbBindingsV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &RiverXkbBindingsV1,
        _event: river_xkb_bindings_v1::client::river_xkb_bindings_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // XKB bindings global events
    }
}

impl Dispatch<RiverXkbBindingV1, (canoe::SeatId, usize)> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &RiverXkbBindingV1,
        event: river_xkb_bindings_v1::client::river_xkb_binding_v1::Event,
        (seat_id, binding_idx): &(canoe::SeatId, usize),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_xkb_bindings_v1::client::river_xkb_binding_v1::Event;

        let seat = match state.context.borrow().seats.get(seat_id) {
            Some(seat) => seat.clone(),
            None => return,
        };
        let (action, binding_event, enabled) = {
            let seat_ref = seat.borrow();
            let Some((binding, _)) = seat_ref.xkb_bindings.get(*binding_idx) else {
                return;
            };
            let action = binding.action.clone();
            (action, binding.event, binding.enabled)
        };

        match event {
            Event::Pressed => {
                if !enabled || binding_event != binding::BindingEvent::Pressed {
                    return;
                }
                match action {
                    binding::Action::WindowMenuCycle => {
                        handle_window_menu_cycle(state, qh);
                    }
                    binding::Action::WindowMenuCycleApp => {
                        handle_window_menu_cycle_app(state, qh);
                    }
                    binding::Action::IconSelectNext
                    | binding::Action::IconSelectPrev
                    | binding::Action::IconSelectUp
                    | binding::Action::IconSelectDown => {
                        state.context.borrow_mut().execute_action(action, *seat_id);
                        render_all_desktop_surfaces(state, qh);
                    }
                    binding::Action::IconActivate => {
                        state.context.borrow_mut().execute_action(action, *seat_id);
                        render_all_desktop_surfaces(state, qh);
                        request_manage_dirty(state);
                    }
                    binding::Action::IconCancel => {
                        state.context.borrow_mut().execute_action(action, *seat_id);
                        render_all_desktop_surfaces(state, qh);
                    }
                    _ => {
                        seat.borrow_mut().queue_action(action);
                    }
                }
            }
            Event::Released => {
                if !enabled || binding_event != binding::BindingEvent::Released {
                    return;
                }
                match action {
                    binding::Action::WindowMenuCommit => {
                        handle_window_menu_commit(state, *seat_id);
                    }
                    _ => {
                        seat.borrow_mut().queue_action(action);
                    }
                }
            }
        }
    }
}

// Implement dispatch for Pointer bindings
impl Dispatch<RiverPointerBindingV1, (canoe::SeatId, usize)> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &RiverPointerBindingV1,
        event: river_window_management_v1::client::river_pointer_binding_v1::Event,
        (seat_id, binding_idx): &(canoe::SeatId, usize),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use river_window_management_v1::client::river_pointer_binding_v1::Event;

        if let Some(seat) = state.context.borrow().seats.get(seat_id) {
            let seat = seat.clone();
            let mut seat_ref = seat.borrow_mut();

            if let Some((binding, _)) = seat_ref.pointer_bindings.get(*binding_idx) {
                let action = binding.action.clone();

                match event {
                    Event::Pressed => {
                        if binding.enabled && binding.event == binding::BindingEvent::Pressed {
                            seat_ref.queue_action(action);
                        }
                    }
                    Event::Released => {
                        if binding.enabled && binding.event == binding::BindingEvent::Released {
                            seat_ref.queue_action(action);
                        }
                    }
                }
            }
        }
    }
}

// Implement dispatch for Input Manager
impl Dispatch<RiverInputManagerV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &RiverInputManagerV1,
        event: river_input_management_v1::client::river_input_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use river_input_management_v1::client::river_input_manager_v1::Event;

        if let Event::InputDevice { id } = event {
            // Input device created - configure it
            let config = &state.context.borrow().config;
            id.set_repeat_info(config.repeat_rate, config.repeat_delay);
            id.set_scroll_factor(config.scroll_factor);
        }
    }

    fn event_created_child(
        opcode: u16,
        qhandle: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            // input_device event (opcode 1) creates a river_input_device_v1
            1 => qhandle.make_data::<RiverInputDeviceV1, _>(()),
            _ => unreachable!("unknown event opcode {}", opcode),
        }
    }
}

impl Dispatch<RiverInputDeviceV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &RiverInputDeviceV1,
        _event: river_input_management_v1::client::river_input_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Input device events
    }
}

// Implement dispatch for Libinput Config
impl Dispatch<RiverLibinputConfigV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &RiverLibinputConfigV1,
        _event: river_libinput_config_v1::client::river_libinput_config_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Libinput config events
    }

    fn event_created_child(
        opcode: u16,
        qhandle: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            // libinput_device event (opcode 1) creates a river_libinput_device_v1
            1 => qhandle.make_data::<RiverLibinputDeviceV1, _>(()),
            _ => unreachable!("unknown event opcode {}", opcode),
        }
    }
}

impl Dispatch<RiverLibinputDeviceV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &RiverLibinputDeviceV1,
        _event: river_libinput_config_v1::client::river_libinput_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Libinput device events
    }

    fn event_created_child(
        _opcode: u16,
        qhandle: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        qhandle.make_data::<RiverLibinputResultV1, _>(())
    }
}

impl Dispatch<RiverLibinputResultV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &RiverLibinputResultV1,
        _event: river_libinput_config_v1::client::river_libinput_result_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Libinput config result events
    }
}

// Standard Wayland protocol dispatches for titlebar surfaces
impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_compositor has no events
    }
}

impl Dispatch<wl_region::WlRegion, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_region has no events
    }
}

impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format { format: _ } = event {
            // We only need ARGB8888 which is always available
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_shm_pool has no events
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            // Buffer is no longer in use by compositor
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for AppState {
    fn event(
        state: &mut Self,
        proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        seat_name: &u32,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let has_pointer = matches!(
                capabilities,
                wayland_client::WEnum::Value(caps)
                    if caps.contains(wl_seat::Capability::Pointer)
            );
            state
                .globals
                .wl_seat_has_pointer
                .insert(*seat_name, has_pointer);

            let seat_ref = {
                let context = state.context.borrow();
                context
                    .seats
                    .values()
                    .find(|seat| seat.borrow().wl_seat_name == *seat_name)
                    .cloned()
            };

            if let Some(seat_ref) = seat_ref {
                if has_pointer {
                    if seat_ref.borrow().wl_pointer.is_none() {
                        let wl_pointer = proxy.get_pointer(qh, seat_ref.borrow().id);
                        seat_ref.borrow_mut().wl_pointer = Some(wl_pointer);
                    }
                    attach_cursor_shape_device(state, &seat_ref, qh);
                } else {
                    let mut seat = seat_ref.borrow_mut();
                    if let Some(pointer) = seat.wl_pointer.take() {
                        pointer.release();
                    }
                    seat.cursor_shape_device = None;
                    seat.pointer_enter_serial = 0;
                    seat.cursor_shape = None;
                }
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, canoe::SeatId> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        seat_id: &canoe::SeatId,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let seat = {
            let context = state.context.borrow();
            context.seats.get(seat_id).cloned()
        };
        if let Some(seat) = seat {
            match event {
                wl_pointer::Event::Enter {
                    serial,
                    surface,
                    surface_x,
                    surface_y,
                } => {
                    let wl_pointer = seat.borrow().wl_pointer.clone();
                    let mut target = canoe::PointerTarget::None;
                    let mut titlebar_window = None;

                    let surface_pos = (surface_x.round() as i32, surface_y.round() as i32);
                    {
                        let mut context = state.context.borrow_mut();
                        if let Some(shield) = context.window_menu_shield.as_ref() {
                            if shield.surface == surface {
                                target = canoe::PointerTarget::MenuShield(shield.output_id);
                            }
                        }
                        if let Some(menu) = context.window_menu.as_mut() {
                            if menu.surface == surface {
                                target = canoe::PointerTarget::Menu;
                                let changed =
                                    context.update_menu_hover(surface_pos.0, surface_pos.1);
                                if changed {
                                    drop(context);
                                    render_window_menu(state, _qh);
                                }
                            }
                        }
                    }

                    if target == canoe::PointerTarget::None {
                        let context = state.context.borrow();
                        for (&window_id, window) in &context.windows {
                            if let Some(titlebar) = window.borrow().titlebar.as_ref() {
                                if titlebar.surface == surface {
                                    target = canoe::PointerTarget::Titlebar(window_id);
                                    titlebar_window = Some(window_id);
                                    break;
                                }
                            }
                        }
                    }

                    if target == canoe::PointerTarget::None {
                        let context = state.context.borrow();
                        for (output_id, output) in &context.outputs {
                            if let Some(desktop) = output.borrow().desktop_surface.as_ref() {
                                if desktop.surface == surface {
                                    target = canoe::PointerTarget::Desktop(*output_id);
                                    break;
                                }
                            }
                        }
                    }

                    {
                        let mut seat_ref = seat.borrow_mut();
                        seat_ref.pointer_enter_serial = serial;
                        seat_ref.pointer_target = target;
                        seat_ref.last_surface_x = surface_pos.0;
                        seat_ref.last_surface_y = surface_pos.1;
                    }
                    if let canoe::PointerTarget::Desktop(output_id)
                    | canoe::PointerTarget::MenuShield(output_id) = target
                    {
                        update_pointer_position_from_output(
                            state, &seat, output_id, surface_x, surface_y,
                        );
                    }
                    if matches!(target, canoe::PointerTarget::MenuShield(_)) {
                        if let Some(pointer) = wl_pointer.as_ref() {
                            pointer.set_cursor(serial, None, 0, 0);
                        }
                    } else {
                        state.context.borrow_mut().update_cursor_for_seat(*seat_id);
                    }
                    if let Some(window_id) = titlebar_window {
                        let changed = update_titlebar_hover_from_surface(
                            state, window_id, surface_x, surface_y,
                        );
                        if changed {
                            request_manage_dirty(state);
                        }
                    }
                }
                wl_pointer::Event::Leave { serial, .. } => {
                    let prev_target = seat.borrow().pointer_target;
                    if let canoe::PointerTarget::Titlebar(window_id) = prev_target {
                        if clear_titlebar_state(state, window_id) {
                            request_manage_dirty(state);
                        }
                    }
                    let mut seat = seat.borrow_mut();
                    seat.pointer_enter_serial = serial;
                    seat.pointer_target = canoe::PointerTarget::None;
                    seat.cursor_shape = None;
                }
                wl_pointer::Event::Motion {
                    surface_x,
                    surface_y,
                    ..
                } => {
                    {
                        let mut seat_ref = seat.borrow_mut();
                        seat_ref.last_surface_x = surface_x.round() as i32;
                        seat_ref.last_surface_y = surface_y.round() as i32;
                    }
                    let target = seat.borrow().pointer_target;
                    if target == canoe::PointerTarget::Menu {
                        let changed = state
                            .context
                            .borrow_mut()
                            .update_menu_hover(surface_x.round() as i32, surface_y.round() as i32);
                        if changed {
                            render_window_menu(state, _qh);
                        }
                    } else if let canoe::PointerTarget::Desktop(output_id)
                    | canoe::PointerTarget::MenuShield(output_id) = target
                    {
                        update_pointer_position_from_output(
                            state, &seat, output_id, surface_x, surface_y,
                        );
                        update_menu_hover_from_surface(state, output_id, surface_x, surface_y, _qh);
                        if matches!(target, canoe::PointerTarget::Desktop(_)) {
                            state.context.borrow_mut().update_cursor_for_seat(*seat_id);
                        }
                    } else if let canoe::PointerTarget::Titlebar(window_id) = target {
                        let changed = update_titlebar_hover_from_surface(
                            state, window_id, surface_x, surface_y,
                        );
                        if changed {
                            request_manage_dirty(state);
                        }
                    }
                }
                wl_pointer::Event::Button {
                    button,
                    state: btn_state,
                    ..
                } => {
                    let target = seat.borrow().pointer_target;
                    if matches!(target, canoe::PointerTarget::MenuShield(_)) {
                        return;
                    }
                    match btn_state {
                        wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed) => {
                            if target != canoe::PointerTarget::Menu {
                                let had_menu = {
                                    let mut context = state.context.borrow_mut();
                                    if context.window_menu.is_some() {
                                        context.close_window_menu();
                                        true
                                    } else {
                                        false
                                    }
                                };
                                if had_menu {
                                    seat.borrow_mut().menu_click_button = None;
                                    if matches!(target, canoe::PointerTarget::Desktop(_)) {
                                        seat.borrow_mut().queue_action(binding::Action::ClearFocus);
                                        request_manage_dirty(state);
                                    }
                                    return;
                                }
                            }
                            match target {
                                canoe::PointerTarget::Desktop(output_id) => {
                                    let (px, py) = {
                                        let seat_ref = seat.borrow();
                                        (seat_ref.last_surface_x, seat_ref.last_surface_y)
                                    };
                                    if button == crate::config::button::LEFT {
                                        // Hit-test desktop icons
                                        let hit = {
                                            let context = state.context.borrow();
                                            let scale = context
                                                .outputs
                                                .get(&output_id)
                                                .map(|o| o.borrow().scale)
                                                .unwrap_or(1);
                                            context.outputs.get(&output_id).and_then(|o| {
                                                o.borrow()
                                                    .desktop_surface
                                                    .as_ref()
                                                    .and_then(|d| d.icon_at(px, py, scale))
                                            })
                                        };
                                        if let Some(icon_window_id) = hit {
                                            // Check for double-click to restore
                                            let now = Instant::now();
                                            let is_double = {
                                                let seat_ref = seat.borrow();
                                                seat_ref
                                                    .last_icon_click
                                                    .map(|(last_wid, when)| {
                                                        last_wid == icon_window_id
                                                            && now.duration_since(when)
                                                                <= CLOSE_DOUBLE_CLICK
                                                    })
                                                    .unwrap_or(false)
                                            };
                                            seat.borrow_mut().last_icon_click =
                                                Some((icon_window_id, now));
                                            if is_double {
                                                // Defer through Action::IconActivate so focus_window
                                                // (window-management state) runs inside a manage
                                                // sequence; the preceding single click already
                                                // selected this icon via enter_icon_focus.
                                                let mut seat_mut = seat.borrow_mut();
                                                seat_mut.last_icon_click = None;
                                                seat_mut.queue_action(binding::Action::IconActivate);
                                                drop(seat_mut);
                                                request_manage_dirty(state);
                                            } else {
                                                // Single click: select icon and enter icon focus mode
                                                let seat_id = *seat_id;
                                                state.context.borrow_mut().enter_icon_focus(
                                                    output_id,
                                                    icon_window_id,
                                                    seat_id,
                                                );
                                                render_desktop_surface(state, output_id, _qh);
                                                request_manage_dirty(state);
                                            }
                                        } else {
                                            // Clicked empty space: clear local icon-mode state
                                            // synchronously, then queue the protocol-bound mode
                                            // switch + clear-focus to run inside the next manage
                                            // sequence (this is a wl_pointer event, not part of
                                            // any river manage sequence).
                                            seat.borrow_mut().last_icon_click = None;
                                            {
                                                let mut context = state.context.borrow_mut();
                                                if let Some(focused_output) =
                                                    context.icon_focus_output.take()
                                                {
                                                    if let Some(output) =
                                                        context.outputs.get(&focused_output)
                                                    {
                                                        if let Some(desktop) = output
                                                            .borrow_mut()
                                                            .desktop_surface
                                                            .as_mut()
                                                        {
                                                            desktop.selected_icon = None;
                                                            desktop.dirty = true;
                                                        }
                                                    }
                                                }
                                            }
                                            render_desktop_surface(state, output_id, _qh);
                                            {
                                                let mut seat_mut = seat.borrow_mut();
                                                seat_mut
                                                    .queue_action(binding::Action::ClearFocus);
                                                seat_mut.queue_action(
                                                    binding::Action::SwitchMode {
                                                        mode: crate::config::Mode::Default,
                                                    },
                                                );
                                            }
                                            request_manage_dirty(state);
                                        }
                                    } else if button == crate::config::button::RIGHT {
                                        let mut context = state.context.borrow_mut();
                                        if context.window_menu.is_some() {
                                            context.close_window_menu();
                                        }
                                        drop(context);
                                        open_window_menu(
                                            state,
                                            output_id,
                                            px,
                                            py,
                                            false,
                                            canoe::WindowMenuMode::Pointer,
                                            Some("Windows".to_string()),
                                            _qh,
                                        );
                                        update_menu_hover_from_global(state, *seat_id, _qh);
                                        seat.borrow_mut().menu_click_button = Some(button);
                                        seat.borrow_mut().queue_action(binding::Action::ClearFocus);
                                        request_manage_dirty(state);
                                    } else {
                                        seat.borrow_mut().queue_action(binding::Action::ClearFocus);
                                        request_manage_dirty(state);
                                    }
                                }
                                canoe::PointerTarget::Menu => {
                                    seat.borrow_mut().menu_click_button = Some(button);
                                }
                                canoe::PointerTarget::MenuShield(_) => {}
                                canoe::PointerTarget::Titlebar(window_id) => {
                                    if button == crate::config::button::LEFT {
                                        let (px, py) = {
                                            let seat_ref = seat.borrow();
                                            (seat_ref.last_surface_x, seat_ref.last_surface_y)
                                        };
                                        update_titlebar_hover_from_surface(
                                            state, window_id, px as f64, py as f64,
                                        );
                                        let should_render = {
                                            let context = state.context.borrow();
                                            if let Some(window) = context.windows.get(&window_id) {
                                                let mut w = window.borrow_mut();
                                                w.titlebar_left_down = true;
                                                w.titlebar_pressed = w.titlebar_hovered;
                                                true
                                            } else {
                                                false
                                            }
                                        };
                                        if should_render {
                                            request_manage_dirty(state);
                                        }
                                    }
                                }
                                canoe::PointerTarget::None => {}
                            }
                        }
                        wayland_client::WEnum::Value(wl_pointer::ButtonState::Released) => {
                            if let canoe::PointerTarget::Titlebar(window_id) = target {
                                if button == crate::config::button::LEFT {
                                    let (px, py) = {
                                        let seat_ref = seat.borrow();
                                        (seat_ref.last_surface_x, seat_ref.last_surface_y)
                                    };
                                    update_titlebar_hover_from_surface(
                                        state, window_id, px as f64, py as f64,
                                    );
                                    let (action, should_render) = {
                                        let context = state.context.borrow();
                                        let Some(window) = context.windows.get(&window_id) else {
                                            return;
                                        };
                                        let mut w = window.borrow_mut();
                                        w.titlebar_left_down = false;
                                        let hovered = w.titlebar_hovered;
                                        let pressed = w.titlebar_pressed.take();
                                        let action = if pressed.is_some() && pressed == hovered {
                                            pressed
                                        } else {
                                            None
                                        };
                                        (action, true)
                                    };
                                    if should_render {
                                        request_manage_dirty(state);
                                    }

                                    match action {
                                        Some(canoe::titlebar::TitlebarButton::Close) => {
                                            let now = Instant::now();
                                            let should_close = {
                                                let mut seat_ref = seat.borrow_mut();
                                                let is_double = seat_ref
                                                    .last_close_click
                                                    .map(|(last_window, when)| {
                                                        last_window == window_id
                                                            && now.duration_since(when)
                                                                <= CLOSE_DOUBLE_CLICK
                                                    })
                                                    .unwrap_or(false);
                                                seat_ref.last_close_click = Some((window_id, now));
                                                is_double
                                            };
                                            if should_close {
                                                if let Some(window) =
                                                    state.context.borrow().windows.get(&window_id)
                                                {
                                                    window
                                                        .borrow_mut()
                                                        .queue_event(canoe::WindowEvent::Close);
                                                    request_manage_dirty(state);
                                                }
                                            }
                                        }
                                        Some(canoe::titlebar::TitlebarButton::Hide) => {
                                            seat.borrow_mut().last_close_click = None;
                                            if let Some(window) =
                                                state.context.borrow().windows.get(&window_id)
                                            {
                                                window
                                                    .borrow_mut()
                                                    .queue_event(canoe::WindowEvent::Minimize);
                                                request_manage_dirty(state);
                                            }
                                        }
                                        Some(canoe::titlebar::TitlebarButton::Maximize) => {
                                            seat.borrow_mut().last_close_click = None;
                                            if let Some(window) =
                                                state.context.borrow().windows.get(&window_id)
                                            {
                                                if window.borrow().maximized {
                                                    window.borrow_mut().queue_event(
                                                        canoe::WindowEvent::Unmaximize,
                                                    );
                                                } else {
                                                    window
                                                        .borrow_mut()
                                                        .queue_event(canoe::WindowEvent::Maximize);
                                                }
                                                request_manage_dirty(state);
                                            }
                                        }
                                        None => {
                                            seat.borrow_mut().last_close_click = None;
                                        }
                                    }
                                }
                                return;
                            }
                            let (activate, close_menu) = {
                                let seat_ref = seat.borrow();
                                if seat_ref.menu_click_button != Some(button) {
                                    (false, false)
                                } else {
                                    let context = state.context.borrow();
                                    let hovered = context
                                        .window_menu
                                        .as_ref()
                                        .and_then(|menu| menu.hovered)
                                        .is_some();
                                    if context.window_menu_mode
                                        == Some(canoe::WindowMenuMode::Pointer)
                                    {
                                        if hovered {
                                            (true, false)
                                        } else {
                                            (false, false)
                                        }
                                    } else {
                                        (false, false)
                                    }
                                }
                            };
                            seat.borrow_mut().menu_click_button = None;
                            if activate {
                                request_manage_dirty(state);
                                seat.borrow_mut()
                                    .queue_action(binding::Action::ActivateMenuHovered);
                                return;
                            }
                            if close_menu {
                                state.context.borrow_mut().close_window_menu();
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeManagerV1,
        _event: wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Cursor shape manager has no events.
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeDeviceV1,
        _event: wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Cursor shape device has no events.
    }
}

// Titlebar surface user data
struct TitlebarSurfaceData {
    #[allow(dead_code)]
    window_id: canoe::WindowId,
}

impl Dispatch<wl_surface::WlSurface, TitlebarSurfaceData> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &TitlebarSurfaceData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let (output_name, is_enter) = match &_event {
            wl_surface::Event::Enter { output } => {
                let name = _state
                    .globals
                    .wl_outputs
                    .iter()
                    .find_map(|(name, wl_output)| {
                        if wl_output == output {
                            Some(*name)
                        } else {
                            None
                        }
                    });
                (name, true)
            }
            wl_surface::Event::Leave { output } => {
                let name = _state
                    .globals
                    .wl_outputs
                    .iter()
                    .find_map(|(name, wl_output)| {
                        if wl_output == output {
                            Some(*name)
                        } else {
                            None
                        }
                    });
                (name, false)
            }
            _ => (None, false),
        };
        let Some(output_name) = output_name else {
            return;
        };

        let window = {
            let context = _state.context.borrow();
            context.windows.get(&_data.window_id).cloned()
        };
        let Some(window) = window else {
            return;
        };

        let mut w = window.borrow_mut();
        let Some(ref mut titlebar) = w.titlebar else {
            return;
        };

        if is_enter {
            if !titlebar.output_names.contains(&output_name) {
                titlebar.output_names.push(output_name);
            }
        } else {
            titlebar.output_names.retain(|name| *name != output_name);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Surface events for menu/desktop are not used.
    }
}

impl Dispatch<wl_output::WlOutput, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_output::WlOutput,
        _event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = _event {
            let scale = factor.max(1);
            let output_name = _state.globals.wl_outputs.iter().find_map(|(name, output)| {
                if output == _proxy {
                    Some(*name)
                } else {
                    None
                }
            });
            if let Some(name) = output_name {
                _state.globals.wl_output_scales.insert(name, scale);
            }
            let outputs = {
                let context = _state.context.borrow();
                context
                    .outputs
                    .values()
                    .filter(|output| {
                        let out = output.borrow();
                        if let Some(name) = output_name {
                            out.wl_output_name == name
                        } else {
                            out.wl_output.as_ref().map(|o| o == _proxy).unwrap_or(false)
                        }
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            };
            for output in outputs {
                output.borrow_mut().scale = scale;
            }
        }
    }
}

impl Dispatch<RiverDecorationV1, canoe::WindowId> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &RiverDecorationV1,
        _event: river_window_management_v1::client::river_decoration_v1::Event,
        _data: &canoe::WindowId,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // river_decoration_v1 has no events
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to Wayland display
    let conn = Connection::connect_to_env()?;
    let display = conn.display();

    // Create event queue
    let mut event_queue: EventQueue<AppState> = conn.new_event_queue();
    let qh = event_queue.handle();

    // Create app state
    let context = Rc::new(RefCell::new(Context::new()));
    let mut state = AppState {
        context: context.clone(),
        globals: Globals::default(),
    };

    // Get registry and collect globals
    let _registry = display.get_registry(&qh, ());

    // Roundtrip to receive globals
    event_queue.roundtrip(&mut state)?;

    // Check required globals
    if state.globals.rwm.is_none() {
        return Err("River window manager protocol not available".into());
    }

    // Set up signal handling via a self-pipe so the poll loop can wake on signals.
    let (signal_read_fd, signal_write_fd) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?;
    SIGNAL_WRITE_FD.store(signal_write_fd.as_raw_fd(), Ordering::Relaxed);
    // The write end must outlive the signal handler that references its fd.
    std::mem::forget(signal_write_fd);

    let action = SigAction::new(
        SigHandler::Handler(forward_signal),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for sig in [
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGQUIT,
        Signal::SIGCHLD,
    ] {
        unsafe { sigaction(sig, &action)? };
    }

    // Run startup commands
    for cmd in &state.context.borrow().config.startup_cmds.clone() {
        state.context.borrow().spawn(cmd);
    }

    // Main event loop
    while state.context.borrow().running {
        // Flush outgoing requests
        conn.flush()?;

        // Prepare poll file descriptors
        let wayland_fd = conn.as_fd();

        let mut poll_fds = [
            PollFd::new(wayland_fd, PollFlags::POLLIN),
            PollFd::new(signal_read_fd.as_fd(), PollFlags::POLLIN),
        ];

        // Poll for events (None = infinite timeout)
        match poll(&mut poll_fds, None::<u16>) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        }

        // Handle Wayland events
        if poll_fds[0]
            .revents()
            .map(|r| r.contains(PollFlags::POLLIN))
            .unwrap_or(false)
        {
            event_queue.dispatch_pending(&mut state)?;

            // Read and dispatch new events
            if let Some(guard) = conn.prepare_read() {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
            event_queue.dispatch_pending(&mut state)?;
        }

        // Handle signals
        if poll_fds[1]
            .revents()
            .map(|r| r.contains(PollFlags::POLLIN))
            .unwrap_or(false)
        {
            let mut buf = [0u8; 32];
            loop {
                match nix::unistd::read(signal_read_fd.as_raw_fd(), &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for &byte in &buf[..n] {
                            match Signal::try_from(byte as i32) {
                                Ok(Signal::SIGINT) | Ok(Signal::SIGTERM) | Ok(Signal::SIGQUIT) => {
                                    state.context.borrow_mut().running = false;
                                }
                                Ok(Signal::SIGCHLD) => loop {
                                    match nix::sys::wait::waitpid(
                                        None,
                                        Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                                    ) {
                                        Ok(nix::sys::wait::WaitStatus::StillAlive) => break,
                                        Ok(_) => continue,
                                        Err(_) => break,
                                    }
                                },
                                _ => {}
                            }
                        }
                        if n < buf.len() {
                            break;
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => break,
                    Err(_) => break,
                }
            }
        }
    }

    Ok(())
}
