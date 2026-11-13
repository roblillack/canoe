# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While pre-1.0, the minor version is bumped for breaking changes.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### [A Canoe without a River](https://github.com/roblillack/canoe/discussions/83)

Canoe is now a Wayland compositor of its own, built on Smithay, instead of a
window manager inside River. Start it with `canoe` from a text console or
pick the Canoe session in your display manager; River is no longer needed.
Your configuration, rules and themes carry over, and X11 programs run through
Xwayland. (#66)

### Added

- `xdg-toplevel-icon-v1`: a minimized window's desktop icon is the icon its
  program sets, unless one is put in `~/.config/canoe/icons/`. (#67)
- KDE's `org_kde_plasma_shell` protocol, of which only `set_skip_switcher`
  is honored: windows that ask for it are left out of the window menu,
  Super+Tab and focus cycling, while their desktop icon still shows when
  minimized. Other requests are accepted and ignored. (#70)
- Super+D minimizes all windows on all screens, like on Windows. Pressing it
  again while no window is shown brings them back, in their old stacking
  order and with the focus they had. (#72)
- Cursor sessions of `ext-image-copy-capture-v1` report where the pointer is
  on the output or window they capture, so programs like eyes following the
  pointer work. Their cursor image stays empty. (#75)
- Windows glide and stretch into place when they are maximized, snapped to
  a half of the screen or restored, instead of jumping there. The frame,
  titlebar and shadow keep their proportions while the content stretches.
  Minimized windows shrink into their desktop icons and fade out, and grow
  back out of them when brought back. `animations_enabled` and
  `animations_duration` (in milliseconds, 150 by default) under `[ui]` turn
  the animations off or change their speed. (#76)
- X11 programs are crisp on scaled outputs: they draw at the outputs'
  scale, rounded up, and learn it through `Xft.dpi` and XSETTINGS
  (`Gdk/WindowScalingFactor`), as does their default cursor. Programs
  that can't scale come out small; `xwayland_hidpi = false` goes back to
  drawing them at scale 1, stretched and blurry. (#80)
- Screenshot tools made for sway or KWin work with `emulate_sway` and
  `emulate_kwin`. The first answers the questions sway's IPC answers about
  outputs, windows and focus, for shotman and grimshot; the second offers
  KWin's screenshot D-Bus interface, for Spectacle in all its modes, with
  Canoe's picker for the window under the cursor. Both are off by default;
  see Screenshots in the README. (#81)
- `xdg-toplevel-tag-v1`: windows may tag themselves. The tags aren't used
  yet, but KDE programs expect the protocol and Spectacle crashes without
  it. (#81)
- Canoe remembers the output layout for each combination of connected
  monitors and restores it when the combination comes back, so kanshi is no
  longer needed: whatever an output tool like `wdisplays` or `wlr-randr`
  applies is saved, in `~/.local/state/canoe/outputs.toml`.
  `remember_outputs = false` turns this off. (#82)
- A primary output per combination of monitors, where new windows open
  after monitors come or go and where layer surfaces without an output go.
  The pointer starts in its middle.
  It is chosen with an experimental protocol of canoe's own,
  `xx-primary-output-v1`. (#82)
- Monitors report their make, model and serial number from their EDID, so
  output tools can tell them apart. (#82)
- Closing a laptop's lid turns its panel off, and opening it turns the
  panel back on, unless the remembered layout says otherwise: the lid
  open and the lid closed each have a layout of their own. Until one of
  them is set up, the other one is used.
  `xx-primary-output-v1` tells output tools which outputs are a laptop's
  panel and whether the lid is closed over them. (#82)

### Changed

- Window titles are centered in the titlebar, moving aside where the
  buttons are in the way. `titlebar_text_align = "left"` under `[ui]` puts
  them next to the close button again. (#89)
- Windows that only draw their own decorations, as libadwaita apps do, keep
  them instead of getting Canoe's titlebar on top. A `force_ssd` rule still
  gives them one. Every other window is decorated by Canoe, even one that
  would rather draw its own decorations, and GTK 3 and plain GTK 4 apps now
  leave theirs off: they ask to be decorated through KDE's server decoration
  protocol, which Canoe now offers. (#66)
- The window menu and Super+Tab lead each title with the name of its
  application, from its desktop entry: "~: fish" shows as
  "Alacritty — ~: fish", and "Page - Google Chrome" as
  "Google Chrome — Page". The menu of one application's windows has that
  name as its header instead of the app id. Desktop entries are also found
  by lower-case app id and by `StartupWMClass`, for names and icons alike. (#73)
- Optimize memory used for window frames and shadows, and more efficiently
  return it to the system when windows close. (#90)

### Fixed

- Windows are no longer minimized when the last output goes away, e.g.
  while the only monitor wakes up and reconnects; they come back on the
  first output that appears. (#82)
- Cursors keep their shapes where no `default` XCursor theme is installed, as
  on NixOS: Canoe falls back to a theme that is, and tells clients about it.
  With no theme at all, the built-in cursor is now a proper arrow instead of
  a triangle. (#88)
- X11 programs take clicks and notice the pointer on outputs placed above or
  left of others (at negative positions, e.g. by kanshi). Such a layout is
  moved so it starts at 0,0, as Xwayland's screen does. (#66)
- X11 programs run as root (e.g. `sudo xterm`) can connect to Xwayland. (#66)
- The first click after moving or resizing a window by its own (client-side)
  decorations, as in GNOME Calculator, reaches what it is on instead of that
  window. (#66)
- The desktop icon of a minimized window disappears when the window closes,
  instead of staying until it is clicked. (#69)
- The desktop icon of a minimized window shows its new title as soon as the
  window changes it, instead of once the icon is clicked or Super+Tab
  opened. (#74)
- A second screen locker, e.g. from an idle daemon, is refused while the
  session is locked, instead of taking over and stranding the first. (#77)
- Monitors that come and go while another VT has the display are set up again
  on switching back, instead of staying dark until replugged. (#77)
- Clients like kitty no longer crash when a monitor is unplugged or turned
  off. (#77)
- Focusing a window only takes fullscreen from windows on its own output, so a
  program can cover every output with a fullscreen window of its own (as
  Spectacle does to select a region). A new window is focused once what it
  asked for before showing up, like fullscreen on a given output, is done. (#81)
- Windows that ask to be maximized or fullscreen before they first show up, as
  Qt programs do, start out that way. Canoe answered such a request at once,
  which kept it from choosing how they start, so they could start
  windowed. (#81)

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
