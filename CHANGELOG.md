# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While pre-1.0, the minor version is bumped for breaking changes.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### [A Canoe without a River](https://github.com/roblillack/canoe/discussions/83)

Canoe is now a Wayland compositor of its own, built on Smithay, instead of a
window manager running inside River. Start it with `canoe` from a text console
or pick the Canoe session in your display manager. Your configuration, rules
and themes carry over, and X11 programs run through Xwayland. (#66)

Canoe now needs libinput, libseat (seatd or systemd-logind), libgbm, libdrm,
Mesa (EGL and OpenGL ES 2), pixman, and Xwayland for X11 programs. Packages
should install the session file `contrib/canoe.desktop`, the portal
configuration `contrib/canoe-portals.conf` and the screen-sharing chooser
configuration `contrib/xdg-desktop-portal-wlr/canoe`, and depend on
xdg-desktop-portal-wlr; see Installation in the README.

### Added

- Canoe remembers the output layout for each set of connected monitors and
  restores it when that set comes back, e.g. when docking a laptop. Set it up
  once with any output tool, like `wdisplays` or `wlr-randr`; kanshi is no
  longer needed. `remember_outputs = false` turns this off. See Multiple
  Outputs in the README. (#82)
- Closing a laptop's lid turns its panel off, and opening it turns the panel
  back on. Lid open and lid closed can each have their own layout. (#82)
- A primary output for each set of monitors, where new windows and launchers
  open when monitors come or go, and where the pointer starts. Output tools
  choose it through Canoe's experimental `xx-primary-output-v1` protocol. (#82)
- Monitors report their make, model and serial number, so a remembered layout
  follows a monitor to another port and output tools can tell monitors
  apart. (#82)
- Super+Shift+Arrow focuses the output in that direction. (#92)
- `-s`/`--startup` launches a program in the session, e.g.
  `canoe -s foot -s firefox`; it can be given any number of times. (#100)
- Windows glide into place when they are maximized, snapped or restored, and
  minimized windows shrink into their desktop icons. `animations_enabled` and
  `animations_duration` under `[ui]` turn the animations off or change their
  speed. (#76)
- Double-clicking a titlebar maximizes or restores the window. (#93)
- Super+D minimizes all windows on all outputs; pressing it again brings them
  back. (#72)
- While dragging, the cursor shows whether a drop would copy, move, ask or
  not be accepted, and Escape cancels the drag. (#91)
- A `[keyboard]` section sets the keyboard layouts, variants, options and key
  repeat, and hotkeys can switch layouts, e.g. `{ switch_layout = "next" }`.
  See Keyboard in the README. (#97)
- X11 programs are sharp on scaled outputs instead of blurry. Programs that
  can't scale come out small; `xwayland_hidpi = false` brings back the old
  behavior. (#80)
- Screenshot tools made for sway (grimshot, shotman) or KDE (Spectacle) work
  once `emulate_sway` or `emulate_kwin` is turned on. See Screenshots in the
  README. (#81)
- A minimized window's desktop icon is the icon its program sets
  (`xdg-toplevel-icon-v1`), unless there is one in `~/.config/canoe/icons/`.
  (#67)
- Iconlets: small programs that live as a desktop icon they draw themselves,
  like a CPU meter, through Canoe's experimental `xx-iconlet-v1` protocol.
  Double-clicking an iconlet is passed on to its program. Through the same
  protocol, any program can draw its minimized window's icon itself. (#78)
- `org_kde_plasma_shell`, of which only `set_skip_switcher` is supported, to
  keep a window out of the window menu, Super+Tab and focus cycling. (#70)
- `xdg-toplevel-tag-v1`, without which Spectacle crashes. (#81)
- Cursor capture sessions of `ext-image-copy-capture-v1` report where the
  pointer is, for programs like eyes following it. (#75)
- A Nix flake and package. (#36, thanks @alvv-z)
- A manpage, `man canoe`, and a Makefile: `make && sudo make install` installs
  Canoe, its manpage, the session file and the portal configuration under
  `/usr/local`. (#102)
- `make install` makes Canoe the screen-sharing chooser of
  xdg-desktop-portal-wlr in Canoe sessions, by installing
  `/etc/xdg/xdg-desktop-portal-wlr/canoe`, so screen sharing needs no
  configuration of your own. (#111)

### Changed

- Every window gets Canoe's titlebar, including GTK 3 and GTK 4 apps, which
  used to draw their own, except windows that can only draw their own
  decorations, like libadwaita apps: they no longer get a second titlebar on
  top. A `force_ssd` rule still gives them Canoe's. (#66)
- Window titles are centered in the titlebar. `titlebar_text_align = "left"`
  under `[ui]` brings back the old look. (#89)
- The window menu and Super+Tab show each window's application name before
  its title, e.g. "Alacritty — ~: fish" or "Google Chrome — Page", and find
  names and icons for more applications. (#73)
- Super+Tab, Super+` and the desktop's window menu list only the windows on
  their output. `window_list_scope = "all"` lists all windows again. (#92)
- Super+Alt+Arrow sends the focused window to the output in that direction,
  instead of to the next or previous output in no particular order. (#92)
- Minimized windows can be handled from the keyboard: minimizing with Super
  keeps the desktop icon selected until Super is released, so Super+Up brings
  the window right back. A clicked icon can be moved between with the arrow
  keys, restored with Super+Up and sent to another output with
  Super+Alt+Arrow. (#95)
- Window frames and shadows use less memory. (#90)

### Removed

- Running as a window manager inside River. Canoe 0.6.2 is the last version
  that does. (#66)

### Fixed

- Windows are no longer minimized when the only monitor goes away for a
  moment, e.g. while it wakes up. (#82)
- Windows of a monitor that goes away while the screen is locked, e.g. on
  suspend, wait for the unlock and go back to it if it came back, instead of
  all ending up on the remaining screen. (#96)
- A fullscreen window stays fullscreen when a window on another output is
  focused, so programs like Spectacle can cover every output to select a
  region. (#81)
- Windows that ask to start maximized or fullscreen, as Qt programs do, start
  out that way. (#81)
- The desktop icon of a minimized window disappears when the window closes.
  (#69)
- Shortcuts and hotkeys work while a non-Latin keyboard layout, like Cyrillic
  or Greek, is active. (#97)
- Menus and other popups of a window that draws its own decorations, with a
  shadow around them, open where the program puts them instead of shifted up
  and to the left by the shadow's size. (#98)
- Startup programs and commands from key bindings no longer hang before they
  start, which also left the window of a nested Canoe open after it quit.
  The nested window is now titled Canoe. (#101)
- A window that asks to be moved or resized after the button is already up no
  longer leaves the pointer stuck moving or resizing it. (#99)
- Super+drag no longer resizes or moves a fullscreen window. (#106)
- A video put in fullscreen mode in Chrome, e.g. with `f` on YouTube, fills
  the screen right away instead of only after asking a second time. (#105)
- Selecting a desktop icon on one monitor deselects the one selected on
  another, which otherwise stayed highlighted, even when its window was
  restored and minimized again later. (#103)
- Clicks on one monitor, its desktop and icons included, no longer go to a
  fullscreen window on another one, e.g. pausing a video playing there.
  (#104)
- Right-clicking the desktop near its bottom or right edge, where the window
  menu is pushed back onto the screen, no longer picks the window under the
  pointer as soon as the button is released. (#107)
- Allow programs to toggle server-side decorations for windows already shown.
  (#108)
- Windows that draw their own decorations with a shadow, as GTK 4 apps and
  Chromium without the system titlebar do, no longer get a second one from
  Canoe. (#108)
- A window that asks to be moved or resized, as X11 programs without a
  title bar do, no longer leaves the pointer stuck when the button is
  released before canoe gets to the request: clicks reached nothing until
  canoe was restarted. (#109)
- Windows that start out maximized, like Inkscape, show their titlebar on
  the screen, and no longer disappear when unmaximized. (#110)

## [0.6.2] - 2026-11-07

### Fixed

- Snapped and maximized windows follow when a bar is started, stopped or
  resized, instead of covering it or leaving a gap. (#65)

## [0.6.1] - 2026-11-07

### Fixed

- Maximized and snapped windows no longer land offset to the side on outputs
  that are not at the origin, e.g. after kanshi moves an output. (#64)

## [0.6.0] - 2026-11-07

### Added

- Screen sharing through xdg-desktop-portal-wlr. `canoe --share-picker` is the
  portal's chooser: it dims all outputs and highlights the window or output
  under the pointer; a click shares it, Tab and the arrow keys cycle through the
  candidates, Return confirms, and Escape or a right click declines. Chrome's
  second request for the same share is answered with the first pick. On startup,
  canoe exports `WAYLAND_DISPLAY` and `XDG_CURRENT_DESKTOP` to the systemd and
  D-Bus activation environment so the portal finds the session;
  `update_activation_environment = false` turns that off. (#49)
- Decorations, shadows, the window menu and the desktop are drawn at the
  output's fractional scale, through `wp_viewporter` and
  `wp_fractional_scale_v1`, with frame edges rounded the way wlroots rounds
  them. (#48)
- `startup_cmds` and an `[env]` table in `canoe.toml` declare commands to run at
  startup and environment variables for launched programs. (#34, thanks
  @Morxemplum)
- A sample `canoe.toml` listing every option with its built-in default, plus an
  example theme. (#35, thanks @Morxemplum)
- Per-glyph font fallback through fontconfig, so CJK and other scripts missing
  from the title font are drawn with a font that has them. (#52, thanks
  @alk3pInjection)
- Text is shaped with rustybuzz, giving kerning and ligatures, and emoji are
  drawn in color when a color emoji font is installed. Emoji in inactive
  titlebars and unselected items are tinted to recede along with the text. (#59,
  thanks @datfooldive)
- XCursor theming: `xcursor_theme` and `xcursor_size` set the cursor theme for
  River and for launched and D-Bus-activated programs. (#55, thanks @alvv-z)
- `ui.desktop_menu = false` lets clicks on the desktop outside of icons fall
  through to wallpaper clients, and `"transparent"` is accepted as a color.
  (#54, thanks @alvv-z)
- New windows ask to stay within the usable area of their output and are placed
  so that they fit it. (#56, thanks @alvv-z)
- An example session file for display managers, `contrib/canoe.desktop`.

### Changed

- Sizes in the config are logical pixels at any scale, and the default chrome is
  larger: `font_size` 15, `border_width` 5, shadows 25 (active) and 12
  (inactive). (#48)
- The desktop background fill and the desktop icons are separate layer surfaces:
  the fill sits below wallpaper clients, the icons above them. (#54, thanks
  @alvv-z)
- Windows follow output changes: windows on a removed output move to the nearest
  remaining one, and windows that no longer fit an output after a resolution or
  scale change are moved back onto it. Snapped and maximized windows are snapped
  again to the new size. (#56, thanks @alvv-z)
- Rasterized glyphs are cached, making text drawing about ten times faster.
  (#59)

### Fixed

- Child dialogs can be resized. (#27)
- Alt-Tab cycles through windows in the right direction. (#51, thanks
  @alk3pInjection)
- Layer-shell clients such as fuzzel open on the current output. (#53, thanks
  @alk3pInjection)
- Focus moves to a visible window when the focused window closes. (#57, thanks
  @datfooldive)
- Keyboard focus is restored after unlocking the session. (#58)
- Bars created before the desktop surface get their clicks again. (#60)
- Starting a second canoe in a running session no longer breaks screen sharing.
  (#61)
- New windows no longer open at the top-left corner after the last focused
  output is removed. (#62)

## [0.5.1] - 2026-06-09

### Fixed

- Show window decorations for clients that don't negotiate them. (#26)
- Adjust window sizes to meet client preferences. (#25)

## [0.5.0] - 2026-06-06

### Added

- New and improved styling for dialogs and non-resizable windows. (#17)
- Custom hotkeys. (#23)
- `--no-config`/`-n` ignores the config file. (#19)
- Reload the config on `SIGHUP`. (#20)

### Changed

- Faster rendering of drop shadows. (#18)
- Faster window resizing through page-faulted, double-buffered shm pools.
  (#22)
- Esc cancels window switching, and the window menu keeps keyboard focus.
  (#24)

### Fixed

- Shadows are rendered on non-Linux systems. (#21)

## [0.4.0] - 2026-05-28

### Added

- Minimized windows show as icons on the desktop. (#4)
- Window shadows. (#2)
- Super+L locks the screen; the locker and terminal are configurable. (#16)
- OpenBSD support. (#9)

### Fixed

- Windows can switch their CSD preference at runtime. (#11)
- Default color of inactive window titles. (#12)
- Dragging on the desktop no longer moves or resizes the active window. (#13)
- Switching away from a fullscreen window. (#3)
- Right-clicking an icon no longer opens the window selector. (#14)
- Clients requesting fullscreen without a specific output. (#15)
- Linux/ARM64 build. (#10)

## [0.3.1] - 2026-02-11

### Added

- Client-side decorations for clients that ask for them. (#1)
- nix-shell support.

### Changed

- Menu titles are drawn like inactive windows by default.

### Fixed

- Keyboard focus is assigned after closing a window.

## [0.3.0] - 2026-01-28

### Added

- Super+Alt+Arrow sends windows to other screens.
- Super+` cycles between windows of the same app.
- Configurable launcher.
- Menu titles.
- `#rrggbb`, `#rrggbbaa` and `#rgb` colors in the config.

### Changed

- Simpler window rule syntax.

### Fixed

- Snapping and maximizing in multi-screen setups.
- New windows in multi-screen setups.
- The launcher opens on the current screen and restores focus when closed.
- Focus handling for desktop clicks and the window menu.

## [0.2.0] - 2026-01-26

### Added

- Snapping windows to the left and right half of the screen.

### Changed

- Smarter handling of Super+Down.
- Improved tabbing through windows.

### Fixed

- Restoring windows from fullscreen.

## [0.1.1] - 2026-01-26

Initial release.

<!-- next-url -->
[Unreleased]: https://github.com/roblillack/canoe/compare/v0.6.2...HEAD
[0.6.2]: https://github.com/roblillack/canoe/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/roblillack/canoe/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/roblillack/canoe/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/roblillack/canoe/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/roblillack/canoe/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/roblillack/canoe/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/roblillack/canoe/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/roblillack/canoe/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/roblillack/canoe/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/roblillack/canoe/releases/tag/v0.1.1
