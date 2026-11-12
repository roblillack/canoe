//! The window manager's side of the compositor: what used to be canoe's
//! River client glue. Events about windows, outputs and the seat are passed
//! to the window manager the way River reported them, its manage and render
//! sequences are run, and its own surfaces (desktop, menus, picker) are
//! created and drawn.

#![allow(clippy::too_many_arguments)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use crate::binding;
use crate::canoe;
use crate::canoe::scale::FractionalSurface;
use crate::canoe::surface::{Anchor, KeyboardInteractivity, Layer, LayerSurface, Surface};
use crate::config;
use crate::ipc;

use super::State;

pub(super) const CLOSE_DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// Where a window's content sits on the output it's drawn on, in logical
/// pixels from the output's corner: what the compositor rounds the window's
/// and its decorations' edges from. A window spanning outputs lines up on
/// the one it belongs to.
pub(super) fn window_origin(
    window: &canoe::window::Window,
    output: Option<&canoe::Output>,
) -> (i32, i32) {
    match output {
        Some(output) => (window.x - output.x, window.y - output.y),
        None => (window.x, window.y),
    }
}

pub(super) fn render_window_menu(state: &mut State) {
    let mut context = state.context.borrow_mut();
    let scale = context.window_menu.as_ref().and_then(|menu| {
        let output = context.outputs.get(&menu.output_id)?;
        let output = output.borrow();
        let device = menu
            .fractional
            .device_scale(output.preferred_scale, output.scale);
        Some(canoe::scale::Scale::new(device))
    });
    let scale = scale.unwrap_or_default();
    if let Some(menu) = context.window_menu.as_mut() {
        if !menu.configured {
            return;
        }
        menu.ensure_buffer(scale);
        menu.render();
        menu.update_input_region();
        menu.commit();
    }
}

/// For `AltTab` menus, the focused window is preselected and then moved by
/// `initial_step`, if given. Other modes ignore `initial_step`.
#[allow(clippy::too_many_arguments)]
pub(super) fn open_window_menu_with_items(
    state: &mut State,
    output_id: canoe::OutputId,
    pointer: (i32, i32),
    centered: bool,
    mode: canoe::WindowMenuMode,
    items: Vec<canoe::MenuItem>,
    header_title: Option<String>,
    initial_step: Option<binding::Direction>,
) {
    let (output_info, focused_window, menu_theme) = {
        let context = state.context.borrow();
        let focused = context.focused_window;
        let menu_theme = canoe::MenuTheme::from_ui(&context.config.ui);
        let info = context.outputs.get(&output_id).map(|output| {
            let out = output.borrow();
            (out.width, out.height, out.render_scale())
        });
        (info, focused, menu_theme)
    };

    let Some((ow, oh, scale)) = output_info else {
        return;
    };

    if items.is_empty() {
        return;
    }

    let surface = Surface::new();
    let layer_surface = LayerSurface::new(
        &surface,
        Some(output_id),
        Layer::Overlay,
        "canoe-window-menu",
    );

    let fractional = FractionalSurface::new();
    let mut menu = canoe::WindowMenu::new(
        surface,
        layer_surface,
        fractional,
        output_id,
        items,
        header_title,
        pointer.0,
        pointer.1,
        menu_theme,
        scale,
    );
    let mut initial_preview = None;
    if mode == canoe::WindowMenuMode::AltTab {
        menu.select_window(focused_window);
        if let Some(direction) = initial_step {
            menu.select_step(direction);
        }
        initial_preview = menu
            .hovered
            .and_then(|idx| menu.items.get(idx).map(|item| item.window_id));
    }
    // Position the surface so the menu title row sits under the cursor:
    // horizontally one half-row-height in from the menu's left edge,
    // vertically at the title row's vertical center.
    let (anchor_x, anchor_y) = menu.pointer_anchor();
    let mut local_x = (pointer.0 - anchor_x).max(0);
    let mut local_y = (pointer.1 - anchor_y).max(0);
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

    menu.layer_surface.set_anchor(Anchor::Top | Anchor::Left);
    menu.layer_surface.set_margin(local_y, 0, 0, local_x);
    menu.layer_surface
        .set_size(menu.width as u32, menu.height as u32);
    menu.layer_surface
        .set_keyboard_interactivity(KeyboardInteractivity::None);
    menu.layer_surface.set_exclusive_zone(-1);
    menu.surface.commit();

    {
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
    configure_canoe_layers(state, output_id);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn open_window_menu(
    state: &mut State,
    output_id: canoe::OutputId,
    pointer_x: i32,
    pointer_y: i32,
    centered: bool,
    mode: canoe::WindowMenuMode,
    header_title: Option<String>,
    initial_step: Option<binding::Direction>,
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
        initial_step,
    );
}

pub(super) fn ensure_window_menu_shield(state: &mut State, output_id: canoe::OutputId) {
    let output = {
        let context = state.context.borrow();
        context.outputs.get(&output_id).cloned()
    };
    let Some(_output) = output else {
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
    let surface = Surface::new();
    let layer_surface = LayerSurface::new(
        &surface,
        Some(output_id),
        Layer::Overlay,
        "canoe-window-menu-shield",
    );

    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.set_size(0, 0);
    surface.commit();

    state.context.borrow_mut().window_menu_shield =
        Some(canoe::ShieldSurface::new(surface, layer_surface, output_id));
    configure_canoe_layers(state, output_id);
}

/// Rebuild every seat's key bindings from the freshly reloaded config, so
/// `[hotkeys]` and `main_modifier` edits take effect (see
/// [`canoe::Context::reload_config`]).
pub(super) fn reregister_xkb_bindings(state: &State) {
    let context = state.context.borrow();
    for seat in context.seats.values() {
        context.rebuild_xkb_bindings(&mut seat.borrow_mut());
    }
}

pub(super) fn update_menu_hover_from_global(state: &mut State, seat_id: canoe::SeatId) {
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
        render_window_menu(state);
    }
}

pub(super) fn update_menu_hover_from_surface(
    state: &mut State,
    output_id: canoe::OutputId,
    surface_x: f64,
    surface_y: f64,
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
        render_window_menu(state);
    }
}

