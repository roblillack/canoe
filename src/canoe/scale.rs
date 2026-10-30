//! How big a pixel is.
//!
//! Canoe deals in three kinds of pixel:
//!
//! * **Design pixels** — what every metric in the config and the code is
//!   given in: a 4px border, 12pt title text, a 20px shadow. They are aimed at
//!   a density of [`DEFAULT_UI_DPI`] (72 dpi), the same reference saudade lays
//!   its widgets out at, so Canoe's chrome and a saudade app's content come
//!   out the same size on the same screen.
//! * **Logical pixels** — the Wayland surface coordinates River lays windows
//!   out in. One of them stands for [`NOMINAL_DPI`] (96 dpi), so a design
//!   pixel spans `96 / ui_dpi` of them ([`UiConfig::density`]): 4/3 by
//!   default. Frame geometry is converted once, with the same factor on every
//!   output, so a window's frame doesn't change size as it crosses outputs.
//! * **Device pixels** — what a buffer is made of. A surface's logical size
//!   times the scale the compositor presents it at (`wp_fractional_scale_v1`,
//!   e.g. 1.25), presented through a `wp_viewport` so the buffer can be that
//!   fractional size.
//!
//! [`Scale`] maps the last two: surface edges go logical → device so the
//! drawing lines up with where River puts things, and the chrome's details
//! (lines, bevels, glyphs, text) go design → device at the snapped UI scale,
//! with thin lines kept to whole device pixels the way saudade draws them.
//!
//! [`UiConfig::density`]: crate::config::UiConfig::density

use wayland_client::protocol::wl_surface;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

/// What one logical (Wayland surface) pixel stands for.
pub const NOMINAL_DPI: f32 = 96.0;

/// The density a design pixel is aimed at unless `ui_dpi` says otherwise.
pub const DEFAULT_UI_DPI: f32 = 72.0;

/// `wp_fractional_scale_v1` reports scales in 120ths.
pub const FRACTIONAL_DENOMINATOR: u32 = 120;

/// The UI scale is snapped to a twelfth, as saudade's is.
const STEP: f32 = 12.0;

/// Below this UI scale a design pixel of thin line stays one device pixel;
/// above it, a whole multiple at a time. See [`chrome_px`].
const CHROME_PIN: f32 = 1.75;

/// From this UI scale up to [`CHROME_PIN`] a bevel is drawn 1.5x deep under a
/// one-pixel outline. See [`Scale::bevel`].
const BEVEL_PIN: f32 = 1.5;

/// How a surface's pixels map to device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    /// Device pixels per logical pixel.
    device: f64,
    /// Device pixels per design pixel, snapped to a twelfth.
    ui: f32,
}

impl Default for Scale {
    fn default() -> Self {
        Self::new(1.0, NOMINAL_DPI / DEFAULT_UI_DPI)
    }
}

impl Scale {
    /// The scale for a surface presented at `device` device pixels per logical
    /// pixel, with design pixels `density` logical pixels wide.
    pub fn new(device: f64, density: f32) -> Self {
        let device = if device.is_finite() && device > 0.0 {
            device
        } else {
            1.0
        };
        let ui = (device as f32 * density * STEP).round().max(1.0) / STEP;
        Self { device, ui }
    }

    /// Device pixels per logical pixel.
    pub fn device(&self) -> f64 {
        self.device
    }

    /// Device pixels per design pixel.
    pub fn ui(&self) -> f32 {
        self.ui
    }

    /// The device-pixel position of a logical coordinate. Used for anything
    /// that has to line up with geometry River knows about.
    pub fn edge(&self, logical: i32) -> i32 {
        (logical as f64 * self.device).round() as i32
    }

    /// A design-pixel length in device pixels.
    pub fn design(&self, design: i32) -> i32 {
        (design as f32 * self.ui).round() as i32
    }

    /// A design-pixel size (a font size, say) in device pixels, unrounded.
    pub fn design_f(&self, design: f32) -> f32 {
        design * self.ui
    }

    /// A thin line — an outline, a divider, a separator — in device pixels:
    /// one up to [`CHROME_PIN`], then a whole multiple at a time, so a line
    /// is never a blur across a pixel and a half.
    pub fn line(&self) -> i32 {
        chrome_px(self.ui)
    }

    /// How deep `n` design pixels of bevel run, in device pixels: `n` lines,
    /// except from [`BEVEL_PIN`] up to [`CHROME_PIN`], where a bevel is drawn at
    /// 1.5x (the half rounded up) so it doesn't vanish into a bigger face.
    pub fn bevel(&self, n: i32) -> i32 {
        let n = n.max(0);
        if (BEVEL_PIN..CHROME_PIN).contains(&self.ui) {
            (3 * n + 1) / 2
        } else {
            n * self.line()
        }
    }
}

