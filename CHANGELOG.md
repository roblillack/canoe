# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While pre-1.0, the minor version is bumped for breaking changes.

<!-- next-header -->

## [Unreleased] - ReleaseDate

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
[Unreleased]: https://github.com/roblillack/canoe/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/roblillack/canoe/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/roblillack/canoe/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/roblillack/canoe/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/roblillack/canoe/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/roblillack/canoe/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/roblillack/canoe/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/roblillack/canoe/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/roblillack/canoe/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/roblillack/canoe/releases/tag/v0.1.1
