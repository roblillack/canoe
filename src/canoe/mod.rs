//! Canoe - River Window Manager core modules

mod context;
mod desktop;
mod font;
mod menu;
mod output;
mod render;
mod seat;
mod shield;
mod shmfile;
pub mod titlebar;
pub mod window;

pub use context::Context;
pub use desktop::DesktopSurface;
pub use menu::{MenuItem, MenuTheme, WindowMenu};
pub use output::{Output, OutputId};
pub use seat::{PointerTarget, Seat, SeatId};
pub use shield::ShieldSurface;
pub use titlebar::Titlebar;
pub use window::{Window, WindowEvent, WindowId};

/// Window menu interaction modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMenuMode {
    Pointer,
    AltTab,
}

/// User data for layer shell surfaces owned by the WM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerSurfaceKind {
    Desktop(OutputId),
    Menu,
    MenuShield(OutputId),
}
