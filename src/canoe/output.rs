//! Output (display) management

#![allow(dead_code)]

use crate::protocol::{RiverLayerShellOutputV1, RiverOutputV1};
use std::cell::RefCell;
use std::rc::Weak;
use wayland_client::protocol::wl_output::WlOutput;

use super::DesktopSurface;

/// Output identifier
pub type OutputId = usize;

/// A managed output (display)
pub struct Output {
    /// Unique output ID
    pub id: OutputId,
    /// River output protocol object
    pub rwm_output: Option<RiverOutputV1>,
    /// Layer shell output for bar
    pub layer_shell_output: Option<RiverLayerShellOutputV1>,
    /// Wayland output object
    pub wl_output: Option<WlOutput>,
    /// Global name of the wl_output
    pub wl_output_name: u32,

    /// Output name (from wl_output)
    pub name: Option<String>,

    /// Position X in global coordinates
    pub x: i32,
    /// Position Y in global coordinates
    pub y: i32,
    /// Width
    pub width: i32,
    /// Height
    pub height: i32,
    /// Output scale factor (integer)
    pub scale: i32,
    /// The fractional scale (in 120ths) the compositor last asked one of our
    /// surfaces on this output to draw at. Seeds surfaces that haven't been
    /// told their own yet.
    pub preferred_scale: Option<u32>,

    /// Currently fullscreen window on this output
    pub fullscreen_window: Option<Weak<RefCell<super::Window>>>,

    /// Area left after layer surfaces' exclusive zones (x, y, width, height),
    /// in global coordinates, as last sent by the compositor. River resends
    /// it whenever the output's box changes, so it is never adjusted locally.
    pub non_exclusive_area: Option<(i32, i32, i32, i32)>,

    /// Desktop surface for pointer input
    pub desktop_surface: Option<DesktopSurface>,

    /// Solid-fill background surface (background layer, below wallpaper).
    pub desktop_background: Option<DesktopSurface>,

    /// Whether this output has been removed
    pub removed: bool,
}

impl Output {
    /// Create a new output
    pub fn new(id: OutputId) -> Self {
        Self {
            id,
            rwm_output: None,
            layer_shell_output: None,
            wl_output: None,
            wl_output_name: 0,
            name: None,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            scale: 1,
            preferred_scale: None,
            fullscreen_window: None,
            non_exclusive_area: None,
            desktop_surface: None,
            desktop_background: None,
            removed: false,
        }
    }

    /// The scale a surface on this output draws at before the compositor has
    /// told it its own: the fractional scale another surface here was given,
    /// else the integer output scale.
    pub fn render_scale(&self) -> super::scale::Scale {
        let device = match self.preferred_scale {
            Some(preferred) => preferred as f64 / super::scale::FRACTIONAL_DENOMINATOR as f64,
            None => self.scale.max(1) as f64,
        };
        super::scale::Scale::new(device)
    }

    /// The scale the desktop surface draws at: its own preferred scale once
    /// it has one, else [`render_scale`](Self::render_scale)'s.
    pub fn desktop_scale(&self) -> super::scale::Scale {
        match self.desktop_surface {
            Some(ref desktop) => super::scale::Scale::new(
                desktop
                    .fractional
                    .device_scale(self.preferred_scale, self.scale),
            ),
            None => self.render_scale(),
        }
    }

    /// Get usable area (after subtracting exclusive zones)
    ///
    /// Falls back to the whole output until the compositor has sent a
    /// non-exclusive area, or if the one it sent does not lie within the
    /// output (e.g. the output's new position has not arrived yet).
    pub fn usable_area(&self) -> (i32, i32, i32, i32) {
        let full = (self.x, self.y, self.width, self.height);
        match self.non_exclusive_area {
            Some((x, y, width, height))
                if width > 0
                    && height > 0
                    && x >= self.x
                    && y >= self.y
                    && x + width <= self.x + self.width
                    && y + height <= self.y + self.height =>
            {
                (x, y, width, height)
            }
            _ => full,
        }
    }

    /// Update position from protocol event
    pub fn update_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
    }

    /// Update dimensions from protocol event
    pub fn update_dimensions(&mut self, width: i32, height: i32) {
        self.width = width;
        self.height = height;
    }

    /// Update the non-exclusive area from the layer shell event; `x` and `y`
    /// are global, per the river-layer-shell-v1 protocol.
    pub fn update_non_exclusive_area(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.non_exclusive_area = Some((x, y, width, height));
    }

    /// Check if a point is within this output
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("x", &self.x)
            .field("y", &self.y)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_area_is_whole_output_without_non_exclusive_area() {
        let mut out = Output::new(0);
        out.update_dimensions(2560, 1440);
        out.update_position(1536, 0);
        assert_eq!(out.usable_area(), (1536, 0, 2560, 1440));
    }

    #[test]
    fn non_exclusive_area_is_not_offset_twice_when_output_moves() {
        // River sends the non-exclusive area (global) before the output's
        // new position, e.g. when kanshi moves an output.
        let mut out = Output::new(0);
        out.update_dimensions(2560, 1440);
        out.update_position(0, 0);
        out.update_non_exclusive_area(1536, 0, 2560, 1440);
        out.update_position(1536, 0);
        assert_eq!(out.usable_area(), (1536, 0, 2560, 1440));
    }

    #[test]
    fn non_exclusive_area_on_fresh_output_is_kept_global() {
        // A newly announced output: area first, then dimensions and position.
        let mut out = Output::new(0);
        out.update_non_exclusive_area(1536, 30, 2560, 1410);
        out.update_dimensions(2560, 1440);
        out.update_position(1536, 0);
        assert_eq!(out.usable_area(), (1536, 30, 2560, 1410));
    }

    #[test]
    fn non_exclusive_area_outside_output_falls_back_to_whole_output() {
        let mut out = Output::new(0);
        out.update_dimensions(2560, 1440);
        out.update_position(1536, 0);
        out.update_non_exclusive_area(3072, 0, 2560, 1440);
        assert_eq!(out.usable_area(), (1536, 0, 2560, 1440));
    }
}
