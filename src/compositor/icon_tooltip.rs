//! Tooltips of desktop icons whose clients gave them one (xx-iconlet-v1):
//! shown once the pointer rests on the icon for a moment, hidden when it
//! leaves or a button is pressed.

use std::time::Duration;

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::canoe;
use crate::canoe::scale::FractionalSurface;
use crate::canoe::surface::{Anchor, KeyboardInteractivity, Layer, LayerSurface, Surface};

use super::wm_glue::configure_canoe_layers;
use super::State;

/// How long the pointer rests on an icon before its tooltip shows.
const TOOLTIP_DELAY: Duration = Duration::from_millis(600);

/// The desktop icon the pointer is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooltipHover {
    window_id: canoe::WindowId,
    /// Tells this hover from an earlier one on the same icon.
    serial: u64,
    /// The pointer has rested long enough for the tooltip to show.
    due: bool,
    /// A button was pressed on the icon: no tooltip until the pointer
    /// leaves it.
    suppressed: bool,
}

impl State {
    /// The desktop icon under the pointer, unless a menu or the picker is
    /// open.
    fn icon_under_pointer(&self) -> Option<(canoe::OutputId, canoe::WindowId)> {
        let seat = self.wm_seat()?;
        let seat = seat.borrow();
        let canoe::PointerTarget::Desktop(output_id) = seat.pointer_target else {
            return None;
        };
        let context = self.context.borrow();
        if context.window_menu.is_some() || context.share_picker.is_some() {
            return None;
        }
        let output = context.outputs.get(&output_id)?;
        let window_id = output
            .borrow()
            .desktop_surface
            .as_ref()?
            .icon_at(seat.last_surface_x, seat.last_surface_y)?;
        Some((output_id, window_id))
    }

    /// Start or stop waiting for a tooltip after the pointer moved, or
    /// something moved under it.
    pub fn update_icon_tooltip(&mut self) {
        let hovered = self.icon_under_pointer().map(|(_, id)| id);
        if hovered == self.icon_tooltip_hover.map(|h| h.window_id) {
            return;
        }
        self.hide_icon_tooltip();
        self.icon_tooltip_serial += 1;
        let serial = self.icon_tooltip_serial;
        self.icon_tooltip_hover = hovered.map(|window_id| TooltipHover {
            window_id,
            serial,
            due: false,
            suppressed: false,
        });
        if hovered.is_none() {
            return;
        }
        let timer = Timer::from_duration(TOOLTIP_DELAY);
        let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
            if let Some(hover) = state
                .icon_tooltip_hover
                .as_mut()
                .filter(|h| h.serial == serial)
            {
                hover.due = true;
                state.show_icon_tooltip();
            }
            TimeoutAction::Drop
        });
    }

    /// A button was pressed: hide the tooltip until the pointer leaves the
    /// icon.
    pub fn suppress_icon_tooltip(&mut self) {
        if let Some(hover) = self.icon_tooltip_hover.as_mut() {
            hover.suppressed = true;
        }
        self.hide_icon_tooltip();
    }

    fn hide_icon_tooltip(&mut self) {
        if self.context.borrow_mut().icon_tooltip.take().is_some() {
            self.queue_redraw_all();
        }
    }

    /// The tooltip of `window_id`'s icon changed: show the new one right
    /// away if the old one was due.
    pub fn refresh_icon_tooltip(&mut self, window_id: canoe::WindowId) {
        if self
            .icon_tooltip_hover
            .is_some_and(|h| h.window_id == window_id && h.due)
        {
            self.hide_icon_tooltip();
            self.show_icon_tooltip();
        }
    }

    /// Show the tooltip of the icon the pointer rests on, if it has one.
    fn show_icon_tooltip(&mut self) {
        let Some(hover) = self.icon_tooltip_hover else {
            return;
        };
        if hover.suppressed || !hover.due {
            return;
        }
        let Some((output_id, window_id)) = self.icon_under_pointer() else {
            return;
        };
        if window_id != hover.window_id {
            return;
        }
        let Some(text) = self
            .window_by_id(window_id)
            .and_then(|w| super::protocols::iconlet::tooltip(w.wl_surface()))
        else {
            return;
        };
        let placement = {
            let context = self.context.borrow();
            context.outputs.get(&output_id).and_then(|output| {
                let output = output.borrow();
                let extent = output.desktop_surface.as_ref()?.icon_extent(window_id)?;
                Some((
                    extent,
                    (output.width, output.height),
                    output.render_scale(),
                    canoe::MenuTheme::from_ui(&context.config.ui),
                ))
            })
        };
        let Some((extent, output_size, scale, theme)) = placement else {
            return;
        };

        let surface = Surface::new();
        let layer_surface = LayerSurface::new(
            &surface,
            Some(output_id),
            Layer::Overlay,
            "canoe-icon-tooltip",
        );
        let tooltip = canoe::IconTooltip::new(
            surface,
            layer_surface,
            FractionalSurface::new(),
            output_id,
            window_id,
            text,
            extent,
            output_size,
            theme,
            scale,
        );
        tooltip.layer_surface.set_anchor(Anchor::Top | Anchor::Left);
        tooltip
            .layer_surface
            .set_margin(tooltip.origin_y, 0, 0, tooltip.origin_x);
        tooltip
            .layer_surface
            .set_size(tooltip.width as u32, tooltip.height as u32);
        tooltip
            .layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::None);
        tooltip.layer_surface.set_exclusive_zone(-1);
        tooltip.surface.commit();
        self.context.borrow_mut().icon_tooltip = Some(tooltip);
        configure_canoe_layers(self, output_id);
        self.queue_redraw_all();
    }
}

/// Lay out and draw the tooltip, if it is on output `output_id`.
pub(super) fn configure_icon_tooltip(
    state: &mut State,
    output_id: canoe::OutputId,
    output_size: (i32, i32),
) {
    let mut context = state.context.borrow_mut();
    let scale = context.icon_tooltip.as_ref().and_then(|tooltip| {
        let output = context.outputs.get(&tooltip.output_id)?;
        let output = output.borrow();
        let device = tooltip
            .fractional
            .device_scale(output.preferred_scale, output.scale);
        Some(canoe::scale::Scale::new(device))
    });
    let Some(tooltip) = context
        .icon_tooltip
        .as_mut()
        .filter(|t| t.output_id == output_id)
    else {
        return;
    };
    let (width, height) = tooltip.layer_surface.configure(output_size);
    if width > 0 && height > 0 && (tooltip.width, tooltip.height) != (width, height) {
        tooltip.reset_buffer();
        tooltip.width = width;
        tooltip.height = height;
    }
    tooltip.configured = true;
    tooltip.ensure_buffer(scale.unwrap_or_default());
    tooltip.render();
    tooltip.commit();
}
