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

    /// Exclusive area (for layer shell surfaces like bars)
    pub exclusive_x: i32,
    pub exclusive_y: i32,
    pub exclusive_width: i32,
    pub exclusive_height: i32,

    /// Desktop background surface for pointer input
    pub desktop_surface: Option<DesktopSurface>,

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
            exclusive_x: 0,
            exclusive_y: 0,
            exclusive_width: 0,
            exclusive_height: 0,
            desktop_surface: None,
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
    pub fn usable_area(&self) -> (i32, i32, i32, i32) {
        if self.exclusive_width > 0 && self.exclusive_height > 0 {
            let in_bounds = self.exclusive_x >= self.x
                && self.exclusive_x < self.x + self.width
                && self.exclusive_y >= self.y
                && self.exclusive_y < self.y + self.height;
            if !in_bounds {
                return (self.x, self.y, self.width, self.height);
            }
            (
                self.exclusive_x,
                self.exclusive_y,
                self.exclusive_width,
                self.exclusive_height,
            )
        } else {
            (self.x, self.y, self.width, self.height)
        }
    }

    /// Update position from protocol event
    pub fn update_position(&mut self, x: i32, y: i32) {
        let old_x = self.x;
        let old_y = self.y;
        self.x = x;
        self.y = y;
        // Reset exclusive area to match new position
        if self.exclusive_width == 0 {
            self.exclusive_x = x;
            self.exclusive_y = y;
        } else {
            let within_old = self.exclusive_x >= old_x
                && self.exclusive_x < old_x + self.width
                && self.exclusive_y >= old_y
                && self.exclusive_y < old_y + self.height;
            if within_old {
                self.exclusive_x += x - old_x;
                self.exclusive_y += y - old_y;
            }
        }
    }

    /// Update dimensions from protocol event
    pub fn update_dimensions(&mut self, width: i32, height: i32) {
        self.width = width;
        self.height = height;
        // Reset exclusive area to match new dimensions
        if self.exclusive_width == 0 {
            self.exclusive_width = width;
            self.exclusive_height = height;
        } else {
            let within_rel = self.exclusive_x >= 0
                && self.exclusive_x < self.width
                && self.exclusive_y >= 0
                && self.exclusive_y < self.height;
            let within_global = self.contains_point(self.exclusive_x, self.exclusive_y);
            if within_rel && !within_global {
                self.exclusive_x += self.x;
                self.exclusive_y += self.y;
            }
        }
    }

    /// Update exclusive area from layer shell
    pub fn update_exclusive_area(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.exclusive_x = x;
        self.exclusive_y = y;
        self.exclusive_width = width;
        self.exclusive_height = height;
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
