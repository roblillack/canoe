//! Configuration for the River window manager

#![allow(dead_code)]

use crate::rule::Rule;
use regex::Regex;
use serde::de::{self, Deserializer};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Mouse button codes (Linux input event codes)
pub mod button {
    pub const LEFT: u32 = 0x110;
    pub const RIGHT: u32 = 0x111;
    pub const MIDDLE: u32 = 0x112;
}

/// Modifier key masks
pub mod modifiers {
    pub const NONE: u32 = 0;
    pub const SHIFT: u32 = 1;
    pub const CTRL: u32 = 4;
    pub const ALT: u32 = 8; // mod1
    pub const MOD3: u32 = 32;
    pub const SUPER: u32 = 64; // mod4
    pub const MOD5: u32 = 128;
}

/// Main modifier key used for default bindings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MainModifier {
    Alt,
    #[default]
    Super,
}

impl MainModifier {
    pub fn mask(self) -> u32 {
        match self {
            Self::Alt => modifiers::ALT,
            Self::Super => modifiers::SUPER,
        }
    }
}

/// Input mode for bindings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    Lock,
    #[default]
    Default,
    Floating,
    Passthrough,
    DesktopIcons,
    SharePicker,
}

/// Window decoration style
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowDecoration {
    Csd,
    #[default]
    Ssd,
}

/// Where the title sits in a window's titlebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TitleAlign {
    /// Next to the close button.
    Left,
    /// In the middle of the titlebar, moved aside as far as needed to stay
    /// clear of the buttons.
    #[default]
    Center,
}

/// Border colors
#[derive(Debug, Clone, Copy)]
pub struct BorderColor {
    pub focus: u32,
    pub unfocus: u32,
    pub urgent: u32,
}

impl Default for BorderColor {
    fn default() -> Self {
        Self {
            focus: 0xffff00ff,   // Bright yellow, fully opaque
            unfocus: 0x888800ff, // Dark yellow/olive for unfocused
            urgent: 0xff0000ff,
        }
    }
}

/// Layered border colors (outer, mid, inner)
#[derive(Debug, Clone, Copy)]
pub struct BorderLayers {
    pub outer: u32,
    pub mid: u32,
    pub inner: u32,
}

impl Default for BorderLayers {
    fn default() -> Self {
        Self {
            outer: 0x000000FF,
            mid: 0xC0C0C0FF,
            inner: 0x000000FF,
        }
    }
}

/// XCursor theme configuration. `name` and `size` are `None` when nothing
/// sets them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XCursorTheme {
    pub name: Option<String>,
    pub size: Option<u32>,
}

impl XCursorTheme {
    /// Theme name, falling back to `"default"`.
    pub fn name_or_default(&self) -> &str {
        self.name.as_deref().unwrap_or(DEFAULT_XCURSOR_THEME)
    }

    /// Cursor size, falling back to 24.
    pub fn size_or_default(&self) -> u32 {
        self.size.unwrap_or(DEFAULT_XCURSOR_SIZE)
    }
}

/// UI theme configuration
#[derive(Debug, Clone)]
pub struct UiConfig {
    pub border_width: i32,
    pub border_active: BorderLayers,
    pub border_inactive: BorderLayers,
    pub titlebar_text_active: u32,
    pub titlebar_text_inactive: u32,
    pub titlebar_bg_active: u32,
    pub titlebar_bg_inactive: u32,
    pub titlebar_text_align: TitleAlign,
    pub menu_bg: u32,
    pub menu_text: u32,
    pub menu_highlight_bg: u32,
    pub menu_highlight_text: u32,
    pub button_bg: u32,
    pub button_highlight: u32,
    pub button_shadow: u32,
    /// Master toggle for all window/menu drop shadows.
    pub shadows_enabled: bool,
    /// Soft shadow size for the focused window.
    pub shadows_active_size: i32,
    /// Soft shadow size for non-focused windows.
    pub shadows_inactive_size: i32,
    /// Shadow color for windows. Menus use the soft window shadow style
    /// (sized by [`shadows_active_size`]) when shadows are enabled, and fall
    /// back to a built-in retro L-shape drop shadow otherwise.
    pub shadows_color: u32,
    pub font_name: Option<String>,
    pub font_size: f32,
    /// Fill color for the desktop background surface (0xRRGGBBAA), drawn on
    /// the background layer. Wallpaper clients (swaybg, shaderbg) started
    /// after Canoe stack above it, but one already running when the surface
    /// is created ends up below it, so their users should configure
    /// `"transparent"` (alpha 0), which draws no fill.
    pub desktop_background: u32,
    /// Whether right-clicking empty desktop opens the window menu. When
    /// `false`, the desktop icon surface only captures input on minimized-
    /// window icons; all other pointer input falls through to wallpaper
    /// clients (e.g. shaderbg) below it.
    pub desktop_menu: bool,
    /// Whether minimized-window icons are shown on the desktop.
    pub icons_enabled: bool,
    /// Font for icon labels. When `None`, falls back to a regular-weight
    /// variant of [`UiConfig::font_name`].
    pub icons_font_name: Option<String>,
    /// Font size for icon labels. When `None`, falls back to
    /// [`UiConfig::font_size`] * 0.80.
    pub icons_font_size: Option<f32>,
    /// Label color for non-selected icons. When `None`, falls back to
    /// [`UiConfig::menu_text`].
    pub icons_text: Option<u32>,
    /// Background for the selected icon and its label. When `None`, falls
    /// back to [`UiConfig::menu_highlight_bg`].
    pub icons_highlight_bg: Option<u32>,
    /// Text color for the selected icon and its label. When `None`, falls
    /// back to [`UiConfig::menu_highlight_text`].
    pub icons_highlight_text: Option<u32>,
    /// Whether windows glide and stretch into place when they are
    /// maximized, snapped or restored.
    pub animations_enabled: bool,
    /// How long those animations take, in milliseconds; 0 turns them off.
    pub animations_duration: u32,
}

/// Longest animation duration accepted from the config file, in
/// milliseconds.
const MAX_ANIMATION_DURATION: u32 = 5000;