fn chrome_px(ui: f32) -> i32 {
    let whole = (ui.floor() as i32).max(1);
    if ui / whole as f32 >= CHROME_PIN {
        whole + 1
    } else {
        whole
    }
}

/// A surface's handles on fractional scaling: the viewport that lets its
/// buffer be any size, and the object the compositor reports the scale it
/// presents the surface at through.
///
/// Without `wp_viewporter` a surface falls back to an integer buffer scale.
#[derive(Default)]
pub struct FractionalSurface {
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    /// The scale the compositor last asked for, in 120ths.
    preferred: Option<u32>,
    /// The destination size last sent to the viewport.
    destination: Option<(i32, i32)>,
}

impl FractionalSurface {
    pub fn new(viewport: Option<WpViewport>, fractional: Option<WpFractionalScaleV1>) -> Self {
        Self {
            viewport,
            fractional,
            preferred: None,
            destination: None,
        }
    }

    /// Record a `preferred_scale` event. True when it changed anything.
    pub fn set_preferred(&mut self, scale: u32) -> bool {
        if scale == 0 || self.preferred == Some(scale) {
            return false;
        }
        self.preferred = Some(scale);
        true
    }

    /// The device scale to draw at: the surface's own preferred scale, else
    /// `output_preferred` (what another surface on the same output was told),
    /// else the output's integer scale. Only the last is possible without a
    /// viewport, since nothing else can present a buffer that isn't a whole
    /// multiple of the surface.
    pub fn device_scale(&self, output_preferred: Option<u32>, integer_scale: i32) -> f64 {
        if self.viewport.is_some() {
            if let Some(preferred) = self.preferred.or(output_preferred) {
                return preferred as f64 / FRACTIONAL_DENOMINATOR as f64;
            }
        }
        integer_scale.max(1) as f64
    }

    /// Size `surface` to `width`×`height` logical pixels at `scale`, and
    /// return the buffer size that covers it in device pixels. The state is
    /// double-buffered: it takes effect with the next commit.
    pub fn apply(
        &mut self,
        surface: &wl_surface::WlSurface,
        width: i32,
        height: i32,
        scale: &Scale,
    ) -> (i32, i32) {
        match self.viewport {
            Some(ref viewport) => {
                surface.set_buffer_scale(1);
                if self.destination != Some((width, height)) {
                    viewport.set_destination(width, height);
                    self.destination = Some((width, height));
                }
                (scale.edge(width).max(1), scale.edge(height).max(1))
            }
            None => {
                let integer = (scale.device().round() as i32).max(1);
                surface.set_buffer_scale(integer);
                (width * integer, height * integer)
            }
        }
    }

    /// Release the protocol objects. Must run before the surface is destroyed.
    pub fn destroy(&mut self) {
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        if let Some(fractional) = self.fractional.take() {
            fractional.destroy();
        }
        self.destination = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DENSITY: f32 = NOMINAL_DPI / DEFAULT_UI_DPI;

    #[test]
    fn ui_scale_matches_saudade() {
        // saudade draws at os_scale * 96/72, snapped to a twelfth.
        for (device, ui) in [
            (1.0, 16.0 / 12.0),
            (1.25, 20.0 / 12.0),
            (1.5, 2.0),
            (2.0, 32.0 / 12.0),
        ] {
            assert!(
                (Scale::new(device, DENSITY).ui() - ui).abs() < 1e-6,
                "{device}"
            );
        }
    }

    #[test]
    fn thin_lines_are_whole_device_pixels() {
        let line = |device| Scale::new(device, DENSITY).line();
        assert_eq!(line(1.0), 1);
        assert_eq!(line(1.25), 1);
        assert_eq!(line(1.5), 2);
        assert_eq!(line(2.0), 2);
    }

    #[test]
    fn bevel_is_one_and_a_half_deep_between_the_pins() {
        // 1.25 draws at 5/3: inside [1.5, 1.75).
        let scale = Scale::new(1.25, DENSITY);
        assert_eq!((scale.bevel(1), scale.bevel(2)), (2, 3));
        let scale = Scale::new(1.0, DENSITY);
        assert_eq!((scale.bevel(1), scale.bevel(2)), (1, 2));
    }

    #[test]
    fn edges_round_to_device_pixels() {
        let scale = Scale::new(1.25, DENSITY);
        assert_eq!(scale.edge(5), 6);
        assert_eq!(scale.edge(25), 31);
        assert_eq!(scale.edge(100), 125);
    }
}
