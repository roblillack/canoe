//! Canoe - River Window Manager core modules

mod context;
mod desktop;
mod font;
mod menu;
mod output;
mod render;
pub mod scale;
mod seat;
mod shadow;
pub mod share_picker;
mod shield;
pub mod surface;
pub mod titlebar;
pub mod window;

pub use context::Context;
pub use desktop::{
    desktop_input_area, regular_weight_font_query, ClientIcon, DesktopSurface, IconTheme,
    CLIENT_ICON_MAX_EDGE, ICON_SIZE,
};
pub use menu::{MenuItem, MenuTheme, WindowMenu};
pub use output::{Output, OutputId};
pub use seat::{PointerTarget, Seat, SeatId};
pub use shadow::WindowShadow;
pub use shield::ShieldSurface;
pub use titlebar::Titlebar;
pub use window::{Window, WindowEvent, WindowId};

/// Window menu interaction modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMenuMode {
    Pointer,
    AltTab,
}