impl UiConfig {
    /// How long window animations take, or `None` if they are off.
    pub fn animation_duration(&self) -> Option<std::time::Duration> {
        (self.animations_enabled && self.animations_duration > 0)
            .then(|| std::time::Duration::from_millis(self.animations_duration.into()))
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            border_width: 5,
            border_active: BorderLayers::default(),
            border_inactive: BorderLayers::default(),
            titlebar_text_active: 0xFFFFFFFF,
            titlebar_text_inactive: 0x000000FF,
            titlebar_bg_active: 0x000080FF,
            titlebar_bg_inactive: 0xFFFFFFFF,
            titlebar_text_align: TitleAlign::Center,
            menu_bg: 0xC0C0C0FF,
            menu_text: 0x000000FF,
            menu_highlight_bg: 0x000080FF,
            menu_highlight_text: 0xFFFFFFFF,
            button_bg: 0xC0C0C0FF,
            button_highlight: 0xFFFFFFFF,
            button_shadow: 0x808080FF,
            shadows_enabled: true,
            shadows_active_size: 25,
            shadows_inactive_size: 12,
            shadows_color: 0x00000033,
            font_name: None,
            font_size: 15.0,
            desktop_background: 0x008080FF,
            desktop_menu: true,
            icons_enabled: true,
            icons_font_name: None,
            icons_font_size: None,
            icons_text: None,
            icons_highlight_bg: None,
            icons_highlight_text: None,
            animations_enabled: true,
            animations_duration: 150,
        }
    }
}

/// Main configuration structure
#[derive(Debug, Clone)]
pub struct Config {
    pub env: HashMap<String, String>,
    pub working_directory: Option<String>,
    pub startup_cmds: Vec<Vec<String>>,
    pub launcher_cmd: Vec<String>,
    pub terminal_cmd: Vec<String>,
    pub lock_cmd: Vec<String>,
    /// Export WAYLAND_DISPLAY, XDG_CURRENT_DESKTOP and, if set, the XCursor
    /// theme to systemd/D-Bus activated services (e.g. the screencast portal)
    /// on startup.
    pub update_activation_environment: bool,
    /// XCursor theme name from the config file.
    pub xcursor_theme: Option<String>,
    /// XCursor size from the config file.
    pub xcursor_size: Option<u32>,
    /// Let X11 programs draw at the outputs' scale (crisp, but programs
    /// that can't scale come out small) instead of drawing them at scale 1
    /// and stretching them (blurry).
    pub xwayland_hidpi: bool,
    /// Answer sway's IPC (`SWAYSOCK`) with what canoe shows, for tools that
    /// ask sway where windows and outputs are (shotman, grimshot).
    pub emulate_sway: bool,
    /// Offer KWin's screenshot D-Bus API (org.kde.KWin.ScreenShot2), the
    /// only way Spectacle takes screenshots on Wayland.
    pub emulate_kwin: bool,
    /// Remember how the outputs are set up for each combination of
    /// monitors and set them up that way again when it comes back.
    pub remember_outputs: bool,
    pub main_modifier: MainModifier,

    pub repeat_rate: i32,
    pub repeat_delay: i32,
    pub scroll_factor: f64,
    pub sloppy_focus: bool,

    pub border_color: BorderColor,
    pub ui: UiConfig,

    pub rules: Vec<Rule>,
    pub hotkeys: Vec<Hotkey>,
}

/// A user-configured global hotkey: a key chord that spawns a command.
#[derive(Debug, Clone)]
pub struct Hotkey {
    /// XKB keysym of the (unshifted) key.
    pub keysym: u32,
    /// Required modifier mask (see [`modifiers`]).
    pub modifiers: u32,
    /// Command to spawn, as an argv vector (first element is the program).
    pub argv: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            env: HashMap::new(),
            working_directory: dirs::home_dir().map(|p| p.to_string_lossy().to_string()),
            startup_cmds: Vec::new(),
            launcher_cmd: vec!["fuzzel".to_string()],
            terminal_cmd: vec!["foot".to_string()],
            lock_cmd: vec!["swaylock".to_string()],
            update_activation_environment: true,
            xcursor_theme: None,
            xcursor_size: None,
            xwayland_hidpi: true,
            emulate_sway: false,
            emulate_kwin: false,
            remember_outputs: true,
            main_modifier: MainModifier::default(),

            repeat_rate: 50,
            repeat_delay: 300,
            scroll_factor: 1.0,
            sloppy_focus: false,

            border_color: BorderColor::default(),
            ui: UiConfig::default(),

            rules: default_rules(),
            hotkeys: Vec::new(),
        }
    }
}

/// XCursor theme used when nothing else names one.
const DEFAULT_XCURSOR_THEME: &str = "default";
/// XCursor size used when nothing else sets one.
const DEFAULT_XCURSOR_SIZE: u32 = 24;

impl Config {
    /// Resolved XCursor theme. Name and size are resolved independently:
    /// config file, then `XCURSOR_THEME`/`XCURSOR_SIZE` as programs launched
    /// by Canoe see them (the `[env]` table overriding Canoe's own
    /// environment). Either stays `None` if none of these sets it.
    pub fn xcursor_theme_resolved(&self) -> XCursorTheme {
        self.xcursor_theme_resolved_with(|name| std::env::var(name).ok())
    }

    /// [`Self::xcursor_theme_resolved`] with Canoe's own environment read
    /// through `lookup`.
    fn xcursor_theme_resolved_with(&self, lookup: impl Fn(&str) -> Option<String>) -> XCursorTheme {
        let env = |name: &str| {
            self.env
                .get(name)
                .cloned()
                .or_else(|| lookup(name))
                .filter(|value| !value.is_empty())
        };
        XCursorTheme {
            name: self.xcursor_theme.clone().or_else(|| env("XCURSOR_THEME")),
            size: self
                .xcursor_size
                .or_else(|| env("XCURSOR_SIZE").and_then(|s| s.trim().parse().ok()))
                .filter(|&size| size > 0),
        }
    }

