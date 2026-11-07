//! How big a pixel is.
//!
//! Every size in the config and the code is in **logical pixels**, the
//! Wayland surface coordinates windows are laid out in. A buffer is made of
//! **device pixels**: a surface's logical size times the output's scale (e.g.
//! 1.25).
//!
//! [`Scale`] maps one to the other: surface edges round to the device pixel
//! nearest to where the compositor puts them, and thin lines and bevels are
//! kept to whole device pixels so they never blur across a pixel and a half.
//!
//! The compositor rounds every surface edge from the *output's* origin, not
//! the surface's: a surface at logical `x`, `w` wide, covers device columns
//! `round(x·s)` up to `round((x + w)·s)` (as the wlroots scene graph does,
//! which canoe's decorations were first written for). Where an edge of a
//! decoration lands inside its buffer therefore depends on where the surface
//! sits on the output — see [`Origin`] and [`Scale::edge`]. Rounding from the
//! surface's own corner instead leaves the frame's cut-out a device pixel off
//! the client's content at some positions, and stretches the buffer by a
//! pixel whenever its size doesn't match the box it's drawn into.

use super::surface::Surface;

/// Fractional scales are expressed in 120ths, as `wp_fractional_scale_v1`
/// does.
pub const FRACTIONAL_DENOMINATOR: u32 = 120;

/// From this scale up to where a thin line becomes two device pixels, a
/// bevel is drawn 1.5x deep so it doesn't vanish into a bigger face. See
/// [`Scale::bevel`].
const BEVEL_PIN: f64 = 1.125;

/// Where a surface's top-left corner sits, in logical pixels from the
/// top-left corner of the output it's drawn on. This is what the compositor
/// rounds the surface's edges from; see [`Scale::edge`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Origin {
    pub x: i32,
    pub y: i32,
}

impl Origin {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// How a surface's logical pixels map to device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    /// Device pixels per logical pixel.
    device: f64,
}

impl Default for Scale {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl Scale {
    /// The scale for a surface presented at `device` device pixels per logical
    /// pixel.
    pub fn new(device: f64) -> Self {
        let device = if device.is_finite() && device > 0.0 {
            device
        } else {
            1.0
        };
        Self { device }
    }

    /// Device pixels per logical pixel.
    pub fn device(&self) -> f64 {
        self.device
    }

    /// Device pixels per logical pixel, for scaling text and other
    /// fractional sizes.
    pub fn factor(&self) -> f32 {
        self.device as f32
    }

    /// A logical coordinate or length in device pixels, rounded.
    pub fn px(&self, logical: i32) -> i32 {
        (logical as f64 * self.device).round() as i32
    }

    /// Where the surface-local logical coordinate `logical` lands, in device
    /// pixels from the surface's first device pixel, for a surface whose
    /// corner sits `origin` logical pixels from the output's (one axis of an
    /// [`Origin`]).
    ///
    /// This rounds exactly as wlroots' scene renderer does —
    /// `round((origin + logical) · scale)`, from the output's origin, in
    /// single precision like its `float` scale — so an edge drawn at
    /// `edge(origin, e)` falls on the same device pixel as the edge of any
    /// other surface River puts at logical `origin + e`, such as the client's
    /// content. `edge(origin, width)` is the size of the box the compositor
    /// draws a surface `width` wide into, so a buffer that size is presented
    /// pixel for pixel.
    pub fn edge(&self, origin: i32, logical: i32) -> i32 {
        self.position(origin + logical) - self.position(origin)
    }

    /// The device pixel a logical coordinate, from the output's origin, lands
    /// on: `round(logical · scale)`, in single precision as wlroots does it.
    pub fn position(&self, logical: i32) -> i32 {
        (logical as f32 * self.device as f32).round() as i32
    }

    /// A logical length in device pixels, rounded down: the least an
    /// [`edge`](Self::edge) that long comes to, wherever it starts.
    pub fn px_floor(&self, logical: i32) -> i32 {
        (logical as f32 * self.device as f32).floor() as i32
    }

    /// A fractional logical size (a font size, say) in device pixels,
    /// unrounded.
    pub fn px_f(&self, logical: f32) -> f32 {
        logical * self.factor()
    }

    /// A thin line — an outline, a divider, a separator — in device pixels:
    /// a logical pixel rounded to whole device pixels, at least one.
    pub fn line(&self) -> i32 {
        (self.device.round() as i32).max(1)
    }

    /// How deep `n` logical pixels of bevel run, in device pixels: `n` lines,
    /// except from [`BEVEL_PIN`] up to where a line becomes two pixels, where a
    /// bevel is drawn 1.5x deep (the half rounded up).
    pub fn bevel(&self, n: i32) -> i32 {
        let n = n.max(0);
        let line = self.line();
        if line == 1 && self.device >= BEVEL_PIN {
            (3 * n + 1) / 2
        } else {
            n * line
        }
    }
}

/// The scale one of canoe's surfaces is drawn at, and the logical size it
/// is presented at: its buffer can be any size, since canoe's compositor
/// draws it straight to the output's device pixels.
#[derive(Default)]
pub struct FractionalSurface {
    /// The scale the compositor last asked for, in 120ths.
    preferred: Option<u32>,
}

impl FractionalSurface {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the buffer may be any size. Always true for canoe's surfaces.
    pub fn has_viewport(&self) -> bool {
        true
    }

