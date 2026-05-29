//! Canoe - River Window Manager core modules

use std::sync::OnceLock;

mod context;
mod desktop;
mod font;
mod menu;
mod output;
mod render;
mod seat;
mod shadow;
mod shield;
mod shmfile;
pub mod titlebar;
pub mod window;

pub use context::Context;
pub use desktop::{regular_weight_font_query, DesktopSurface, IconTheme};
pub use menu::{MenuItem, MenuTheme, WindowMenu};
pub use output::{Output, OutputId};
pub use seat::{PointerTarget, Seat, SeatId};
pub use shadow::WindowShadow;
pub use shield::ShieldSurface;
pub use titlebar::Titlebar;
pub use window::{Window, WindowEvent, WindowId};

/// Whether verbose debug logging is enabled. Set `CANOE_DEBUG=1` to turn it on.
/// The env var is read once and cached so it adds no per-frame overhead.
pub fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CANOE_DEBUG").is_some())
}

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