    /// Point `xcursor_theme` at a theme that is really installed, when the
    /// resolved name (config file, `[env]`, `XCURSOR_THEME`, or the `default`
    /// theme) names one that cannot be loaded at all. Systems that ship no
    /// `default` theme — NixOS, minimal installs, containers — would otherwise
    /// end up with Canoe's built-in arrow for every cursor shape.
    ///
    /// The name is written back into the config so everything follows it: the
    /// cursors Canoe draws, `XCURSOR_THEME` for the programs it launches and
    /// for the session bus, and the XSETTINGS Xwayland hands to X11 programs.
    /// A theme that does load is never second-guessed.
    pub fn resolve_installed_xcursor_theme(&mut self) {
        let wanted = self.xcursor_theme_resolved().name_or_default().to_string();
        if xcursor_theme_loads(&wanted) {
            return;
        }
        let installed = installed_xcursor_themes(&xcursor_search_paths());
        // Well-known themes first, then whatever else is installed.
        let preferred: Vec<String> = FALLBACK_XCURSOR_THEMES
            .iter()
            .filter(|name| installed.iter().any(|found| found == *name))
            .map(|name| name.to_string())
            .collect();
        let rest = installed
            .into_iter()
            .filter(|name| !preferred.iter().any(|preferred| preferred == name));
        match preferred
            .iter()
            .cloned()
            .chain(rest)
            .find(|name| xcursor_theme_loads(name))
        {
            Some(name) => {
                eprintln!("canoe: cursor theme {wanted:?} is not installed, using {name:?}");
                self.xcursor_theme = Some(name);
            }
            None => eprintln!(
                "canoe: no cursor theme installed (looked for {wanted:?}), \
                 drawing the built-in arrow"
            ),
        }
    }
}

/// Cursor themes to fall back to, in order of preference, when nothing names
/// one that is installed.
const FALLBACK_XCURSOR_THEMES: &[&str] = &[
    "Adwaita",
    "breeze_cursors",
    "Bibata-Modern-Classic",
    "capitaine-cursors",
    "DMZ-White",
    "Vanilla-DMZ",
];

/// Whether `name` resolves to a theme an arrow can be loaded from, following
/// the theme's `Inherits` like every XCursor loader does: a bare `default`
/// theme that only inherits another one counts as loadable.
fn xcursor_theme_loads(name: &str) -> bool {
    let theme = xcursor::CursorTheme::load(name);
    ["default", "left_ptr"]
        .iter()
        .any(|icon| theme.load_icon(icon).is_some())
}

/// The names of the themes with cursors in them under `paths`, in search path
/// order and sorted within each directory (`read_dir` order is arbitrary).
fn installed_xcursor_themes(paths: &[PathBuf]) -> Vec<String> {
    let mut themes: Vec<String> = Vec::new();
    for path in paths {
        let Ok(entries) = std::fs::read_dir(path) else {
            continue;
        };
        let mut here: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().join("cursors").is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| !themes.contains(name))
            .collect();
        here.sort();
        themes.extend(here);
    }
    themes
}

/// The directories XCursor themes are looked up in, the way libXcursor,
/// libwayland-cursor and the `xcursor` crate do it: `XCURSOR_PATH`, or else
/// the XDG data directories.
fn xcursor_search_paths() -> Vec<PathBuf> {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    xcursor_search_paths_from(
        env("HOME").map(PathBuf::from),
        env("XCURSOR_PATH"),
        env("XDG_DATA_HOME"),
        env("XDG_DATA_DIRS"),
    )
}

