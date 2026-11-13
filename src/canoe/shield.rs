//! Transparent overlay surface for blocking pointer input.

#![allow(dead_code)]

use super::surface::{Buffer, LayerSurface, Region, Surface};
use super::OutputId;

/// Transparent overlay surface used to capture pointer input.
pub struct ShieldSurface {
    pub surface: Surface,
    pub layer_surface: LayerSurface,
    pub buffer: Option<Buffer>,
    pub width: i32,
    pub height: i32,
    pub configured: bool,
    pub output_id: OutputId,
}

impl ShieldSurface {
    pub fn new(surface: Surface, layer_surface: LayerSurface, output_id: OutputId) -> Self {
        Self {
            surface,
            layer_surface,
            buffer: None,
            width: 0,
            height: 0,
            configured: false,
            output_id,
        }
    }

    pub fn configure(&mut self, width: i32, height: i32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.configured = true;
    }

    pub fn reset_buffer(&mut self) {
        self.buffer = None;
    }

    pub fn ensure_buffer(&mut self) {
        if self.width <= 0 || self.height <= 0 || self.buffer.is_some() {
            return;
        }
        // A single transparent pixel stretched over the output: the shield
        // only exists to take pointer input.
        self.buffer = Buffer::new(1, 1);
        self.surface.set_destination(self.width, self.height);
    }

    pub fn render(&mut self) {
        if let Some(ref buffer) = self.buffer {
            buffer.draw(|pixels| pixels.fill(0));
        }
    }

    pub fn update_input_region(&self) {
        if self.width <= 0 || self.height <= 0 {
            return;
        }

        let mut region = Region::new();
        region.add(0, 0, self.width, self.height);
        self.surface.set_input_region(Some(&region));
    }

    pub fn commit(&self) {
        if let Some(ref buffer) = self.buffer {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface
                .damage_buffer(0, 0, buffer.width(), buffer.height());
            self.surface.commit();
        }
    }
}

impl Drop for ShieldSurface {
    fn drop(&mut self) {
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}
