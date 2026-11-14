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

/// Where an output lies from another one
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputDirection {
    Left,
    Right,
    Up,
    Down,
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
    /// Spawn the configured terminal emulator
    SpawnTerminal,
    /// Spawn the configured screen locker
    SpawnLock,

    /// Cycle focus through windows
    FocusIter { direction: Direction },
    /// Focus the output next to the current one: its last focused window,
    /// with the pointer in its middle
    FocusOutput { direction: OutputDirection },

    /// Send the focused window, or the selected desktop icon, to the output
    /// next to its own
    SendToOutput { direction: OutputDirection },
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
    /// Minimize all visible windows, or restore the ones this minimized
    ToggleShowDesktop,
    /// Unfullscreen/unmaximize if needed, otherwise hide (minimize) the focused window
    SmartHideFocused,
    /// Snap to a half; if on the opposite side, restore
    SmartSnapHalf { side: SnapSide },
    /// Maximize the focused window to the output
    MaximizeFocused,

    /// Activate selected window menu item
    ActivateMenuHovered,
    /// Cycle window menu entries
    WindowMenuCycle { direction: Direction },
    /// Cycle window menu entries for the focused application
    WindowMenuCycleApp,
    /// Main modifier released: activate the selected window switcher item,
    /// and end a desktop icon selection made by minimizing a window
    WindowMenuCommit,
    /// Cancel the window switcher menu, restoring the previously focused window
    WindowMenuCancel,

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

    /// Share the target highlighted in the screen-sharing picker
    SharePickerConfirm,
    /// Close the screen-sharing picker without sharing anything
    SharePickerCancel,
    /// Highlight the next target in the screen-sharing picker
    SharePickerNext,
    /// Highlight the previous target in the screen-sharing picker
    SharePickerPrev,

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
                direction: OutputDirection::Left,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Right.raw(),
            super_alt,
            Action::SendToOutput {
                direction: OutputDirection::Right,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Up.raw(),
            super_alt,
            Action::SendToOutput {
                direction: OutputDirection::Up,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Down.raw(),
            super_alt,
            Action::SendToOutput {
                direction: OutputDirection::Down,
            },
            super::BindingEvent::Pressed,
        ),
        // Focus the output next to the current one
        (
            Mode::Default,
            Keysym::Left.raw(),
            main | shift,
            Action::FocusOutput {
                direction: OutputDirection::Left,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Right.raw(),
            main | shift,
            Action::FocusOutput {
                direction: OutputDirection::Right,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Up.raw(),
            main | shift,
            Action::FocusOutput {
                direction: OutputDirection::Up,
            },
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Down.raw(),
            main | shift,
            Action::FocusOutput {
                direction: OutputDirection::Down,
            },
            super::BindingEvent::Pressed,
        ),
        // Focus navigation (cycle through windows)
        (
            Mode::Default,
            Keysym::Tab.raw(),
            main,
            Action::WindowMenuCycle {
                direction: Direction::Forward,
            },
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
            Action::WindowMenuCycle {
                direction: Direction::Reverse,
            },
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
        // Cancel the window switcher menu (Esc): restore the previously focused
        // window without activating the current selection. The main modifier is
        // still held while the menu is open, so it is part of the binding.
        (
            Mode::Default,
            Keysym::Escape.raw(),
            main,
            Action::WindowMenuCancel,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::Default,
            Keysym::Escape.raw(),
            main | shift,
            Action::WindowMenuCancel,
            super::BindingEvent::Pressed,
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
            Action::SpawnTerminal,
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
        // Spawn screen locker
        (
            Mode::Default,
            Keysym::l.raw(),
            main,
            Action::SpawnLock,
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
        (
            Mode::Default,
            Keysym::d.raw(),
            main,
            Action::ToggleShowDesktop,
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
        // Main+Up brings the selected icon's window back, where it would
        // maximize the focused window otherwise.
        (
            Mode::DesktopIcons,
            Keysym::Up.raw(),
            main,
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
        // Screen-sharing picker (SharePicker mode)
        (
            Mode::SharePicker,
            Keysym::Escape.raw(),
            NONE,
            Action::SharePickerCancel,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::SharePicker,
            Keysym::Return.raw(),
            NONE,
            Action::SharePickerConfirm,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::SharePicker,
            Keysym::KP_Enter.raw(),
            NONE,
            Action::SharePickerConfirm,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::SharePicker,
            Keysym::space.raw(),
            NONE,
            Action::SharePickerConfirm,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::SharePicker,
            Keysym::Tab.raw(),
            NONE,
            Action::SharePickerNext,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::SharePicker,
            Keysym::Right.raw(),
            NONE,
            Action::SharePickerNext,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::SharePicker,
            Keysym::Down.raw(),
            NONE,
            Action::SharePickerNext,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::SharePicker,
            Keysym::Tab.raw(),
            shift,
            Action::SharePickerPrev,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::SharePicker,
            Keysym::Left.raw(),
            NONE,
            Action::SharePickerPrev,
            super::BindingEvent::Pressed,
        ),
        (
            Mode::SharePicker,
            Keysym::Up.raw(),
            NONE,
            Action::SharePickerPrev,
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