/// [`xcursor_search_paths`] with the environment passed in, for tests.
fn xcursor_search_paths_from(
    home: Option<PathBuf>,
    xcursor_path: Option<String>,
    xdg_data_home: Option<String>,
    xdg_data_dirs: Option<String>,
) -> Vec<PathBuf> {
    let entries = |list: &str| -> Vec<PathBuf> {
        list.split(':')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect()
    };
    // XCURSOR_PATH, where set, replaces the whole search path.
    if let Some(ref path) = xcursor_path {
        return entries(path);
    }

    let mut paths = Vec::new();
    match xdg_data_home {
        Some(ref dir) => paths.push(PathBuf::from(dir).join("icons")),
        None => paths.extend(home.as_ref().map(|home| home.join(".local/share/icons"))),
    }
    paths.extend(home.as_ref().map(|home| home.join(".icons")));
    match xdg_data_dirs {
        Some(ref dirs) => paths.extend(entries(dirs).into_iter().map(|dir| dir.join("icons"))),
        None => paths.extend(
            ["/usr/local/share/icons", "/usr/share/icons"]
                .iter()
                .map(PathBuf::from),
        ),
    }
    paths.push(PathBuf::from("/usr/share/pixmaps"));
    paths.extend(home.as_ref().map(|home| home.join(".cursors")));
    paths.push(PathBuf::from("/usr/share/cursors/xorg-x11"));
    paths
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    env: Option<HashMap<String, String>>,
    main_modifier: Option<MainModifier>,
    xcursor_theme: Option<String>,
    xcursor_size: Option<u32>,
    xwayland_hidpi: Option<bool>,
    emulate_sway: Option<bool>,
    emulate_kwin: Option<bool>,
    remember_outputs: Option<bool>,
    launcher_cmd: Option<StringOrVec>,
    terminal_cmd: Option<StringOrVec>,
    lock_cmd: Option<StringOrVec>,
    startup_cmds: Option<Vec<StringOrVec>>,
    update_activation_environment: Option<bool>,
    ui: Option<UiConfigFile>,
    rules: Option<Vec<RuleFile>>,
    /// Map of key chord (e.g. `"Super+I"`) to the command it spawns.
    hotkeys: Option<HashMap<String, StringOrVec>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringOrVec {
    String(String),
    Vec(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct RuleFile {
    match_app_id: Option<StringOrVec>,
    match_app_id_prefix: Option<StringOrVec>,
    match_props: Option<StringOrVec>,
    title: Option<StringOrVec>,
    app_id_regex: Option<String>,
    title_regex: Option<String>,
    force_ssd: Option<bool>,
    swallow_top: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct UiConfigFile {
    border_width: Option<i32>,
    border_active: Option<BorderLayersFile>,
    border_inactive: Option<BorderLayersFile>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    titlebar_text_active: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    titlebar_text_inactive: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    titlebar_bg_active: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    titlebar_bg_inactive: Option<u32>,
    titlebar_text_align: Option<TitleAlign>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    menu_bg: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    menu_text: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    menu_highlight_bg: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    menu_highlight_text: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    button_bg: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    button_highlight: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    button_shadow: Option<u32>,
    shadows_enabled: Option<bool>,
    shadows_active_size: Option<i32>,
    shadows_inactive_size: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    shadows_color: Option<u32>,
    font_name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_f32")]
    font_size: Option<f32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    desktop_background: Option<u32>,
    desktop_menu: Option<bool>,
    icons_enabled: Option<bool>,
    icons_font_name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_f32")]
    icons_font_size: Option<f32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    icons_text: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    icons_highlight_bg: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    icons_highlight_text: Option<u32>,
    animations_enabled: Option<bool>,
    animations_duration: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct BorderLayersFile {
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    outer: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    mid: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_opt_color")]
    inner: Option<u32>,
}

impl BorderLayers {
    fn apply(&mut self, overrides: BorderLayersFile) {
        if let Some(outer) = overrides.outer {
            self.outer = outer;
        }
        if let Some(mid) = overrides.mid {
            self.mid = mid;
        }
        if let Some(inner) = overrides.inner {
            self.inner = inner;
        }
    }
}

impl UiConfig {
    fn apply(&mut self, overrides: UiConfigFile) {
        if let Some(border_width) = overrides.border_width {
            self.border_width = border_width;
        }
        if let Some(border_active) = overrides.border_active {
            self.border_active.apply(border_active);
        }
        if let Some(border_inactive) = overrides.border_inactive {
            self.border_inactive.apply(border_inactive);
        }
        if let Some(color) = overrides.titlebar_text_active {
            self.titlebar_text_active = color;
        }
        if let Some(color) = overrides.titlebar_text_inactive {
            self.titlebar_text_inactive = color;
        }
        if let Some(color) = overrides.titlebar_bg_active {
            self.titlebar_bg_active = color;
        }
        if let Some(color) = overrides.titlebar_bg_inactive {
            self.titlebar_bg_inactive = color;
        }
        if let Some(align) = overrides.titlebar_text_align {
            self.titlebar_text_align = align;
        }
        if let Some(color) = overrides.menu_bg {
            self.menu_bg = color;
        }
        if let Some(color) = overrides.menu_text {
            self.menu_text = color;
        }
        if let Some(color) = overrides.menu_highlight_bg {
            self.menu_highlight_bg = color;
        }
        if let Some(color) = overrides.menu_highlight_text {
            self.menu_highlight_text = color;
        }
        if let Some(color) = overrides.button_bg {
            self.button_bg = color;
        }
        if let Some(color) = overrides.button_highlight {
            self.button_highlight = color;
        }
        if let Some(color) = overrides.button_shadow {
            self.button_shadow = color;
        }
        if let Some(enabled) = overrides.shadows_enabled {
            self.shadows_enabled = enabled;
        }
        if let Some(size) = overrides.shadows_active_size {
            self.shadows_active_size = size.max(0);
        }
        if let Some(size) = overrides.shadows_inactive_size {
            self.shadows_inactive_size = size.max(0);
        }
        if let Some(color) = overrides.shadows_color {
            self.shadows_color = color;
        }
        if let Some(font_name) = overrides.font_name {
            let trimmed = font_name.trim().to_string();
            self.font_name = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
        }
        if let Some(font_size) = overrides.font_size {
            self.font_size = font_size;
        }
        if let Some(color) = overrides.desktop_background {
            self.desktop_background = color;
        }
        if let Some(enabled) = overrides.desktop_menu {
            self.desktop_menu = enabled;
        }
        if let Some(enabled) = overrides.icons_enabled {
            self.icons_enabled = enabled;
        }
        if let Some(font_name) = overrides.icons_font_name {
            let trimmed = font_name.trim().to_string();
            self.icons_font_name = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
        }
        if let Some(font_size) = overrides.icons_font_size {
            self.icons_font_size = Some(font_size);
        }
        if let Some(color) = overrides.icons_text {
            self.icons_text = Some(color);
        }
        if let Some(color) = overrides.icons_highlight_bg {
            self.icons_highlight_bg = Some(color);
        }
        if let Some(color) = overrides.icons_highlight_text {
            self.icons_highlight_text = Some(color);
        }
        if let Some(enabled) = overrides.animations_enabled {
            self.animations_enabled = enabled;
        }
        if let Some(duration) = overrides.animations_duration {
            self.animations_duration = duration.min(MAX_ANIMATION_DURATION);
        }
    }
}

fn deserialize_opt_f32<'de, D>(deserializer: D) -> Result<Option<f32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<toml::Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(toml::Value::Float(value)) => Ok(Some(value as f32)),
        Some(toml::Value::Integer(value)) => Ok(Some(value as f32)),
        Some(other) => Err(de::Error::custom(format!(
            "expected float or integer, got {}",
            other.type_str()
        ))),
    }
}

fn deserialize_opt_color<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<toml::Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(toml::Value::Integer(value)) => {
            let parsed = u32::try_from(value)
                .map_err(|_| de::Error::custom("expected color integer in range 0..=0xFFFFFFFF"))?;
            Ok(Some(parsed))
        }
        Some(toml::Value::String(value)) => parse_color_str(&value)
            .ok_or_else(|| {
                de::Error::custom("expected color string #rgb, #rrggbb, #rrggbbaa, or transparent")
            })
            .map(Some),
        Some(other) => Err(de::Error::custom(format!(
            "expected string or integer, got {}",
            other.type_str()
        ))),
    }
}

fn parse_color_str(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("transparent") {
        return Some(0x00000000);
    }
    let hex = value.strip_prefix('#')?;
    if !hex.is_ascii() {
        return None;
    }

    match hex.len() {
        3 => {
            let mut chars = hex.chars();
            let r = chars.next()?.to_digit(16)? as u8;
            let g = chars.next()?.to_digit(16)? as u8;
            let b = chars.next()?.to_digit(16)? as u8;
            Some(
                (u32::from(r * 0x11) << 24)
                    | (u32::from(g * 0x11) << 16)
                    | (u32::from(b * 0x11) << 8)
                    | 0xFF,
            )
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some((u32::from(r) << 24) | (u32::from(g) << 16) | (u32::from(b) << 8) | 0xFF)
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some((u32::from(r) << 24) | (u32::from(g) << 16) | (u32::from(b) << 8) | u32::from(a))
        }
        _ => None,
    }
}

fn string_or_vec(value: Option<StringOrVec>) -> Option<Vec<String>> {
    match value {
        Some(StringOrVec::String(s)) => Some(vec![s]),
        Some(StringOrVec::Vec(values)) => Some(values),
        None => None,
    }
}

fn parse_match_props(value: Option<StringOrVec>) -> (Option<bool>, Option<bool>) {
    let props = string_or_vec(value).unwrap_or_default();
    let mut require_csd_only = None;
    let mut require_no_parent = None;

    for prop in props {
        match prop.as_str() {
            "csd_only" => require_csd_only = Some(true),
            "toplevel" => require_no_parent = Some(true),
            _ => {}
        }
    }

    (require_csd_only, require_no_parent)
}

fn clean_cmd_args(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Parse a single modifier token (case-insensitive) into its mask bit.
fn parse_modifier(token: &str) -> Option<u32> {
    use modifiers::*;
    match token.trim().to_ascii_lowercase().as_str() {
        "shift" => Some(SHIFT),
        "ctrl" | "control" => Some(CTRL),
        "alt" | "mod1" => Some(ALT),
        "super" | "logo" | "win" | "mod4" => Some(SUPER),
        "mod3" => Some(MOD3),
        "mod5" => Some(MOD5),
        _ => None,
    }
}

/// Parse a key token into an XKB keysym.
///
/// The lookup is case-insensitive, so a letter like `I` resolves to the
/// unshifted `i` keysym. That matches how the built-in bindings are expressed
/// (a base keysym plus an explicit `Shift` in the modifier mask) and lets a
/// chord fire off the physical key regardless of Shift state.
fn parse_keysym(token: &str) -> Option<u32> {
    use xkbcommon::xkb;
    let raw = xkb::keysym_from_name(token.trim(), xkb::KEYSYM_CASE_INSENSITIVE).raw();
    if raw == xkb::keysyms::KEY_NoSymbol {
        None
    } else {
        Some(raw)
    }
}

/// Parse a chord string such as `"Super+Shift+I"` into `(modifiers, keysym)`.
///
/// The final `+`-separated token is the key; every token before it is a
/// modifier. Returns `None` if any token is unrecognized or the key is missing.
fn parse_chord(chord: &str) -> Option<(u32, u32)> {
    let tokens: Vec<&str> = chord.split('+').map(str::trim).collect();
    let (key, mods) = tokens.split_last()?;
    if key.is_empty() {
        return None;
    }
    let mut modifiers = 0u32;
    for m in mods {
        modifiers |= parse_modifier(m)?;
    }
    Some((modifiers, parse_keysym(key)?))
}

/// Build the hotkey list from the `[hotkeys]` config table, skipping (with a
/// warning) any entry whose chord can't be parsed or whose command is empty.
///
/// A string command is split on whitespace into an argv (so
/// `"control-panel -m display"` runs three arguments); the array form is the
/// explicit alternative for arguments that themselves contain spaces.
fn hotkeys_from_file(hotkeys: HashMap<String, StringOrVec>) -> Vec<Hotkey> {
    let mut parsed = Vec::new();
    for (chord, cmd) in hotkeys {
        let Some((modifiers, keysym)) = parse_chord(&chord) else {
            eprintln!("canoe: ignoring hotkey with unparsable chord {chord:?}");
            continue;
        };
        let argv = match cmd {
            StringOrVec::String(cmd) => cmd.split_whitespace().map(String::from).collect(),
            StringOrVec::Vec(args) => clean_cmd_args(args),
        };
        if argv.is_empty() {
            eprintln!("canoe: ignoring hotkey {chord:?} with empty command");
            continue;
        }
        parsed.push(Hotkey {
            keysym,
            modifiers,
            argv,
        });
    }
    parsed
}

fn compile_regex(value: Option<String>) -> Option<Regex> {
    let pattern = value?;

    Regex::new(&pattern).ok()
}

fn config_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".config").join("canoe").join("canoe.toml"))
}

