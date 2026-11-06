# Canoe 🛶 - River Window Manager

![Canoe screenshot](./assets/canoe-screenshot-1.png)

A stacking window manager for the River Wayland compositor, written in Rust.

## Features

- Stacking window management
- Server-side decorations with classic window borders & titlebars
- Titlebar/edge window movement and resizing (Super+Drag anywhere)
- Multihead support (focus/send windows across outputs)
- Window switcher (keyboard cycle + desktop right-click menu)
- Window focus follows click
- Minimized windows shown as icons on the desktop (can be disabled)
- Optional forcing server-side decorations via per-window rules
- Optional "swallowing" of client-side decoration via per-window rules
- Screen sharing of whole screens or individual windows, picked by clicking them

## Installation

```bash
cargo install canoe
```

## Running

```bash
river -c canoe
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

## Screen Sharing

Apps share the screen through the screencast portal. On River that portal is
[xdg-desktop-portal-wlr](https://github.com/emersion/xdg-desktop-portal-wlr)
(0.8 or newer for sharing single windows). It asks a *chooser* program what to
share, and Canoe is one: when an app wants to share, Canoe dims all screens and
highlights the window or screen under the pointer. Click to share it, or use
`Tab`/arrow keys and `Return`; `Esc` or a right click cancels.

Chrome asks twice per share: once for the preview in its own dialog, and again
when you confirm there. A request within 10 seconds of your pick that offers the
same window or screen gets the same answer without asking, once.

1. Install `xdg-desktop-portal-wlr` (e.g. `apt install xdg-desktop-portal-wlr`).

2. Route screen sharing to it in `~/.config/xdg-desktop-portal/river-portals.conf`:

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
exports `WAYLAND_DISPLAY` and `XDG_CURRENT_DESKTOP` (defaulting to `river`) to
the systemd user manager and D-Bus activation environment, using the same
values as the programs it launches (see [Environment
variables](#environment-variables)). If you manage the session environment
yourself (or run Canoe nested), turn that off:

```toml
update_activation_environment = false
```

wlroots 0.20.2 and earlier can show a black share when an app captures a
window that is already being captured (Chrome does this, previewing the window
while it asks to share it) and the window doesn't change. To work around it,
Canoe makes a window repaint shortly after you pick it, by briefly flagging it
as resizing and tiled. Its size doesn't change.

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
```

The commands are launched in order, but Canoe does not wait for one to finish
before starting the next. They start once Canoe has set up the desktop on every
output, so a wallpaper client launched here is drawn above
[`desktop_background`](#desktop-and-wallpapers). They run only at startup: a `SIGHUP` reload does not
run them again, so changes take effect the next time Canoe starts.

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
Colors accept `#RGB`, `#RRGGBB`, or `#RRGGBBAA`.

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

- `desktop_background` is a solid fill on the `background` layer. River stacks
  surfaces on the same layer in the order they are created, so a wallpaper
  client such as `swaybg` started after Canoe is drawn on top of it, and the
  fill shows through wherever the wallpaper is transparent. Set an alpha of
  `00` (e.g. `"#00000000"`) to draw no fill at all.
- Minimized-window icons are drawn on a transparent surface on the `bottom`
  layer, above any wallpaper.

By default the icon surface takes pointer input across the whole output:
right-clicking the empty desktop opens the window menu, and left-clicking it
unfocuses the current window and deselects icons. With `desktop_menu = false`,
only the icons take input and every other click goes to whatever is below,
e.g. an interactive wallpaper like `shaderbg`. Canoe then no longer sees clicks
on the empty desktop, so they neither open the menu nor unfocus windows.

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

## Requirements

- [River](https://codeberg.org/river/river) Wayland compositor
- foot terminal (for Super+Shift+Return)
- fuzzel (for Super+Space launcher)
- swaylock (for Super+L screen lock)
- xdg-desktop-portal-wlr (for screen sharing)

## License

MIT