pub(super) fn update_pointer_position_from_output(
    state: &State,
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

pub(super) fn update_titlebar_hover_from_surface(
    state: &mut State,
    window_id: canoe::WindowId,
    surface_x: f64,
    surface_y: f64,
) -> bool {
    let local_x = surface_x.round() as i32;
    let local_y = surface_y.round() as i32;
    let context = state.context.borrow();
    let Some(window) = context.windows.get(&window_id) else {
        return false;
    };
    let mut w = window.borrow_mut();
    let ui = &context.config.ui;
    let titlebar_height = canoe::titlebar::titlebar_height(ui);
    let border_width = canoe::titlebar::border_width(ui, w.frame_style());
    let show_min = w.has_minimize_button();
    let show_max = w.has_maximize_button();
    let new_hover = canoe::titlebar::button_at(
        w.width,
        border_width,
        local_x,
        local_y,
        titlebar_height,
        show_min,
        show_max,
    );
    if w.titlebar_hovered == new_hover {
        return false;
    }
    w.titlebar_hovered = new_hover;
    true
}

pub(super) fn update_titlebar_hover_from_global(
    state: &mut State,
    window_id: canoe::WindowId,
    pointer_x: i32,
    pointer_y: i32,
) -> bool {
    let (win_x, win_y, win_w, swallow_top, frame_style) = {
        let context = state.context.borrow();
        let Some(window) = context.windows.get(&window_id) else {
            return false;
        };
        let w = window.borrow();
        (w.x, w.y, w.width, w.swallow_top, w.frame_style())
    };

    let (border_width, titlebar_height) = {
        let ui = &state.context.borrow().config.ui;
        (
            canoe::titlebar::border_width(ui, frame_style),
            canoe::titlebar::titlebar_height(ui),
        )
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
    let show_min = w.has_minimize_button();
    let show_max = w.has_maximize_button();
    let new_hover = canoe::titlebar::button_at(
        win_w,
        border_width,
        local_x,
        local_y,
        titlebar_height,
        show_min,
        show_max,
    );
    if w.titlebar_hovered == new_hover {
        return false;
    }
    w.titlebar_hovered = new_hover;
    true
}

pub(super) fn clear_titlebar_state(state: &mut State, window_id: canoe::WindowId) -> bool {
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

pub(super) fn menu_matches_app_id(
    context: &canoe::Context,
    menu: &canoe::WindowMenu,
    app_id: &str,
) -> bool {
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

pub(super) fn handle_window_menu_cycle(state: &mut State, direction: binding::Direction) {
    let mut should_render = false;
    let mut open_new = false;
    let mut ensure_shield = None;
    let mut preview_window = None;

    {
        let mut context = state.context.borrow_mut();
        let is_alt_tab = context.window_menu_mode == Some(canoe::WindowMenuMode::AltTab);
        if let Some(menu) = context.window_menu.as_mut() {
            if is_alt_tab {
                if menu.select_step(direction) {
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
        render_window_menu(state);
    }

    if let Some(output_id) = ensure_shield {
        ensure_window_menu_shield(state, output_id);
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
        Some(direction),
    );
    ensure_window_menu_shield(state, output_id);
}

pub(super) fn handle_window_menu_cycle_app(state: &mut State) {
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
        render_window_menu(state);
    }

    if let Some(output_id) = ensure_shield {
        ensure_window_menu_shield(state, output_id);
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
        Some(binding::Direction::Forward),
    );
    ensure_window_menu_shield(state, output_id);
}

pub(super) fn handle_window_menu_commit(state: &mut State, seat_id: canoe::SeatId) {
    let Some(seat) = state.context.borrow().seats.get(&seat_id).cloned() else {
        return;
    };
    seat.borrow_mut()
        .queue_action(binding::Action::WindowMenuCommit);
}

pub(super) fn ensure_desktop_surfaces(state: &mut State, output_id: canoe::OutputId) {
    ensure_desktop_background(state, output_id);
    ensure_desktop_surface(state, output_id);
}

/// Solid-fill surface on the background layer, *below* wallpaper clients:
/// River stacks same-layer surfaces in creation order, and the startup
/// commands are launched only after this exists.
///
/// The surface is kept even when the fill is fully transparent. Creating it
/// later, say after a reload that makes the fill visible, would put it on
/// top of a wallpaper that is already running.
pub(super) fn ensure_desktop_background(state: &mut State, output_id: canoe::OutputId) {
    let output = {
        let context = state.context.borrow();
        context.outputs.get(&output_id).cloned()
    };
    let Some(output) = output else {
        return;
    };

    if output.borrow().desktop_background.is_some() {
        return;
    }
    let surface = Surface::new();
    let layer_surface = LayerSurface::new(
        &surface,
        Some(output_id),
        Layer::Background,
        "canoe-desktop-background",
    );

    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.set_size(0, 0);
    surface.commit();
    output.borrow_mut().desktop_background = Some(canoe::DesktopSurface::new(
        surface,
        layer_surface,
        FractionalSurface::new(),
        output_id,
    ));
}

pub(super) fn ensure_desktop_surface(state: &mut State, output_id: canoe::OutputId) {
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
    let surface = Surface::new();
    let layer_surface =
        LayerSurface::new(&surface, Some(output_id), Layer::Bottom, "canoe-desktop");

    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.set_size(0, 0);
    surface.commit();

    let fractional = FractionalSurface::new();
    output.borrow_mut().desktop_surface = Some(canoe::DesktopSurface::new(
        surface,
        layer_surface,
        fractional,
        output_id,
    ));
}

/// Repaint the solid-fill background surface when dirty. Purely visual:
/// the input region is always empty so every event reaches clients below.
pub(super) fn render_desktop_background(state: &mut State, output_id: canoe::OutputId) {
    let (bg_color, dirty) = {
        let context = state.context.borrow();
        let dirty = context
            .outputs
            .get(&output_id)
            .and_then(|o| o.borrow().desktop_background.as_ref().map(|d| d.dirty))
            .unwrap_or(false);
        (context.config.ui.desktop_background, dirty)
    };
    if !dirty {
        return;
    }

    let output = {
        let context = state.context.borrow();
        context.outputs.get(&output_id).cloned()
    };
    let Some(output) = output else {
        return;
    };

    let mut out = output.borrow_mut();
    let Some(background) = out.desktop_background.as_mut() else {
        return;
    };
    if !background.configured {
        return;
    }

    background.ensure_solid_buffer();
    background.render(bg_color);
    background.update_input_region(None);
    background.commit();
    background.dirty = false;
}

pub(super) fn render_desktop_surface(state: &mut State, output_id: canoe::OutputId) {
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

    let (
        icons,
        icon_theme,
        font_name,
        font_size,
        label_font_name,
        label_font_size,
        scale,
        desktop_menu,
    ) = {
        let mut context = state.context.borrow_mut();
        let ui = &context.config.ui;
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
        let desktop_menu = ui.desktop_menu;
        let scale = context
            .outputs
            .get(&output_id)
            .map(|o| o.borrow().desktop_scale())
            .unwrap_or_default();
        let icons = if icons_enabled {
            context.collect_minimized_icons(output_id)
        } else {
            Vec::new()
        };
        (
            icons,
            icon_theme,
            font_name,
            font_size,
            label_font_name,
            label_font_size,
            scale,
            desktop_menu,
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
    let usable_area = out.usable_area();
    let output_origin = (out.x, out.y);
    let Some(desktop) = out.desktop_surface.as_mut() else {
        return;
    };
    if !desktop.configured {
        return;
    }
    let desktop_area = desktop_menu
        .then(|| {
            canoe::desktop_input_area(usable_area, output_origin, (desktop.width, desktop.height))
        })
        .flatten();

    if icons.is_empty() {
        // Nothing to draw: a single transparent pixel covers the output.
        desktop.ensure_solid_buffer();
    } else {
        desktop.ensure_buffer(scale);
    }
    desktop.render_with_icons(
        0, // transparent fill: wallpaper/fill on the background layer shows through
        &icons,
        &icon_theme,
        font_name.as_deref(),
        font_size,
        Some(label_font_name.as_str()),
        label_font_size,
    );
    desktop.update_input_region(desktop_area);
    desktop.commit();
    desktop.dirty = false;
}

pub(super) fn render_all_desktop_surfaces(state: &mut State) {
    let output_ids: Vec<canoe::OutputId> = {
        let context = state.context.borrow();
        context.outputs.keys().copied().collect()
    };
    for output_id in output_ids {
        render_desktop_background(state, output_id);
        render_desktop_surface(state, output_id);
    }
}

/// Answer a `canoe --share-picker` request without asking the user.
pub(super) fn decline_share_request(
    state: &mut State,
    request: canoe::share_picker::ShareRequestId,
) {
    if let Some(server) = state.share_server.as_mut() {
        server.reply(request, None);
    }
}

/// Open the screen-sharing picker for a `canoe --share-picker` request: an
/// overlay on every output that highlights the window or screen under the
/// pointer.
pub(super) fn start_share_picker(state: &mut State, request: ipc::ShareRequest) {
    let (seat_id, candidates) = {
        let context = state.context.borrow();
        // One question at a time; and nothing is shared from a locked screen.
        if context.share_picker.is_some() || context.session_locked {
            drop(context);
            decline_share_request(state, request.id);
            return;
        }
        let Some(seat_id) = context.current_seat else {
            drop(context);
            decline_share_request(state, request.id);
            return;
        };
        (seat_id, context.share_candidates(&request.lines))
    };
    // Chrome asks again right after the user picked something; give it the
    // same answer.
    let repeated = state.context.borrow_mut().repeat_share_choice(&candidates);
    if let Some(candidate) = repeated {
        if let Some(server) = state.share_server.as_mut() {
            server.reply(request.id, Some(&candidate.reply));
        }
        return;
    }
    if candidates.is_empty() {
        decline_share_request(state, request.id);
        return;
    }

    let outputs: Vec<canoe::OutputId> = {
        let mut context = state.context.borrow_mut();
        if context.window_menu.is_some() {
            context.close_window_menu();
        }
        // Leave desktop icon selection; the picker takes the keyboard.
        if let Some(output_id) = context.icon_focus_output.take() {
            if let Some(output) = context.outputs.get(&output_id) {
                if let Some(desktop) = output.borrow_mut().desktop_surface.as_mut() {
                    desktop.selected_icon = None;
                    desktop.dirty = true;
                }
            }
        }
        context.outputs.iter().map(|(&id, _)| id).collect()
    };

    let theme = {
        let context = state.context.borrow();
        canoe::share_picker::PickerTheme::from_ui(&context.config.ui)
    };
    let mut picker = canoe::share_picker::SharePicker::new(request.id, seat_id, candidates, theme);
    for output_id in outputs {
        let surface = Surface::new();
        let layer_surface = LayerSurface::new(
            &surface,
            Some(output_id),
            Layer::Overlay,
            "canoe-share-picker",
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer_surface.set_size(0, 0);
        surface.commit();
        let fractional = FractionalSurface::new();
        picker
            .surfaces
            .push(canoe::share_picker::PickerSurface::new(
                surface,
                layer_surface,
                fractional,
                output_id,
            ));
    }

    let mut context = state.context.borrow_mut();
    context.share_picker = Some(picker);
    if let Some(seat) = context.seats.get(&seat_id).cloned() {
        let (px, py) = {
            let seat = seat.borrow();
            (seat.pointer_x, seat.pointer_y)
        };
        let hovered = context.share_candidate_at(px, py);
        if let Some(picker) = context.share_picker.as_mut() {
            picker.hovered = hovered;
        }
        // Binding modes may only change in a manage sequence.
        seat.borrow_mut().queue_action(binding::Action::SwitchMode {
            mode: config::Mode::SharePicker,
        });
    }
    drop(context);
    let output_ids: Vec<canoe::OutputId> = state.context.borrow().outputs.keys().copied().collect();
    for output_id in output_ids {
        configure_canoe_layers(state, output_id);
    }
    state.schedule_wm();
}

/// Redraw the picker overlay on every output.
pub(super) fn render_share_picker(state: &mut State) {
    let mut context = state.context.borrow_mut();
    let highlight = context.share_highlight();
    let context = &mut *context;
    let Some(picker) = context.share_picker.as_mut() else {
        return;
    };
    let banner = picker.banner();
    let canoe::share_picker::SharePicker {
        surfaces, theme, ..
    } = picker;
    for surface in surfaces.iter_mut() {
        let Some(output) = context.outputs.get(&surface.output_id) else {
            continue;
        };
        let out = output.borrow();
        let scale = canoe::scale::Scale::new(
            surface
                .fractional
                .device_scale(out.preferred_scale, out.scale),
        );
        let scene = canoe::share_picker::PickerScene {
            highlight: highlight
                .as_ref()
                .map(|&((x, y, w, h), _)| (x - out.x, y - out.y, w, h)),
            label: highlight.as_ref().map(|(_, label)| label.as_str()),
            banner,
            theme,
        };
        surface.render(&scene, scale);
    }
}

/// Highlight the picker target under the seat's pointer.
pub(super) fn update_share_picker_hover(state: &mut State, seat: &Rc<RefCell<canoe::Seat>>) {
    let (px, py) = {
        let seat = seat.borrow();
        (seat.pointer_x, seat.pointer_y)
    };
    let changed = {
        let mut context = state.context.borrow_mut();
        let hovered = context.share_candidate_at(px, py);
        match context.share_picker.as_mut() {
            Some(picker) if picker.hovered != hovered => {
                picker.hovered = hovered;
                true
            }
            _ => false,
        }
    };
    if changed {
        render_share_picker(state);
    }
}

/// Close the picker. With `confirm`, share the highlighted target, if any;
/// otherwise tell the portal the user declined. `restore_mode` switches the
/// seat back to the default binding mode.
pub(super) fn finish_share_picker(state: &mut State, confirm: bool, restore_mode: bool) {
    let picker = {
        let mut context = state.context.borrow_mut();
        let Some(picker) = context.share_picker.take() else {
            return;
        };
        let choice = picker
            .hovered
            .and_then(|idx| picker.candidates.get(idx))
            .map(|candidate| candidate.reply.clone());
        if confirm && choice.is_none() {
            // Nothing highlighted yet: keep asking.
            context.share_picker = Some(picker);
            return;
        }
        (picker, if confirm { choice } else { None })
    };
    let (picker, choice) = picker;
    {
        let mut context = state.context.borrow_mut();
        match picker.hovered.and_then(|idx| picker.candidates.get(idx)) {
            Some(candidate) if choice.is_some() => context.record_share_choice(candidate.target),
            _ => context.share_last_choice = None,
        }
    }
    if let Some(server) = state.share_server.as_mut() {
        server.reply(picker.request, choice.as_deref());
    }
    let seat_id = picker.seat_id;
    // Destroys the overlay surfaces.
    drop(picker);

    let context = state.context.borrow();
    for seat in context.seats.values() {
        let mut seat = seat.borrow_mut();
        if matches!(seat.pointer_target, canoe::PointerTarget::SharePicker(_)) {
            seat.pointer_target = canoe::PointerTarget::None;
        }
    }
    if restore_mode {
        if let Some(seat) = context.seats.get(&seat_id) {
            seat.borrow_mut().queue_action(binding::Action::SwitchMode {
                mode: config::Mode::Default,
            });
        }
        drop(context);
        state.schedule_wm();
    }
}
impl State {
    /// The render sequence: the window manager places windows and draws
    /// their decorations.
    pub(super) fn render_sequence(&mut self) {
        let state = self;
        #[cfg(feature = "debug-logging")]
        let total_t0 = Instant::now();
        #[cfg(feature = "debug-logging")]
        let handle_start_t0 = Instant::now();
        state.context.borrow_mut().handle_render_start();
        #[cfg(feature = "debug-logging")]
        let handle_start_us = handle_start_t0.elapsed().as_micros();

        #[cfg(feature = "debug-logging")]
        let render_t0 = Instant::now();
        #[cfg(feature = "debug-logging")]
        let mut titlebars_rendered = 0u32;
        #[cfg(feature = "debug-logging")]
        let mut shadows_rendered = 0u32;
        #[cfg(feature = "debug-logging")]
        let mut shadow_render_us = 0u128;
        #[cfg(feature = "debug-logging")]
        let mut titlebar_render_us = 0u128;
        #[cfg(feature = "debug-logging")]
        let mut windows_iterated = 0u32;
        #[cfg(feature = "debug-logging")]
        let mut input_region_us = 0u128;

        // Update shadows and titlebars
        {
            let context = state.context.borrow();
            let focused_window = context.focused_window;
            let ui = &context.config.ui;

            for window in context.windows.values() {
                let mut w = window.borrow_mut();

                if w.hidden {
                    continue;
                }

                let width = w.width;
                let height = w.height;
                if width <= 0 || height <= 0 {
                    continue;
                }

                #[cfg(feature = "debug-logging")]
                {
                    windows_iterated += 1;
                }

                let is_fullscreen = !matches!(w.fullscreen, canoe::window::FullscreenState::None);
                let is_focused = focused_window == Some(w.id);
                let shadows_on = ui.shadows_enabled && !is_fullscreen && !w.maximized;
                let shadow_size = if !shadows_on {
                    0
                } else if is_focused {
                    ui.shadows_active_size.max(0)
                } else {
                    ui.shadows_inactive_size.max(0)
                };
                // Size the buffer for the larger of the active/inactive
                // sizes so it stays constant across focus changes (no
                // realloc); a smaller band is drawn into it when inactive.
                let buffer_shadow_size = if !shadows_on {
                    0
                } else {
                    ui.shadows_active_size
                        .max(0)
                        .max(ui.shadows_inactive_size.max(0))
                };
                let shadow_color = ui.shadows_color;
                let use_ssd = w.decoration != Some(config::WindowDecoration::Csd);
                let border_width = if use_ssd {
                    canoe::titlebar::border_width(ui, w.frame_style())
                } else {
                    0
                };
                let titlebar_height = if use_ssd {
                    canoe::titlebar::titlebar_height(ui)
                } else {
                    0
                };
                let swallow_top = if use_ssd { w.swallow_top } else { 0 };
                let content_height = if use_ssd {
                    (height - swallow_top).max(1)
                } else {
                    height.max(1)
                };
                let frame_width = (width + border_width * 2).max(1);
                let frame_height = (content_height + titlebar_height + border_width * 2).max(1);
                let fit = w.frame_fit();

                let window_output = w.output.as_ref().and_then(|o| o.upgrade());
                let window_output = window_output.as_ref().map(|o| o.borrow());
                let (window_x, window_y) = window_origin(&w, window_output.as_deref());

                // The rect sits `buffer_shadow_size` in from the
                // buffer edges, so position the surface by that inset
                // (constant across focus). The drop-shadow shift still
                // tracks the actual band size.
                let offset_x = if use_ssd {
                    -border_width - buffer_shadow_size
                } else {
                    -buffer_shadow_size
                };
                let shadow_shift = shadow_size / 2;
                let offset_y = if use_ssd {
                    -border_width - titlebar_height + swallow_top - buffer_shadow_size
                        + shadow_shift
                } else {
                    -buffer_shadow_size + shadow_shift
                };

                if let Some(ref mut shadow) = w.shadow {
                    let integer = window_output.as_ref().map(|o| o.scale).unwrap_or(1);
                    let device = shadow.fractional.device_scale(
                        window_output.as_ref().and_then(|o| o.preferred_scale),
                        integer,
                    );
                    let scale = canoe::scale::Scale::new(device);
                    shadow.ensure_buffer(
                        frame_width,
                        frame_height,
                        buffer_shadow_size,
                        scale,
                        canoe::scale::Origin::new(window_x + offset_x, window_y + offset_y),
                    );
                    {
                        #[cfg(feature = "debug-logging")]
                        let ir_t0 = Instant::now();
                        shadow.update_input_region();
                        #[cfg(feature = "debug-logging")]
                        {
                            input_region_us += ir_t0.elapsed().as_micros();
                        }
                    }
                    #[cfg(feature = "debug-logging")]
                    let sh_t0 = Instant::now();
                    // Cast the shadow from the device pixels the frame
                    // actually covers, its top dropped by the shift.
                    let frame = canoe::titlebar::DeviceFrame::new(
                        &scale,
                        fit,
                        canoe::scale::Origin::new(
                            window_x - border_width,
                            window_y - border_width - titlebar_height + swallow_top,
                        ),
                        border_width,
                        titlebar_height,
                        width,
                        content_height,
                    );
                    let drop = scale.px(shadow_shift).min(frame.outer.height - 1).max(0);
                    let cast = canoe::titlebar::Rect {
                        y: frame.outer.y + drop,
                        height: frame.outer.height - drop,
                        ..frame.outer
                    };
                    let did_render = shadow.render(cast, shadow_size, shadow_color);
                    #[cfg(feature = "debug-logging")]
                    {
                        shadow_render_us += sh_t0.elapsed().as_micros();
                    }
                    shadow.set_offset(offset_x, offset_y);
                    if did_render && shadow.is_ready() {
                        shadow.sync_next_commit();
                        shadow.commit();
                        #[cfg(feature = "debug-logging")]
                        {
                            shadows_rendered += 1;
                        }
                    }
                }
            }

            for (&window_id, window) in &context.windows {
                let mut w = window.borrow_mut();

                let hide_titlebar = w.hidden || w.decoration == Some(config::WindowDecoration::Csd);
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
                let show_minimize = w.has_minimize_button();
                let show_maximize = w.has_maximize_button();
                let frame_style = w.frame_style();
                let height = w.height;
                let hovered_button = w.titlebar_hovered;
                let titlebar_left_down = w.titlebar_left_down;
                let swallow_top = w.swallow_top;
                let frame_fit = w.frame_fit();

                // Update titlebar if it exists and window has valid dimensions
                let window_output = w.output.as_ref().and_then(|o| o.upgrade());
                let window_output = window_output.as_ref().map(|o| o.borrow());
                let (window_x, window_y) = window_origin(&w, window_output.as_deref());
                let border_width = canoe::titlebar::border_width(ui, frame_style);
                let titlebar_height = canoe::titlebar::titlebar_height(ui);
                let (offset_x, offset_y) =
                    (-border_width, -border_width - titlebar_height + swallow_top);
                if let Some(ref mut titlebar) = w.titlebar {
                    if width > 0 && height > 0 {
                        let integer = window_output.as_ref().map(|o| o.scale).unwrap_or(1);
                        let device = titlebar.fractional.device_scale(
                            window_output.as_ref().and_then(|o| o.preferred_scale),
                            integer,
                        );
                        let scale = canoe::scale::Scale::new(device);
                        let content_height = (height - swallow_top).max(1);
                        // Ensure buffer is allocated
                        titlebar.ensure_buffer(
                            width,
                            content_height,
                            scale,
                            canoe::scale::Origin::new(window_x + offset_x, window_y + offset_y),
                            frame_fit,
                            ui,
                            frame_style,
                        );
                        {
                            #[cfg(feature = "debug-logging")]
                            let ir_t0 = Instant::now();
                            titlebar.update_input_region(ui, frame_style);
                            #[cfg(feature = "debug-logging")]
                            {
                                input_region_us += ir_t0.elapsed().as_micros();
                            }
                        }

                        // Render titlebar content
                        #[cfg(feature = "debug-logging")]
                        let tb_t0 = Instant::now();
                        let did_render = titlebar.render(
                            title.as_deref(),
                            is_focused,
                            is_maximized,
                            show_minimize,
                            show_maximize,
                            frame_style,
                            hovered_button,
                            titlebar_left_down,
                            ui,
                        );
                        #[cfg(feature = "debug-logging")]
                        {
                            titlebar_render_us += tb_t0.elapsed().as_micros();
                        }

                        // Position decoration so it sits above content with borders
                        titlebar.set_offset(offset_x, offset_y);

                        // Sync and commit (only if we have a buffer)
                        if did_render && titlebar.is_ready() {
                            titlebar.sync_next_commit();
                            titlebar.commit();
                            #[cfg(feature = "debug-logging")]
                            {
                                titlebars_rendered += 1;
                            }
                        }
                    }
                }
            }
        }

        #[cfg(feature = "debug-logging")]
        let loop_us = render_t0.elapsed().as_micros();

        #[cfg(feature = "debug-logging")]
        let finish_t0 = Instant::now();
        state.context.borrow().finish_render();
        #[cfg(feature = "debug-logging")]
        let finish_us = finish_t0.elapsed().as_micros();

        #[cfg(feature = "debug-logging")]
        {
            let total_us = total_t0.elapsed().as_micros();
            // Anything not explicitly attributed lives here -- if it
            // grows, the next thing to instrument is whatever Wayland
            // calls the loop wraps (attach/commit/sync).
            let attributed_us = handle_start_us
                + titlebar_render_us
                + shadow_render_us
                + input_region_us
                + finish_us;
            let other_us = total_us.saturating_sub(attributed_us);
            eprintln!(
            "[canoe render] RenderStart total={:.2}ms windows={} handle_start={:.2}ms loop={:.2}ms ({}tb={:.2}ms + {}sh={:.2}ms + input_region={:.2}ms + other={:.2}ms) finish={:.2}ms",
            total_us as f64 / 1000.0,
            windows_iterated,
            handle_start_us as f64 / 1000.0,
            loop_us as f64 / 1000.0,
            titlebars_rendered,
            titlebar_render_us as f64 / 1000.0,
            shadows_rendered,
            shadow_render_us as f64 / 1000.0,
            input_region_us as f64 / 1000.0,
            other_us as f64 / 1000.0,
            finish_us as f64 / 1000.0,
        );
        }
    }
}

/// Lay out the window manager's surfaces on output `output_id`, like the
/// `configure` events a layer-shell compositor sends, and draw them at
/// their new size.
pub(super) fn configure_canoe_layers(state: &mut State, output_id: canoe::OutputId) {
    let Some(output) = state.context.borrow().outputs.get(&output_id).cloned() else {
        return;
    };
    let output_size = {
        let out = output.borrow();
        (out.width, out.height)
    };

    let desktop_size = output
        .borrow()
        .desktop_surface
        .as_ref()
        .map(|d| (d.layer_surface.configure(output_size), (d.width, d.height)));
    if let Some(((width, height), old)) = desktop_size {
        {
            let mut out = output.borrow_mut();
            let desktop = out.desktop_surface.as_mut().unwrap();
            if old != (width, height) {
                desktop.reset_buffer();
            }
            desktop.configure(width, height);
        }
        render_desktop_surface(state, output_id);
    }

    let background_size = output
        .borrow()
        .desktop_background
        .as_ref()
        .map(|d| (d.layer_surface.configure(output_size), (d.width, d.height)));
    if let Some(((width, height), old)) = background_size {
        {
            let mut out = output.borrow_mut();
            let background = out.desktop_background.as_mut().unwrap();
            if old != (width, height) {
                background.reset_buffer();
            }
            background.configure(width, height);
        }
        render_desktop_background(state, output_id);
    }

    let menu_here = {
        let mut context = state.context.borrow_mut();
        match context.window_menu.as_mut() {
            Some(menu) if menu.output_id == output_id => {
                let (width, height) = menu.layer_surface.configure(output_size);
                if width > 0 && height > 0 && (menu.width, menu.height) != (width, height) {
                    menu.reset_buffer();
                    menu.width = width;
                    menu.height = height;
                }
                menu.configured = true;
                true
            }
            _ => false,
        }
    };
    if menu_here {
        render_window_menu(state);
    }

    {
        let mut context = state.context.borrow_mut();
        if let Some(shield) = context.window_menu_shield.as_mut() {
            if shield.output_id == output_id {
                let (width, height) = shield.layer_surface.configure(output_size);
                if width > 0 && height > 0 && (shield.width, shield.height) != (width, height) {
                    shield.reset_buffer();
                    shield.width = width;
                    shield.height = height;
                }
                shield.configured = true;
                shield.ensure_buffer();
                shield.render();
                shield.update_input_region();
                shield.commit();
            }
        }
    }

    let picker_here = {
        let mut context = state.context.borrow_mut();
        match context
            .share_picker
            .as_mut()
            .and_then(|picker| picker.surface_mut(output_id))
        {
            Some(surface) => {
                let (width, height) = surface.layer_surface.configure(output_size);
                surface.configure(width, height);
                true
            }
            None => false,
        }
    };
    if picker_here {
        render_share_picker(state);
    }
}

/// Handle `canoe --share-picker` requests and due nudges of shared windows.
/// Runs from a timer, as the server's connections come and go.
pub fn poll_share_server(state: &mut State) {
    use nix::poll::{poll, PollFd, PollFlags};

    let next_nudge = state.context.borrow().next_share_nudge();
    if next_nudge.is_some_and(|due| due <= Instant::now()) {
        state.context.borrow_mut().share_nudge_requested = true;
        state.schedule_wm();
    }

    let Some(server) = state.share_server.as_ref() else {
        return;
    };
    let (ready, hung_up) = {
        let fds = server.poll_fds();
        let ids: Vec<_> = fds.iter().map(|(_, _, id)| *id).collect();
        let mut poll_fds: Vec<PollFd> = fds
            .iter()
            .map(|(fd, input, _)| {
                PollFd::new(
                    *fd,
                    if *input {
                        PollFlags::POLLIN
                    } else {
                        PollFlags::empty()
                    },
                )
            })
            .collect();
        if poll(&mut poll_fds, nix::poll::PollTimeout::ZERO).unwrap_or(0) <= 0 {
            return;
        }
        let ready = poll_fds
            .iter()
            .any(|fd| fd.revents().is_some_and(|r| !r.is_empty()));
        let hung_up: Vec<canoe::share_picker::ShareRequestId> = poll_fds
            .iter()
            .zip(&ids)
            .filter(|(fd, _)| {
                fd.revents()
                    .is_some_and(|r| r.intersects(PollFlags::POLLHUP | PollFlags::POLLERR))
            })
            .filter_map(|(_, id)| *id)
            .collect();
        (ready, hung_up)
    };
    if !ready {
        return;
    }
    let (requests, gone) = match state.share_server.as_mut() {
        Some(server) => server.dispatch(&hung_up),
        None => (Vec::new(), Vec::new()),
    };
    // The portal gave up on the question (the app cancelled, say).
    let open_request = state
        .context
        .borrow()
        .share_picker
        .as_ref()
        .map(|picker| picker.request);
    if open_request.is_some_and(|id| gone.contains(&id)) {
        finish_share_picker(state, false, true);
    }
    for request in requests {
        start_share_picker(state, request);
    }
    state.queue_redraw_all();
}
