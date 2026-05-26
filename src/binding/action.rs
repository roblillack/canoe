//! Binding actions - what happens when a binding is triggered

#![allow(dead_code)]

use crate::config::Mode;

/// Direction for iteration/movement
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Reverse,
}

/// Snap target side for window actions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapSide {
    Left,
    Right,
}

impl SnapSide {
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// Window manager state for custom actions
#[derive(Debug, Clone)]
pub struct State {}

/// Argument types for custom functions
#[derive(Debug, Clone)]
pub enum Arg {
    None,
    Int(i32),
    Float(f32),
    Uint(u32),
    Char(char),
}

/// Type alias for custom action functions
pub type CustomFn = fn(&State, &Arg);

/// All possible binding actions
#[derive(Debug, Clone, Default)]
pub enum Action {
    /// Quit the window manager
    #[default]
    Quit,
    /// Close the focused window
    Close,

    /// Spawn a command
    Spawn { argv: Vec<String> },
    /// Spawn a shell command
    SpawnShell { cmd: String },
    /// Spawn the application launcher on the focused output
    SpawnLauncher,

    /// Cycle focus through windows
    FocusIter { direction: Direction },
    /// Cycle focus through outputs
    FocusOutputIter { direction: Direction },

    /// Send focused window to another output
    SendToOutput { direction: Direction },
    /// Start pointer move operation
    PointerMove,
    /// Start pointer resize operation
    PointerResize,

    /// Switch input mode
    SwitchMode { mode: Mode },

    /// Toggle fullscreen
    ToggleFullscreen { in_window: bool },

    /// Hide (minimize) the focused window
    HideFocused,
    /// Unfullscreen/unmaximize if needed, otherwise hide (minimize) the focused window
    SmartHideFocused,
    /// Snap to a half; if on the opposite side, restore
    SmartSnapHalf { side: SnapSide },
    /// Maximize the focused window to the output
    MaximizeFocused,

    /// Activate selected window menu item
    ActivateMenuHovered,
    /// Cycle window menu entries
    WindowMenuCycle,
    /// Cycle window menu entries for the focused application
    WindowMenuCycleApp,
    /// Activate selected window menu item
    WindowMenuCommit,

    /// Clear keyboard focus
    ClearFocus,
    /// Restore keyboard focus to the last focused window
    RestoreFocus,

    /// Move desktop icon selection to next icon
    IconSelectNext,
    /// Move desktop icon selection to previous icon
    IconSelectPrev,
    /// Move desktop icon selection up one row
    IconSelectUp,
    /// Move desktop icon selection down one row
    IconSelectDown,
    /// Activate (restore) the selected desktop icon
    IconActivate,
    /// Cancel desktop icon selection and exit icon mode
    IconCancel,

    /// Custom function action
    CustomFn { func: CustomFn, arg: Arg },
}

/// Default keybindings configuration for stacking WM
pub fn default_xkb_bindings(
    main_modifier: crate::config::MainModifier,
) -> Vec<(Mode, u32, u32, Action, super::BindingEvent)> {
    use crate::config::modifiers::*;
    use xkbcommon::xkb::Keysym;

    let main = main_modifier.mask();
    let shift = SHIFT;
    let super_alt = SUPER | ALT;
    let (main_left, main_right) = match main_modifier {
        crate::config::MainModifier::Alt => (Keysym::Alt_L.raw(), Keysym::Alt_R.raw()),
        crate::config::MainModifier::Super => (Keysym::Super_L.raw(), Keysym::Super_R.raw()),
    };

    vec![
        // Essential window management
        (
            Mode::Default,
            Keysym::w.raw(),
            main,
            Action::Close,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Down.raw(),
            main,
            Action::SmartHideFocused,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Up.raw(),
            main,
            Action::MaximizeFocused,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Left.raw(),
            main,
            Action::SmartSnapHalf {
                side: SnapSide::Left,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Right.raw(),
            main,
            Action::SmartSnapHalf {
                side: SnapSide::Right,
            },
            super::BindingEvent::Pressed,
        ),
        // Send window to other output
        (
            Mode::Default,
            Keysym::Left.raw(),
            super_alt,
            Action::SendToOutput {
                direction: Direction::Reverse,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Right.raw(),
            super_alt,
            Action::SendToOutput {
                direction: Direction::Forward,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Up.raw(),
            super_alt,
            Action::SendToOutput {
                direction: Direction::Reverse,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Down.raw(),
            super_alt,
            Action::SendToOutput {
                direction: Direction::Forward,
            },
            super::BindingEvent::Pressed,
        ),
        // Focus navigation (cycle through windows)
        (
            Mode::Default,
            Keysym::Tab.raw(),
            main,
            Action::WindowMenuCycle,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::grave.raw(),
            main,
            Action::WindowMenuCycleApp,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Tab.raw(),
            main | shift,
            Action::WindowMenuCycle,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            main_left,
            0,
            Action::WindowMenuCommit,
            super::BindingEvent::Released,
        ),
        (
            Mode::Default,
            main_right,
            0,
            Action::WindowMenuCommit,
            super::BindingEvent::Released,
        ),
        // Fullscreen toggle
        (
            Mode::Default,
            Keysym::Return.raw(),
            main,
            Action::ToggleFullscreen { in_window: false },
            super::BindingEvent::Pressed,
        ),
        // Spawn terminal
        (
            Mode::Default,
            Keysym::Return.raw(),
            main | shift,
            Action::Spawn {
                argv: vec!["foot".to_string()],
            },
            super::BindingEvent::Pressed,
        ),
        // Spawn launcher
        (
            Mode::Default,
            Keysym::space.raw(),
            main,
            Action::SpawnLauncher,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::h.raw(),
            main,
            Action::HideFocused,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::m.raw(),
            main,
            Action::HideFocused,
            super::BindingEvent::Pressed,
        ),
        // Desktop icon navigation (DesktopIcons mode)
        (
            Mode::DesktopIcons,
            Keysym::Right.raw(),
            NONE,
            Action::IconSelectNext,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::DesktopIcons,
            Keysym::Tab.raw(),
            NONE,
            Action::IconSelectNext,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::DesktopIcons,
            Keysym::Left.raw(),
            NONE,
            Action::IconSelectPrev,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::DesktopIcons,
            Keysym::Tab.raw(),
            shift,
            Action::IconSelectPrev,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::DesktopIcons,
            Keysym::Up.raw(),
            NONE,
            Action::IconSelectUp,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::DesktopIcons,
            Keysym::Down.raw(),
            NONE,
            Action::IconSelectDown,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::DesktopIcons,
            Keysym::Return.raw(),
            NONE,
            Action::IconActivate,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::DesktopIcons,
            Keysym::Escape.raw(),
            NONE,
            Action::IconCancel,
            super::BindingEvent::Pressed,
        ),
    ]
}

/// Default pointer bindings
pub fn default_pointer_bindings(
    main_modifier: crate::config::MainModifier,
) -> Vec<(Mode, u32, u32, Action)> {
    use crate::config::button;

    // Main+Drag to move, Main+Right-Drag to resize
    vec![
        (
            Mode::Default,
            button::LEFT,
            main_modifier.mask(),
            Action::PointerMove,
        ),
        (
            Mode::Default,
            button::RIGHT,
            main_modifier.mask(),
            Action::PointerResize,
        ),
    ]
}
