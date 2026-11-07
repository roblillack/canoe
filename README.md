# Canoe 🛶 - Stacking Wayland Compositor

![Canoe screenshot](./assets/canoe-screenshot-1.png)

A stacking Wayland compositor and window manager, written in Rust on top of
[Smithay](https://github.com/Smithay/smithay).

> **Experimental branch.** Canoe started out as a window manager for the
> River compositor. This branch runs the same window manager inside its own
> compositor instead. See [Status of the standalone
> compositor](#status-of-the-standalone-compositor) for what is missing.

## Features

- Stacking window management
- Server-side decorations with classic window borders & titlebars
- Titlebar/edge window movement and resizing (Super+Drag anywhere)
- Multihead support (focus/send windows across outputs, windows follow
  output layout and resolution changes, and move off unplugged outputs)
- New windows open fully on screen; apps that ask for more room than the
  screen has are shrunk to fit, as far as their minimum size allows
- Window switcher (keyboard cycle + desktop right-click menu)
- Window focus follows click
- Minimized windows shown as icons on the desktop (can be disabled)
- Optional forcing server-side decorations via per-window rules
- Optional "swallowing" of client-side decoration via per-window rules
- Screen sharing of whole screens or individual windows, picked by clicking them
- Shaped text with font fallback and color emoji in titles, menus and icon labels

## Installation

```bash
cargo install canoe
```

## Running

From a text console (VT), with seatd or systemd-logind managing the seat:

```bash
canoe
```

Inside a running Wayland or X11 session, `canoe` opens a window instead
(`--backend winit`; built with the default `winit` feature), which is handy
for trying it out. `--backend headless` runs without any monitor, for
automated tests (see `tools/e2e`).

To start Canoe from a display manager (GDM, SDDM, LightDM, greetd with
tuigreet, …), install the session file:

```bash
sudo install -Dm644 contrib/canoe.desktop /usr/share/wayland-sessions/canoe.desktop
```

The display manager looks up `canoe` in its own `PATH`, which does not include
your home directory, so changes in `~/.profile` or `~/.config/environment.d/`
don't help. If `canoe` lives in your home directory (e.g. after
`cargo install`), link it somewhere on the system `PATH`:

```bash
sudo ln -s ~/.cargo/bin/canoe /usr/local/bin/
```

## Building From Source

```bash
cargo build --release
```


## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Super+Shift+Return` | Open terminal (foot) |
| `Super+Space` | Open application launcher (fuzzel) |
| `Super+l` | Lock the screen (swaylock) |
| `Super+w` | Close focused window |
| `Super+Tab` | Focus next window |
| `Super+Shift+Tab` | Focus previous window |
| ``Super+` `` | Focus next window of the same application |
| `Super+Enter` | Toggle fullscreen |
| `Super+Down` | Unfullscreen/unmaximize, otherwise minimize focused window |
| `Super+Up` | Maximize focused window |
| `Super+h` | Minimize focused window |
| `Super+m` | Minimize focused window |
| `Super+Left` | Snap focused window to left half; restore if snapped right |
| `Super+Right` | Snap focused window to right half; restore if snapped left |
| `Super+Alt+Left` | Send focused window to previous output |
| `Super+Alt+Right` | Send focused window to next output |
| `Super+Alt+Up` | Send focused window to previous output |
| `Super+Alt+Down` | Send focused window to next output |

## Mouse Actions

| Action | Result |
|--------|--------|
| Click on window | Focus window |
| Drag titlebar | Move window |
| Drag window edges | Resize window |
| `Super+Left Drag` | Move window (anywhere) |
| `Super+Right Drag` | Resize window (anywhere) |

## Multiple Outputs

Windows belong to the output they were opened or last moved on, and keep
their place on it when the output layout changes:

- When an output moves, e.g. because one next to it was unplugged or
  rearranged with a tool like `wdisplays` or `kanshi`, its windows move with it.
- When an output is unplugged, its windows move to the output you are working
  on, or to the nearest remaining one if you were working on the unplugged
  output. They keep their relative position and are shrunk only if they don't
  fit. Snapped and maximized windows are snapped the same way on their new
  output, and fullscreen windows go back to their windowed size.
- When an output's resolution or scale changes, snapped and maximized windows
  follow the new size. Windows that fit on the output before but are now cut
  off are moved back onto it, and shrunk if needed and their minimum size
  allows. Windows you placed partly off-screen yourself stay where they are.

Sending a window to another output with `Super+Alt+Arrow` works the same way.

## Screen Sharing

Apps share the screen through the screencast portal. For Canoe that portal is
[xdg-desktop-portal-wlr](https://github.com/emersion/xdg-desktop-portal-wlr)
(0.8 or newer for sharing single windows). It asks a *chooser* program what to
share, and Canoe is one: when an app wants to share, Canoe dims all screens and
highlights the window or screen under the pointer. Click to share it, or use
`Tab`/arrow keys and `Return`; `Esc` or a right click cancels.

Chrome asks twice per share: once for the preview in its own dialog, and again
when you confirm there. A request within 10 seconds of your pick that offers the
same window or screen gets the same answer without asking, once.

1. Install `xdg-desktop-portal-wlr` (e.g. `apt install xdg-desktop-portal-wlr`).

2. Route screen sharing to it in `~/.config/xdg-desktop-portal/canoe-portals.conf`:

   ```ini
   [preferred]
   default=gtk
   org.freedesktop.impl.portal.ScreenCast=wlr
   org.freedesktop.impl.portal.Screenshot=wlr
   ```

3. Make Canoe its chooser in `~/.config/xdg-desktop-portal-wlr/config`:

   ```ini
   [screencast]
   chooser_type=dmenu
   chooser_cmd=canoe --share-picker
   ```

   The portal runs as a systemd user service, whose `PATH` may not include
   `~/.cargo/bin`; give the full path to `canoe` if it lives there.

4. Restart the portals (or log in again):
   `systemctl --user restart xdg-desktop-portal xdg-desktop-portal-wlr`.

The portal needs to know which Wayland display to capture. On startup Canoe
exports `WAYLAND_DISPLAY` and `XDG_CURRENT_DESKTOP` (defaulting to `canoe`) to
the systemd user manager and D-Bus activation environment, using the same
values as the programs it launches (see [Environment
variables](#environment-variables)). `XCURSOR_THEME` and `XCURSOR_SIZE` are
exported too if set, so services started this way use the same cursors. If you
manage the session environment yourself (or run Canoe nested), turn that off:

```toml
update_activation_environment = false
```

On River, wlroots 0.20.2 and earlier could show a black share when an app
captured a window that was already being captured (Chrome does this,
previewing the window while it asks to share it). Canoe's own compositor
renders every capture afresh and doesn't have this problem; the workaround
from the River days (briefly flagging the picked window as resizing and tiled
so it repaints) still runs, and is harmless.

## Configuration

Canoe reads `~/.config/canoe/canoe.toml`. After editing it, send `SIGHUP` to
re-read it without restarting (e.g. `pkill -HUP canoe`); this refreshes the
theme, desktop, window rules, and key bindings. Pass `--no-config` to ignore the
file and use the built-in defaults (reloads keep honoring this).

[`contrib/canoe.toml`](contrib/canoe.toml) lists every setting with its
default and is a good starting point for your own configuration;
[`contrib/themes/`](contrib/themes) has an example theme.

The main modifier defaults to `super`, but you can change it:

```toml
main_modifier = "alt"
```

The launcher defaults to `fuzzel`. You can override it with a command or argv:

```toml
launcher_cmd = "fuzzel"
# Or with arguments:
launcher_cmd = ["fuzzel", "--dmenu"]
```

The terminal defaults to `foot`. Override it the same way:

```toml
terminal_cmd = "alacritty"
# Or with arguments:
terminal_cmd = ["wezterm", "start"]
```

The screen locker defaults to `swaylock`. Override it the same way:

```toml
lock_cmd = "swaylock"
# Or with arguments:
lock_cmd = ["swaylock", "-f", "-c", "000000"]
```

### Startup commands

`startup_cmds` lists programs Canoe launches when it starts, e.g. to configure
displays, set a wallpaper, or start a notification daemon. Each entry is a
program name or an argv array, like `launcher_cmd`. A plain string is not run
through a shell, so pass arguments as an array (`"swaybg -i wall.png"` would
look for a program with that literal name).

```toml
startup_cmds = [
    "kanshi",
    "mako",
    ["swaybg", "-i", "/home/me/wallpaper.png", "-m", "fill"],
]

[ui]
desktop_background = "transparent"  # let the wallpaper show, see below
```

The commands are launched in order, but Canoe does not wait for one to finish
before starting the next. They start once Canoe has set up the desktop on every
output. They run only at startup: a `SIGHUP` reload does not run them again, so
changes take effect the next time Canoe starts.

When a program like `swaybg` draws your wallpaper, set `desktop_background =
"transparent"`, as above; see [Desktop and wallpapers](#desktop-and-wallpapers)
for why.

### Environment variables

Variables under the `[env]` table are set for every program Canoe launches:
startup commands, the launcher, terminal, screen locker, and hotkey commands.
They do not change Canoe's own environment. Changes apply to programs launched
after the next `SIGHUP` reload. Values for `WAYLAND_DISPLAY` and
`XDG_CURRENT_DESKTOP` are also what Canoe exports for the screen-sharing portal
on startup (see [Screen Sharing](#screen-sharing)).

```toml
[env]
MOZ_ENABLE_WAYLAND = "1"
QT_QPA_PLATFORM = "wayland"
```

All values must be strings: `MOZ_ENABLE_WAYLAND = 1` is a type error, which
makes Canoe ignore the whole configuration file.

### Cursor theme

`xcursor_theme` and `xcursor_size` set the XCursor theme and cursor size (in
logical pixels) for cursors that Canoe draws: on the desktop, titlebars and
borders, and while moving or resizing windows. Canoe also sets
`XCURSOR_THEME` and `XCURSOR_SIZE` for the programs it launches, so their
cursors match.

```toml
xcursor_theme = "Adwaita"
xcursor_size = 32
```

The theme and size are looked up separately: first in `canoe.toml`, then
`XCURSOR_THEME`/`XCURSOR_SIZE` in the [`[env]`](#environment-variables) table,
then Canoe's own environment. If none of them sets a value, Canoe uses the
`default` theme at size 24, and launched programs get no `XCURSOR_*` variable,
so they pick their own default.

A `SIGHUP` reload changes the cursors Canoe draws right away. Programs that are
already running keep the theme they started with.

GTK apps usually ignore these variables and take their cursor theme from
GNOME's settings instead (`gsettings set org.gnome.desktop.interface
cursor-theme Adwaita`, and `cursor-size` for the size).

### Hotkeys

Bind arbitrary key chords to commands under the `[hotkeys]` table. Each key is a
`+`-separated chord (modifiers first, the key last); each value is the command to
spawn, given as a string that is split on whitespace into program + arguments:

```toml
[hotkeys]
"Super+I"       = "control-panel"
"Super+P"       = "control-panel -m display"
"Super+Shift+T" = "foot"
```

If an argument itself contains spaces, use the array form instead, which is
taken verbatim as the argv (no splitting):

```toml
"Super+E" = ["sh", "-c", "notify-send 'hi there' && foot"]
```

Recognized modifiers are `super` (aliases `logo`/`win`/`mod4`), `alt` (`mod1`),
`ctrl`/`control`, `shift`, `mod3`, and `mod5`. Keys are named by their XKB keysym
(`i`, `Return`, `space`, `F5`, `XF86AudioRaiseVolume`, …), matched
case-insensitively — so `Super+I` triggers on the physical `I` key; add `Shift`
explicitly if you mean the shifted chord. A hotkey naming the same chord as a
built-in shortcut overrides it.

Edits to `[hotkeys]` (and `main_modifier`) take effect on the next `SIGHUP`
reload — no restart needed; canoe rebinds every seat's shortcuts.

### UI Settings

UI options live under the `[ui]` table and let you tune borders, titlebars, and menu colors.
Colors accept `#RGB`, `#RRGGBB`, `#RRGGBBAA`, or `transparent`.

Sizes (borders, shadows, fonts) are given in logical pixels. They scale with the
display's (fractional) scale and are drawn crisply at its native resolution.

```toml
[ui]
border_width = 10
border_active = { outer = "#FFD000", mid = "#000000", inner = "#FFD000" }
border_inactive = { outer = "#000000", mid = "#000000", inner = "#000000" }
titlebar_text_active = "#000000"
titlebar_text_inactive = "#808080"
titlebar_bg_active = "#FFD000"
titlebar_bg_inactive = "#202020"
menu_bg = "#000000"
menu_text = "#FFFFFF"
menu_highlight_bg = "#FFD000"
menu_highlight_text = "#000000"
button_bg = "#202020"
button_highlight = "#FFD000"
button_shadow = "#000000"
shadows_enabled = true          # master toggle; when false menus fall back to a flat L-shape
shadows_active_size = 25        # soft shadow size for the focused window (and menus)
shadows_inactive_size = 12      # soft shadow size for non-focused windows
shadows_color = "#00000033"     # shadow color used for the soft drop shadow
font_name = "Sans"
font_size = 15.0
desktop_background = "#101010"
desktop_menu = true             # set to false to let clicks on the empty desktop reach a wallpaper client
icons_enabled = true            # set to false to hide minimized-window icons on the desktop
icons_font_name = "Sans"        # optional; defaults to a regular-weight variant of font_name
icons_font_size = 12.0          # optional; defaults to font_size * 0.80
icons_text = "#FFFFFF"          # optional; defaults to menu_text
icons_highlight_bg = "#FFD000"  # optional; defaults to menu_highlight_bg
icons_highlight_text = "#000000" # optional; defaults to menu_highlight_text
```

#### Desktop and wallpapers

Canoe draws the desktop on two layer-shell surfaces per output:

- `desktop_background` is a solid fill on the `background` layer.
- Minimized-window icons are drawn on a transparent surface on the `bottom`
  layer, above any wallpaper.

**If you use a separate program to show a background image** (`swaybg`,
`shaderbg`, `wpaperd`, …), set:

```toml
[ui]
desktop_background = "transparent"
```

Wallpaper programs draw on the `background` layer too. Canoe always draws
its fill below them, so a wallpaper is never hidden by it, but drawing a fill
nobody sees is wasted work.

By default the icon surface takes pointer input across the whole output,
except where bars and panels reserve space (their exclusive zones), so clicks
on them always reach them: right-clicking the empty desktop opens the window
menu, and left-clicking it unfocuses the current window and deselects icons.
With `desktop_menu = false`, only the icons take input and every other click
goes to whatever is below, e.g. an interactive wallpaper like `shaderbg`.
Canoe then no longer sees clicks on the empty desktop, so they neither open
the menu nor unfocus windows.

### Rule Matching

Rules live under `[[rules]]` in `canoe.toml`. App ID matching uses OR across the
app-id fields, and property matching uses AND across the listed properties.

```toml
[[rules]]
match_app_id = ["foot", "kitty"]   # exact match, any value matches
match_app_id_prefix = "mate-"      # prefix match (e.g. mate-calc)
match_props = ["toplevel", "csd_only"] # all props must match
```

Supported `match_props` values:
- `toplevel` (window has no parent)
- `csd_only` (client does not support SSD)

The matching windows can have parts of their content "swallowed". This removes
the client-side decoration on my Firefox, for instance:

```toml
match_app_id = "firefox-esr"
match_props = "toplevel"
force_ssd = true
swallow_top = 48
```

## Status of the standalone compositor

Canoe's window manager is unchanged in substance: it runs in process and
talks to the compositor through the same requests and events it used with
River (see `src/wm.rs`), so its behavior carries over. `tools/e2e` runs
scripted scenarios on River with Canoe from `main` and on this compositor,
and compares screenshots: windows, decorations at fractional scales, menus,
the window switcher, icons, multiple outputs, layer shell, session locking,
screen-sharing picks, configuration and rules give the same results.

Supported: xdg-shell (with xdg-decoration and popups), wlr-layer-shell,
ext-session-lock, wlr-screencopy and ext-image-copy-capture (outputs and
single windows), ext-foreign-toplevel-list, wlr-output-management,
fractional scaling and viewporter, cursor-shape, data device, primary
selection and wlr-data-control, xdg-activation, presentation-time,
idle-notify and idle-inhibit, pointer constraints and relative pointer,
virtual keyboard and pointer, linux-dmabuf.

Missing or untested:

- **XWayland**: X11 programs don't run.
- **DRM/KMS**: the `udev` backend (real monitors) uses one GPU, has no
  direct scanout tuning, adaptive sync, gamma control or DRM leasing, and has
  only been compile-tested so far.
- No wlr-foreign-toplevel-management (taskbars that control windows), no
  tablet, touch or gesture input, no text-input/input-method.
- Input devices take libinput's defaults, apart from tap-to-click.

## Requirements

- libinput, libseat (seatd or systemd-logind), libgbm, libdrm, Mesa (EGL and
  OpenGL ES 2), pixman, xkbcommon, fontconfig, freetype
- foot terminal (for Super+Shift+Return)
- fuzzel (for Super+Space launcher)
- swaylock (for Super+L screen lock)
- xdg-desktop-portal-wlr (for screen sharing)
- A color emoji font such as Noto Color Emoji (optional, for emoji in window titles; without one, emoji are drawn in monochrome if any installed font has them)

## License

MIT