/// Turn the configured startup commands into argv vectors, dropping empty ones.
fn startup_cmds_from_file(cmds: Vec<StringOrVec>) -> Vec<Vec<String>> {
    cmds.into_iter()
        .filter_map(|cmd| string_or_vec(Some(cmd)))
        .map(clean_cmd_args)
        .filter(|cmd| !cmd.is_empty())
        .collect()
}

fn rules_from_file(rules: Vec<RuleFile>) -> Vec<Rule> {
    rules
        .into_iter()
        .map(|rule| {
            let (require_csd_only, require_no_parent) = parse_match_props(rule.match_props);

            Rule {
                app_id: string_or_vec(rule.match_app_id),
                app_id_prefixes: string_or_vec(rule.match_app_id_prefix),
                title: string_or_vec(rule.title),
                app_id_regex: compile_regex(rule.app_id_regex),
                title_regex: compile_regex(rule.title_regex),
                require_csd_only,
                require_no_parent,
                force_ssd: rule.force_ssd.unwrap_or(false),
                swallow_top: rule.swallow_top,
            }
        })
        .collect()
}

/// Load config from ~/.config/canoe/canoe.toml and apply overrides to defaults.
///
/// When `skip_config` is true the file is never read and the built-in defaults
/// are used (see the `--no-config` command-line flag). Either way the cursor
/// theme is checked against the ones installed, which may fill in a name.
pub fn load_config(skip_config: bool) -> Config {
    let mut config = Config::default();
    if !skip_config {
        if let Some(path) = config_path() {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(file_config) = toml::from_str::<FileConfig>(&contents) {
                    config.apply(file_config);
                }
            }
        }
    }
    // Only now that the file and `[env]` had their say: both feed the name.
    config.resolve_installed_xcursor_theme();

    config
}

impl Config {
    /// Apply the settings present in a parsed config file.
    fn apply(&mut self, file: FileConfig) {
        if let Some(env) = file.env {
            if !env.is_empty() {
                self.env = env
            }
        }
        if let Some(main_modifier) = file.main_modifier {
            self.main_modifier = main_modifier;
        }
        if let Some(name) = file.xcursor_theme {
            let name = name.trim();
            self.xcursor_theme = (!name.is_empty()).then(|| name.to_string());
        }
        if let Some(size) = file.xcursor_size {
            self.xcursor_size = (size > 0).then_some(size);
        }
        if let Some(hidpi) = file.xwayland_hidpi {
            self.xwayland_hidpi = hidpi;
        }
        if let Some(emulate) = file.emulate_sway {
            self.emulate_sway = emulate;
        }
        if let Some(emulate) = file.emulate_kwin {
            self.emulate_kwin = emulate;
        }
        if let Some(remember) = file.remember_outputs {
            self.remember_outputs = remember;
        }
        if let Some(launcher_cmd) = string_or_vec(file.launcher_cmd) {
            let launcher_cmd = clean_cmd_args(launcher_cmd);
            if !launcher_cmd.is_empty() {
                self.launcher_cmd = launcher_cmd;
            }
        }
        if let Some(terminal_cmd) = string_or_vec(file.terminal_cmd) {
            let terminal_cmd = clean_cmd_args(terminal_cmd);
            if !terminal_cmd.is_empty() {
                self.terminal_cmd = terminal_cmd;
            }
        }
        if let Some(lock_cmd) = string_or_vec(file.lock_cmd) {
            let lock_cmd = clean_cmd_args(lock_cmd);
            if !lock_cmd.is_empty() {
                self.lock_cmd = lock_cmd;
            }
        }
        if let Some(startup_cmds) = file.startup_cmds {
            self.startup_cmds = startup_cmds_from_file(startup_cmds);
        }
        if let Some(update) = file.update_activation_environment {
            self.update_activation_environment = update;
        }
        if let Some(ui) = file.ui {
            self.ui.apply(ui);
        }
        if let Some(rules) = file.rules {
            self.rules = rules_from_file(rules);
        }
        if let Some(hotkeys) = file.hotkeys {
            self.hotkeys = hotkeys_from_file(hotkeys);
        }
    }
}