    /// Record the output's fractional scale. True when it changed anything.
    pub fn set_preferred(&mut self, scale: u32) -> bool {
        if scale == 0 || self.preferred == Some(scale) {
            return false;
        }
        self.preferred = Some(scale);
        true
    }

    /// The device scale to draw at: the surface's own preferred scale, else
    /// `output_preferred`, else the output's integer scale.
    pub fn device_scale(&self, output_preferred: Option<u32>, integer_scale: i32) -> f64 {
        if let Some(preferred) = self.preferred.or(output_preferred) {
            return preferred as f64 / FRACTIONAL_DENOMINATOR as f64;
        }
        integer_scale.max(1) as f64
    }

    /// Size `surface` to `width`×`height` logical pixels at `scale`, and
    /// return the buffer size that covers it in device pixels: the box the
    /// compositor draws the surface into when its corner sits at `origin`, so
    /// the buffer isn't resampled. Takes effect with the next commit.
    pub fn apply(
        &mut self,
        surface: &Surface,
        width: i32,
        height: i32,
        scale: &Scale,
        origin: Origin,
    ) -> (i32, i32) {
        surface.set_buffer_scale(1);
        surface.set_destination(width, height);
        (
            scale.edge(origin.x, width).max(1),
            scale.edge(origin.y, height).max(1),
        )
    }

    pub fn destroy(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thin_lines_are_whole_device_pixels() {
        let line = |device| Scale::new(device).line();
        assert_eq!(line(1.0), 1);
        assert_eq!(line(1.25), 1);
        assert_eq!(line(1.5), 2);
        assert_eq!(line(2.0), 2);
        assert_eq!(line(2.5), 3);
    }

    #[test]
    fn bevel_is_one_and_a_half_deep_between_the_pins() {
        let scale = Scale::new(1.25);
        assert_eq!((scale.bevel(1), scale.bevel(2)), (2, 3));
        let scale = Scale::new(1.0);
        assert_eq!((scale.bevel(1), scale.bevel(2)), (1, 2));
        let scale = Scale::new(1.5);
        assert_eq!((scale.bevel(1), scale.bevel(2)), (2, 4));
    }

    /// wlroots' `scale_box`: a box at logical `x`, `w` wide, covers device
    /// pixels `round(x·s)` up to `round((x + w)·s)`, from the output's origin.
    fn wlroots_box(scale: f32, x: i32, w: i32) -> (i32, i32) {
        let at = |v: i32| (v as f32 * scale).round() as i32;
        (at(x), at(x + w) - at(x))
    }

    #[test]
    fn edges_land_where_wlroots_puts_them() {
        for device in [1.0, 1.15, 1.25, 1.5, 1.75, 2.0, 2.25] {
            let scale = Scale::new(device);
            for origin in -40..40 {
                let (surface_x, _) = wlroots_box(device as f32, origin, 1);
                for e in 0..40 {
                    let (other_x, _) = wlroots_box(device as f32, origin + e, 1);
                    assert_eq!(
                        surface_x + scale.edge(origin, e),
                        other_x,
                        "scale {device}, origin {origin}, edge {e}"
                    );
                }
            }
        }
    }

    #[test]
    fn edges_depend_on_where_the_surface_sits() {
        // A 5px border at 1.25: from a corner at -3 the content starts 7
        // device pixels in (round(2.5) - round(-3.75) = 3 - -4), not the 6 a
        // border rounded on its own would make.
        let scale = Scale::new(1.25);
        assert_eq!(scale.edge(-3, 5), 7);
        assert_eq!(scale.edge(0, 5), 6);
        assert_eq!(scale.edge(1, 5), 7); // round(7.5) - round(1.25)
        assert_eq!(scale.edge(2, 5), 6); // round(8.75) - round(2.5)
        assert_eq!(scale.edge(-3, 5) + scale.edge(2, 100), scale.edge(-3, 105));
    }

    #[test]
    fn buffers_match_the_box_they_are_drawn_into() {
        let scale = Scale::new(1.25);
        for origin in -10..10 {
            for width in 1..50 {
                let (_, box_width) = wlroots_box(1.25, origin, width);
                assert_eq!(scale.edge(origin, width), box_width);
            }
        }
    }

    #[test]
    fn floor_is_the_shortest_an_edge_comes_to() {
        for device in [1.0, 1.15, 1.25, 1.5, 1.75, 2.0] {
            let scale = Scale::new(device);
            for length in 0..60 {
                let shortest = (-40..40).map(|o| scale.edge(o, length)).min().unwrap();
                assert!(scale.px_floor(length) <= shortest, "{device} {length}");
            }
        }
    }

    #[test]
    fn edges_round_to_device_pixels() {
        let scale = Scale::new(1.25);
        assert_eq!(scale.px(5), 6);
        assert_eq!(scale.px(25), 31);
        assert_eq!(scale.px(100), 125);
    }
}
