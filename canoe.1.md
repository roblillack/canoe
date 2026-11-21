% CANOE(1)
% Robert Lillack
% November 2026

# NAME

canoe - a stacking Wayland compositor with Win 3.x looks

# SYNOPSIS

**canoe** [**-n**] [**-s** *COMMAND*]... [**\-\-backend** *BACKEND*] [**\-\-no-xwayland**]

**canoe** **\-\-share-picker**

**canoe** **-h** | **-V**

# DESCRIPTION

Canoe is a stacking Wayland compositor and window manager. Windows overlap
and are moved and resized freely; Canoe draws classic window borders and
titlebars around every program that can be asked to leave its decorations to
the window manager, unless it turns them off while its window is shown, as to
hide its titlebar. Minimized windows become icons on the desktop, and
right-clicking the empty desktop opens a menu of all windows.

Started from a text console (VT), with seatd or systemd-logind managing the
seat, Canoe takes over the monitors and input devices of that seat. Started
inside a running Wayland or X11 session, it opens a window instead, which is
handy for trying it out.

X11 programs run through Xwayland, which Canoe starts if it is installed.
Programs Canoe launches find it through **DISPLAY**.

# OPTIONS

**-n**, **\-\-no-config**
: Ignore *~/.config/canoe/canoe.toml* and use the built-in defaults. Reloads
  keep honoring this.

**-s** *COMMAND*, **\-\-startup**=*COMMAND*
: Run *COMMAND* in the session once it is up, after the configuration's
  **startup_cmds**. Each is run through **sh -c**, so arguments, quoting and
  *$VARIABLES* work as in a shell. May be given more than once; the commands
  start in the order given.

**\-\-backend**=*BACKEND*
: Where to show the session:

  **udev** (alias **drm**)
  : On the monitors of this seat. The default on a text console.

  **winit** (alias **nested**)
  : In a window of the running session. The default when **WAYLAND_DISPLAY**
    or **DISPLAY** is set. Only available if Canoe was built with the
    **winit** feature, which is on by default.

  **headless**
  : Without any monitor, for automated tests. The outputs are taken from
    **CANOE_HEADLESS_OUTPUTS** (see ENVIRONMENT).

**\-\-no-xwayland**
: Don't start Xwayland, so X11 programs don't run.

**\-\-share-picker**
: Act as the screen-sharing chooser of **xdg-desktop-portal-wlr**: ask the
  running Canoe what to share, print the answer and exit. See SCREEN SHARING.

**-h**, **\-\-help**
: Print a short help and exit.

**-V**, **\-\-version**
: Print the version and exit.

# KEYBOARD SHORTCUTS

*Super* stands for the main modifier, which can be changed to *Alt* with
**main_modifier** in the configuration file. A hotkey for the same chord (see
CONFIGURATION) overrides a shortcut.

**Super+Shift+Return**
: Open a terminal (**terminal_cmd**, default **foot**).

**Super+Space**
: Open the application launcher (**launcher_cmd**, default **fuzzel**).

**Super+L**
: Lock the screen (**lock_cmd**, default **swaylock**).

**Super+W**
: Close the focused window.

**Super+Tab**, **Super+Shift+Tab**
: Focus the next or previous window.