/// Get default window rules
fn default_rules() -> Vec<Rule> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_parsing_match_app_id_and_props() {
        let contents = r#"
            [[rules]]
            match_app_id = ["foot", "kitty"]
            match_props = ["toplevel", "csd_only"]

            [[rules]]
            match_app_id = "alacritty"
            match_props = "toplevel"

            [[rules]]
            match_app_id_prefix = "mate-"
        "#;

        let file_config = toml::from_str::<FileConfig>(contents).expect("parse config");
        let rules = rules_from_file(file_config.rules.expect("rules present"));

        assert_eq!(rules.len(), 3);

        let rule = &rules[0];
        let app_ids = rule.app_id.as_ref().expect("app ids parsed");
        assert!(app_ids.contains(&"foot".to_string()));
        assert!(app_ids.contains(&"kitty".to_string()));
        assert_eq!(rule.require_csd_only, Some(true));
        assert_eq!(rule.require_no_parent, Some(true));

        let rule = &rules[1];
        let app_ids = rule.app_id.as_ref().expect("app ids parsed");
        assert_eq!(app_ids, &vec!["alacritty".to_string()]);
        assert_eq!(rule.require_csd_only, None);
        assert_eq!(rule.require_no_parent, Some(true));

        let rule = &rules[2];
        let prefixes = rule
            .app_id_prefixes
            .as_ref()
            .expect("app id prefixes parsed");
        assert_eq!(prefixes, &vec!["mate-".to_string()]);
        assert_eq!(rule.require_csd_only, None);
        assert_eq!(rule.require_no_parent, None);
    }

    #[test]
    fn test_startup_cmds_and_env_parsing() {
        let contents = r#"
            startup_cmds = [
                "kanshi",
                ["swaybg", " -i ", "/tmp/wall.png", ""],
                "",
                [],
            ]

            [env]
            MOZ_ENABLE_WAYLAND = "1"
            QT_QPA_PLATFORM = "wayland"
        "#;

        let file_config = toml::from_str::<FileConfig>(contents).expect("parse config");

        let startup_cmds =
            startup_cmds_from_file(file_config.startup_cmds.expect("startup_cmds present"));
        assert_eq!(
            startup_cmds,
            vec![
                vec!["kanshi".to_string()],
                vec![
                    "swaybg".to_string(),
                    "-i".to_string(),
                    "/tmp/wall.png".to_string()
                ],
            ]
        );

        let env = file_config.env.expect("env present");
        assert_eq!(env.len(), 2);
        assert_eq!(env.get("MOZ_ENABLE_WAYLAND").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("QT_QPA_PLATFORM").map(String::as_str),
            Some("wayland")
        );
    }

    #[test]
    fn test_env_rejects_non_string_values() {
        let contents = r#"
            [env]
            MOZ_ENABLE_WAYLAND = 1
        "#;

        assert!(toml::from_str::<FileConfig>(contents).is_err());
    }

    #[test]
    fn test_icons_config_parsing() {
        let contents = r##"
            [ui]
            icons_enabled = false
            icons_font_name = "Monospace"
            icons_font_size = 9.5
            icons_text = "#101010"
            icons_highlight_bg = "#FFD000"
            icons_highlight_text = "#000000"
        "##;

        let file_config = toml::from_str::<FileConfig>(contents).expect("parse config");
        let mut ui = UiConfig::default();
        ui.apply(file_config.ui.expect("ui present"));

        assert!(!ui.icons_enabled);
        assert_eq!(ui.icons_font_name.as_deref(), Some("Monospace"));
        assert_eq!(ui.icons_font_size, Some(9.5));
        assert_eq!(ui.icons_text, Some(0x101010FF));
        assert_eq!(ui.icons_highlight_bg, Some(0xFFD000FF));
        assert_eq!(ui.icons_highlight_text, Some(0x000000FF));
    }

    const SAMPLE_CONFIG: &str = include_str!("../contrib/canoe.toml");

    /// Uncomment the settings in the sample config: lines where `#` is
    /// directly followed by text. Explanatory comments start with `# `.
    fn uncomment_settings(contents: &str) -> String {
        contents
            .lines()
            .map(|line| match line.strip_prefix('#') {
                Some(rest) if !rest.is_empty() && !rest.starts_with([' ', '#']) => rest,
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_xcursor_parsing() {
        let parse = |contents: &str| {
            let mut config = Config::default();
            config.apply(toml::from_str(contents).expect("parse config"));
            (config.xcursor_theme, config.xcursor_size)
        };

        assert_eq!(parse(""), (None, None));
        assert_eq!(
            parse("xcursor_theme = \" Adwaita \"\nxcursor_size = 32"),
            (Some("Adwaita".to_string()), Some(32))
        );
        assert_eq!(
            parse("xcursor_theme = \"Adwaita\""),
            (Some("Adwaita".to_string()), None)
        );
        assert_eq!(parse("xcursor_size = 32"), (None, Some(32)));
        // Blank names and zero sizes count as unset.
        assert_eq!(
            parse("xcursor_theme = \"  \"\nxcursor_size = 0"),
            (None, None)
        );
    }

    #[test]
    fn test_xcursor_resolution_order() {
        let own = |name: &str| match name {
            "XCURSOR_THEME" => Some("Own".to_string()),
            "XCURSOR_SIZE" => Some("16".to_string()),
            _ => None,
        };
        let resolve = |config: &Config| {
            let theme = config.xcursor_theme_resolved_with(own);
            (theme.name.expect("name"), theme.size.expect("size"))
        };

        let mut config = Config::default();
        assert_eq!(resolve(&config), ("Own".to_string(), 16));

        // [env] overrides Canoe's own environment ...
        config
            .env
            .insert("XCURSOR_THEME".to_string(), "Env".to_string());
        config
            .env
            .insert("XCURSOR_SIZE".to_string(), "32".to_string());
        assert_eq!(resolve(&config), ("Env".to_string(), 32));

        // ... and the config file settings override both.
        config.xcursor_theme = Some("File".to_string());
        config.xcursor_size = Some(48);
        assert_eq!(resolve(&config), ("File".to_string(), 48));

        // Without any of them, nothing is set and the defaults apply.
        let theme = Config::default().xcursor_theme_resolved_with(|_| None);
        assert_eq!(theme, XCursorTheme::default());
        assert_eq!(
            (theme.name_or_default(), theme.size_or_default()),
            ("default", 24)
        );
    }

    #[test]
    fn test_sample_config_changes_nothing() {
        let file_config: FileConfig = toml::from_str(SAMPLE_CONFIG).expect("sample parses");
        let mut config = Config::default();
        config.apply(file_config);

        let defaults = Config::default();
        assert_eq!(format!("{:?}", config), format!("{:?}", defaults));
    }

    #[test]
    fn test_sample_config_shows_defaults() {
        let contents = uncomment_settings(SAMPLE_CONFIG);
        let file_config: FileConfig = toml::from_str(&contents).expect("uncommented sample parses");
        let mut config = Config::default();
        config.apply(file_config);

        let defaults = Config::default();
        assert_eq!(config.main_modifier, defaults.main_modifier);
        assert_eq!(config.launcher_cmd, defaults.launcher_cmd);
        assert_eq!(config.terminal_cmd, defaults.terminal_cmd);
        assert_eq!(config.lock_cmd, defaults.lock_cmd);
        assert_eq!(
            config.update_activation_environment,
            defaults.update_activation_environment
        );
        assert_eq!(config.emulate_sway, defaults.emulate_sway);
        assert_eq!(config.emulate_kwin, defaults.emulate_kwin);
        assert_eq!(config.remember_outputs, defaults.remember_outputs);

        // The sample spells out the values that UiConfig leaves as `None` and
        // derives at runtime.
        let mut expected_ui = defaults.ui.clone();
        expected_ui.font_name = Some("sans".to_string());
        expected_ui.icons_font_name = Some("sans".to_string());
        expected_ui.icons_font_size = Some(expected_ui.font_size * 0.80);
        expected_ui.icons_text = Some(expected_ui.menu_text);
        expected_ui.icons_highlight_bg = Some(expected_ui.menu_highlight_bg);
        expected_ui.icons_highlight_text = Some(expected_ui.menu_highlight_text);
        assert_eq!(format!("{:?}", config.ui), format!("{:?}", expected_ui));

        // Settings that are empty by default show examples instead.
        assert!(!config.startup_cmds.is_empty());
        assert!(!config.env.is_empty());
        assert!(!config.hotkeys.is_empty());
        assert!(!config.rules.is_empty());
    }

    #[test]
    fn test_animations_config() {
        let ui = UiConfig::default();
        assert_eq!(
            ui.animation_duration(),
            Some(std::time::Duration::from_millis(150))
        );

        let parse = |contents: &str| {
            let file_config = toml::from_str::<FileConfig>(contents).expect("parse config");
            let mut ui = UiConfig::default();
            ui.apply(file_config.ui.expect("ui present"));
            ui
        };
        let ui = parse("[ui]\nanimations_duration = 250\n");
        assert_eq!(
            ui.animation_duration(),
            Some(std::time::Duration::from_millis(250))
        );
        assert_eq!(
            parse("[ui]\nanimations_enabled = false\n").animation_duration(),
            None
        );
        assert_eq!(
            parse("[ui]\nanimations_duration = 0\n").animation_duration(),
            None
        );
        assert_eq!(
            parse("[ui]\nanimations_duration = 60000\n").animations_duration,
            MAX_ANIMATION_DURATION
        );
        assert!(toml::from_str::<FileConfig>("[ui]\nanimations_duration = -1\n").is_err());
    }

    #[test]
    fn test_sample_theme_parses() {
        let contents = include_str!("../contrib/themes/yellow.toml");
        let file_config: FileConfig = toml::from_str(contents).expect("theme parses");
        assert!(file_config.ui.is_some());
    }

    #[test]
    fn test_titlebar_text_align() {
        let parse = |contents: &str| {
            let file_config = toml::from_str::<FileConfig>(contents)?;
            let mut ui = UiConfig::default();
            if let Some(overrides) = file_config.ui {
                ui.apply(overrides);
            }
            Ok::<_, toml::de::Error>(ui.titlebar_text_align)
        };

        assert_eq!(parse("").unwrap(), TitleAlign::Center);
        assert_eq!(
            parse("[ui]\ntitlebar_text_align = \"left\"").unwrap(),
            TitleAlign::Left
        );
        assert_eq!(
            parse("[ui]\ntitlebar_text_align = \"center\"").unwrap(),
            TitleAlign::Center
        );
        assert!(parse("[ui]\ntitlebar_text_align = \"middle\"").is_err());
    }

    #[test]
    fn test_icons_config_defaults() {
        let ui = UiConfig::default();
        assert!(ui.icons_enabled);
        assert!(ui.icons_font_name.is_none());
        assert!(ui.icons_font_size.is_none());
        assert!(ui.icons_text.is_none());
        assert!(ui.icons_highlight_bg.is_none());
        assert!(ui.icons_highlight_text.is_none());
    }

    #[test]
    fn test_shadow_config_parsing() {
        let contents = r##"
            [ui]
            shadows_enabled = false
            shadows_active_size = 24
            shadows_inactive_size = 6
            shadows_color = "#11223344"
        "##;

        let file_config = toml::from_str::<FileConfig>(contents).expect("parse config");
        let mut ui = UiConfig::default();
        ui.apply(file_config.ui.expect("ui present"));

        assert!(!ui.shadows_enabled);
        assert_eq!(ui.shadows_active_size, 24);
        assert_eq!(ui.shadows_inactive_size, 6);
        assert_eq!(ui.shadows_color, 0x11223344);
    }

    #[test]
    fn test_shadow_config_defaults() {
        let ui = UiConfig::default();
        assert!(ui.shadows_enabled);
        assert_eq!(ui.shadows_active_size, 25);
        assert_eq!(ui.shadows_inactive_size, 12);
        assert_eq!(ui.shadows_color, 0x00000033);
    }

    #[test]
    fn test_shadow_size_clamped_to_zero() {
        let contents = r#"
            [ui]
            shadows_active_size = -5
            shadows_inactive_size = -1
        "#;

        let file_config = toml::from_str::<FileConfig>(contents).expect("parse config");
        let mut ui = UiConfig::default();
        ui.apply(file_config.ui.expect("ui present"));

        assert_eq!(ui.shadows_active_size, 0);
        assert_eq!(ui.shadows_inactive_size, 0);
    }

    #[test]
    fn test_chord_parsing() {
        use modifiers::*;
        // Single-letter keys normalize to the unshifted keysym; modifiers OR together.
        assert_eq!(parse_chord("Super+I"), Some((SUPER, 0x69))); // 'i'
        assert_eq!(parse_chord("Super+A"), Some((SUPER, 0x61))); // 'a'
        assert_eq!(parse_chord("Super+Shift+T"), Some((SUPER | SHIFT, 0x74))); // 't'
                                                                               // Named keys and modifier aliases resolve too.
        assert_eq!(parse_chord("Super+Return"), Some((SUPER, 0xFF0D)));
        assert_eq!(parse_chord("Ctrl+Alt+space"), Some((CTRL | ALT, 0x20)));
        assert_eq!(parse_chord("mod4+i"), Some((SUPER, 0x69)));
        // Unknown modifier, unknown key, and missing key are all rejected.
        assert!(parse_chord("Hyper+x").is_none());
        assert!(parse_chord("Super+NotAKey").is_none());
        assert!(parse_chord("Super+").is_none());
    }

    #[test]
    fn test_hotkey_parsing() {
        use modifiers::*;
        let contents = r#"
            [hotkeys]
            "Super+I" = "control-panel"
            "Super+D" = "control-panel -m display"
            "Super+P" = ["control-panel", "-m", "display"]
            "Super+Shift+T" = "foot"
            "Super+Bogus++" = "nope"
        "#;

        let file_config = toml::from_str::<FileConfig>(contents).expect("parse config");
        let hotkeys = hotkeys_from_file(file_config.hotkeys.expect("hotkeys present"));

        // The malformed "Super+Bogus++" entry is dropped; four valid ones remain.
        assert_eq!(hotkeys.len(), 4);

        let argv_of = |keysym: u32| -> Vec<String> {
            hotkeys
                .iter()
                .find(|h| h.keysym == keysym)
                .unwrap_or_else(|| panic!("hotkey for keysym {keysym:#x} bound"))
                .argv
                .clone()
        };
        let display_argv = vec![
            "control-panel".to_string(),
            "-m".to_string(),
            "display".to_string(),
        ];

        // A bare string is a single-element argv; a string with args splits on
        // whitespace, producing the same argv as the explicit array form.
        assert_eq!(argv_of(0x69), vec!["control-panel".to_string()]); // Super+I
        assert_eq!(argv_of(0x64), display_argv); // Super+D, string split
        assert_eq!(argv_of(0x70), display_argv); // Super+P, array form
        assert_eq!(argv_of(0x74), vec!["foot".to_string()]); // Super+Shift+T

        let cp = hotkeys.iter().find(|h| h.keysym == 0x69).unwrap();
        assert_eq!(cp.modifiers, SUPER);
        let term = hotkeys.iter().find(|h| h.keysym == 0x74).unwrap();
        assert_eq!(term.modifiers, SUPER | SHIFT);
    }

    #[test]
    fn test_ui_color_parsing() {
        let contents = r##"
            [ui]
            menu_bg = "#112233"
            menu_text = "#123"
            titlebar_bg_active = "#11223344"
            desktop_background = "transparent"
        "##;

        let file_config = toml::from_str::<FileConfig>(contents).expect("parse config");
        let ui = file_config.ui.expect("ui present");

        assert_eq!(ui.menu_bg, Some(0x112233FF));
        assert_eq!(ui.menu_text, Some(0x112233FF));
        assert_eq!(ui.titlebar_bg_active, Some(0x11223344));
        assert_eq!(ui.desktop_background, Some(0x00000000));
    }

    #[test]
    fn test_xcursor_search_paths() {
        let home = Some(PathBuf::from("/home/me"));
        let paths =
            |xcursor_path: Option<&str>, data_home: Option<&str>, data_dirs: Option<&str>| {
                xcursor_search_paths_from(
                    home.clone(),
                    xcursor_path.map(str::to_string),
                    data_home.map(str::to_string),
                    data_dirs.map(str::to_string),
                )
            };

        // XCURSOR_PATH replaces the whole search path, empty entries aside.
        assert_eq!(
            paths(Some("/a::/b"), Some("/ignored"), Some("/ignored")),
            [PathBuf::from("/a"), PathBuf::from("/b")]
        );

        // Otherwise the XDG data directories, with the home ones first.
        let found = paths(None, Some("/home/me/.data"), Some("/pkg/share:/share"));
        assert_eq!(
            found[..4],
            [
                PathBuf::from("/home/me/.data/icons"),
                PathBuf::from("/home/me/.icons"),
                PathBuf::from("/pkg/share/icons"),
                PathBuf::from("/share/icons"),
            ]
        );

        // Without XDG_DATA_HOME/XDG_DATA_DIRS, their defaults.
        let found = paths(None, None, None);
        assert_eq!(
            found[..4],
            [
                PathBuf::from("/home/me/.local/share/icons"),
                PathBuf::from("/home/me/.icons"),
                PathBuf::from("/usr/local/share/icons"),
                PathBuf::from("/usr/share/icons"),
            ]
        );
        assert!(found.contains(&PathBuf::from("/home/me/.cursors")));
    }

    #[test]
    fn test_installed_xcursor_themes() {
        let root = std::env::temp_dir().join(format!("canoe-cursor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (first, second) = (root.join("first"), root.join("second"));
        for (dir, theme) in [
            (&first, "Zebra"),
            (&first, "Adwaita"),
            (&second, "Adwaita"),
            (&second, "breeze_cursors"),
        ] {
            std::fs::create_dir_all(dir.join(theme).join("cursors")).expect("make theme");
        }
        // Icon themes without cursors, and a stray file, are not themes.
        std::fs::create_dir_all(first.join("hicolor").join("16x16")).expect("make icons");
        std::fs::write(first.join("index.theme"), "").expect("write file");

        let found = installed_xcursor_themes(&[first.clone(), second.clone(), root.join("gone")]);
        // Search path order, sorted within each directory, without repeats.
        assert_eq!(found, ["Adwaita", "Zebra", "breeze_cursors"]);

        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Helper module for home directory
mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}