**Super+\`**
: Focus the next window of the same application.

**Super+Return**
: Toggle fullscreen.

**Super+Up**
: Maximize the focused window.

**Super+Down**
: Unfullscreen or unmaximize the focused window; otherwise minimize it.

**Super+H**, **Super+M**
: Minimize the focused window.

**Super+D**
: Minimize all windows; press again to bring them back.

**Super+Left**, **Super+Right**
: Snap the focused window to the left or right half of the output; restore
  it if it is snapped to the other side.

**Super+Shift+***Arrow*
: Focus the output in that direction. The window last used there gets the
  focus, and the pointer jumps to the middle of the output.

**Super+Alt+***Arrow*
: Send the focused window, or the selected desktop icon, to the output in
  that direction.

Minimizing the focused window with **Super+Down**, **Super+H** or **Super+M**
selects its desktop icon for as long as *Super* stays pressed, so
**Super+Up** brings the window right back and **Super+Alt+***Arrow* sends the
icon to another output. Releasing *Super* focuses the next window.

A clicked desktop icon stays selected and has the keyboard:

*Arrow*, **Tab**, **Shift+Tab**
: Select the icon next to it.

**Super+Up**, **Return**
: Bring the selected icon's window back.

**Escape**
: Deselect the icon and focus the last window again.

# MOUSE ACTIONS

Click on a window
: Focus it.

Drag the titlebar
: Move the window.

Double-click the titlebar
: Maximize or restore the window.

Drag a window edge
: Resize the window.

**Super**+left drag
: Move the window, wherever it is grabbed.

**Super**+right drag
: Resize the window, wherever it is grabbed.

Right-click the empty desktop
: Open the window menu.

Left-click the empty desktop
: Unfocus the current window and deselect icons.

Double-click a desktop icon
: Bring its window back.

# MULTIPLE OUTPUTS

Windows belong to the output they were opened or last moved on, and keep
their place on it when the output layout changes. When an output is
unplugged, its windows move to the output in use, or to the nearest remaining
one, keeping their relative position.

Canoe remembers how the outputs are set up for each combination of connected
monitors, and sets them up that way again when that combination comes back,
e.g. when docking a laptop. Set them up once with any tool that speaks
wlr-output-management, such as **wdisplays**(1) or **wlr-randr**(1). Monitors
are recognized by make, model and serial number. On a laptop, whether the
lid is closed counts as part of the combination.

Each combination can have a primary output: the one new windows open on when
monitors come or go, and the one launchers and panels without an output of
their own appear on. Output tools choose it through Canoe's experimental
**xx-primary-output-v1** protocol.

# DESKTOP ICONS

A minimized window's icon is the first of these that exists:

1. *~/.config/canoe/icons/APP-ID.svg* or *.png*;
2. the icon the program set for the window (xdg-toplevel-icon);
3. the **Icon=** of the program's desktop entry, *APP-ID.desktop*;
4. the first letter of the window's title.

Programs can draw a minimized window's icon themselves with Canoe's
experimental **xx-iconlet-v1** protocol. An X11 window's app id is the class
of its **WM_CLASS**.

# CONFIGURATION

Canoe reads *~/.config/canoe/canoe.toml*. Every setting is optional. If the
file can't be parsed, Canoe ignores all of it and uses the built-in defaults.
Send **SIGHUP** to re-read it without restarting. A sample listing every
setting with its default comes with Canoe as *contrib/canoe.toml*.

## Top-level settings

**main_modifier** = "super" | "alt"
: Modifier for the built-in shortcuts. Default: "super".

**window_list_scope** = "output" | "all"
: Whether **Super+Tab**, **Super+\`** and the window menu list the windows
  on the output they open on, or on every output. Default: "output".

**launcher_cmd**, **terminal_cmd**, **lock_cmd**
: Programs for the launcher, terminal and screen locker shortcuts. Each is a
  program name or an argv array, e.g. `["swaylock", "-f", "-c", "000000"]`.
  A plain string is not run through a shell. Defaults: "fuzzel", "foot",
  "swaylock".

**startup_cmds**
: Programs launched once when Canoe has set up the desktop, in order, without
  waiting for one to finish. Each entry is a program name or an argv array.
  They don't run again on reload.

**xcursor_theme**, **xcursor_size**
: XCursor theme and size (in logical pixels) for the cursors Canoe draws.
  Also set as **XCURSOR_THEME** and **XCURSOR_SIZE** for programs Canoe
  launches. Unset by default: Canoe then looks at the **[env]** table and its
  own environment, falling back to the "default" theme at size 24. A theme
  that is not installed is replaced by one that is.

**xwayland_hidpi** = true | false
: Let X11 programs draw at the outputs' scale, rounded up to a whole number,
  so they are crisp. With false they draw at scale 1 and are stretched.
  Default: true.

**update_activation_environment** = true | false
: On startup, export **WAYLAND_DISPLAY**, **DISPLAY**, **XDG_CURRENT_DESKTOP**,
  **XDG_SESSION_TYPE** and, if set, **XCURSOR_THEME** and **XCURSOR_SIZE** to
  the systemd user manager and D-Bus activation environment, so portals
  started on demand find this session. Default: true. The headless backend
  never exports anything.

**remember_outputs** = true | false
: Remember and restore output layouts and the primary output (see MULTIPLE
  OUTPUTS). With false, new outputs are lined up left to right. Default: true.

**emulate_sway** = true | false
: Answer sway's IPC on the socket **SWAYSOCK** names (set for programs Canoe
  launches), telling where outputs and windows are and which is focused, for
  screenshot tools like **shotman** and **grimshot**. Commands and events are
  not supported. Default: false.

**emulate_kwin** = true | false
: Take the names **org.kde.KWin** and **org.kde.KWin.ScreenShot2** on the
  session bus and offer KWin's screenshot interface, for **spectacle**(1).
  Default: false.

## [env]

Environment variables set for every program Canoe launches: startup
commands, the launcher, terminal, screen locker and hotkey commands. They do
not change Canoe's own environment. Values must be strings.

```
[env]
QT_QPA_PLATFORM = "wayland;xcb"
ELECTRON_OZONE_PLATFORM_HINT = "auto"
```

## [keyboard]

**layout**, **variant**, **options**, **rules**, **model**
: The XKB keymap, with comma-separated lists as in
  */usr/share/X11/xkb/rules/base.lst*. Whatever is left out comes from
  **XKB_DEFAULT_LAYOUT** and friends in Canoe's own environment, then from
  libxkbcommon's defaults ("us"). **options = ""** turns off any from
  **XKB_DEFAULT_OPTIONS**.

**repeat_rate**, **repeat_delay**
: Keys per second, and milliseconds before a held key repeats.

With more than one layout, switch between them with an XKB option such as
**grp:alt_shift_toggle**, or with a hotkey. Shortcuts and hotkeys name Latin
keys and keep working while a layout without them is active.

## [hotkeys]

Key chords that run commands. Each key is a **+**-separated chord, modifiers
first; each value is a command, either a string split on whitespace or an
argv array taken verbatim. Instead of a command, **{ switch_layout = "next" }**,
**"previous"** or a layout's place in the list (counting from 1) switches the
keyboard layout.

Modifiers are **super** (**logo**, **win**, **mod4**), **alt** (**mod1**),
**ctrl** (**control**), **shift**, **mod3** and **mod5**. Keys are XKB keysym
names (**i**, **Return**, **space**, **F5**, **XF86AudioRaiseVolume**, ...),
matched case-insensitively; add **Shift** explicitly to mean the shifted
chord.

```
[hotkeys]
"Super+I" = "control-panel"
"Super+E" = ["sh", "-c", "notify-send 'hi there' && foot"]
"Ctrl+Alt+space" = { switch_layout = "next" }
```

## [ui]

Look of window frames, menus and the desktop. Colors are **#RGB**,
**#RRGGBB**, **#RRGGBBAA** or **"transparent"**. Sizes are in logical pixels
and scale with the output.

**border_width**
: Width of window borders. Default: 5.

**border_active**, **border_inactive**
: Border colors as **{ outer = ..., mid = ..., inner = ... }**.

**titlebar_bg_active**, **titlebar_bg_inactive**, **titlebar_text_active**, **titlebar_text_inactive**
: Titlebar colors.

**titlebar_text_align** = "center" | "left"
: Where window titles go. Default: "center".

**menu_bg**, **menu_text**, **menu_highlight_bg**, **menu_highlight_text**
: Menu colors.

**button_bg**, **button_highlight**, **button_shadow**
: Titlebar button colors.

**shadows_enabled**, **shadows_active_size**, **shadows_inactive_size**, **shadows_color**
: Soft drop shadows below windows and menus. Defaults: true, 25, 12,
  "#00000033".

**font_name**, **font_size**
: Font for titles and menus, as a fontconfig pattern. Defaults: "sans", 15.0.

**desktop_background**
: Fill color of the desktop. Default: "#008080". Use "transparent" when a
  program like **swaybg**(1) draws the wallpaper.

**desktop_menu** = true | false
: Whether the empty desktop takes clicks (to open the window menu and
  unfocus windows). With false, only icons do and every other click goes to
  the wallpaper below. Default: true.

**icons_enabled** = true | false
: Show minimized windows as desktop icons. Default: true.

**icons_font_name**, **icons_font_size**, **icons_text**, **icons_highlight_bg**, **icons_highlight_text**
: Look of icon labels. Default to values derived from the font and menu
  settings above.

**animations_enabled**, **animations_duration**
: Let windows glide into place when maximized, snapped or restored, and
  shrink into their icons when minimized. Duration in milliseconds; 0 turns
  animations off. Defaults: true, 150.

## [[rules]]

Per-window rules. A window matches a rule if its app id matches any of
**match_app_id** (exact), **match_app_id_prefix** or **app_id_regex**, or its
title matches **title** (exact) or **title_regex**, and it has all
properties listed in **match_props**:

**toplevel**
: The window has no parent.

**csd_only**
: The program draws its own decorations and can't be asked not to (e.g.
  libadwaita programs).

A matching rule can set:

**force_ssd** = true
: Draw Canoe's decorations even for **csd_only** windows.

**swallow_top** = *PIXELS*
: Hide that many pixels at the top of the window, e.g. to remove a
  program's own titlebar.

```
[[rules]]
match_app_id = ["firefox", "firefox-esr"]
match_props = "toplevel"
force_ssd = true
swallow_top = 48
```

# SCREEN SHARING

Programs share the screen through the screencast portal, which for Canoe is
**xdg-desktop-portal-wlr** (0.8 or newer for single windows). Route the
portals with *canoe-portals.conf* (shipped as *contrib/canoe-portals.conf*,
installed into */usr/share/xdg-desktop-portal/* or
*~/.config/xdg-desktop-portal/*) and make Canoe the chooser in
*~/.config/xdg-desktop-portal-wlr/config*:

```
[screencast]
chooser_type=dmenu
chooser_cmd=canoe --share-picker
```

When a program wants to share, Canoe dims all screens and highlights the
window or screen under the pointer. Click to share it, or use **Tab**, the
arrow keys and **Return**; **Escape** or a right click cancels.

# SIGNALS

**SIGHUP**
: Re-read the configuration file: theme, desktop, window rules, keyboard,
  hotkeys and screenshot emulation are updated. Startup commands don't run
  again.

**SIGINT**, **SIGTERM**, **SIGQUIT**
: Quit.

**SIGUSR1**
: With the headless backend and **CANOE_HEADLESS_LID** set, open or close
  the lid.

# ENVIRONMENT

## Read by Canoe

**WAYLAND_DISPLAY**, **DISPLAY**
: If either is set, Canoe defaults to the **winit** backend. For
  **\-\-share-picker**, **WAYLAND_DISPLAY** names the Canoe to ask.

**XDG_RUNTIME_DIR**
: Where the sockets for **\-\-share-picker** and **emulate_sway** are
  created.

**XDG_STATE_HOME**
: Where output layouts are remembered. Defaults to *~/.local/state*.

**XKB_DEFAULT_RULES**, **XKB_DEFAULT_MODEL**, **XKB_DEFAULT_LAYOUT**, **XKB_DEFAULT_VARIANT**, **XKB_DEFAULT_OPTIONS**
: The keymap, for whatever **[keyboard]** leaves out.

**XCURSOR_THEME**, **XCURSOR_SIZE**
: The cursor theme, unless set in the configuration file.

**CANOE_OUTPUT_PROFILES**
: File to remember output layouts in, instead of the default. The headless
  backend remembers none unless this is set.

**CANOE_HEADLESS_OUTPUTS**
: Outputs of the headless backend, as comma-separated
  *WIDTH*x*HEIGHT*\[\@*SCALE*\], e.g. **1920x1080@1.5,1280x720**.
  Default: **1280x720**.

**CANOE_HEADLESS_REVERSE**
: If set, the headless outputs are announced last to first.

**CANOE_HEADLESS_LID** = **open** | **closed**
: Make the first headless output a laptop's panel with its lid open or
  closed.

## Set by Canoe

Programs Canoe launches see **WAYLAND_DISPLAY** naming its socket,
**DISPLAY** naming Xwayland, **XDG_SESSION_TYPE=wayland**, **SWAYSOCK** if
**emulate_sway** is on, and the **[env]** table.

# FILES

*~/.config/canoe/canoe.toml*
: The configuration file.

*~/.config/canoe/icons/*
: Icons for minimized windows, named *APP-ID.svg* or *APP-ID.png*.

*~/.local/state/canoe/outputs.toml*
: Remembered output layouts.

*$XDG_RUNTIME_DIR/canoe-WAYLAND_DISPLAY.sock*
: The socket **canoe \-\-share-picker** talks to.

*/usr/share/wayland-sessions/canoe.desktop*
: Session file for display managers, shipped as *contrib/canoe.desktop*.

# EXIT STATUS

**canoe** exits with 0 when it is asked to quit, 1 if it can't start (no
seat, no GPU, ...), and 2 for unknown options.

**canoe \-\-share-picker** exits with 0 when the running Canoe answered,
whether something was picked or not, and 1 if it can't be reached.

# BUGS

To report bugs, please create a ticket at
https://github.com/roblillack/canoe/issues

# SEE ALSO

**foot**(1), **fuzzel**(1), **swaylock**(1), **swaybg**(1), **grim**(1),
**xdg-desktop-portal-wlr**(5), **xkeyboard-config**(7), **Xwayland**(1)
