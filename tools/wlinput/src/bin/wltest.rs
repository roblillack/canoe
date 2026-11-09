//! wltest: a configurable Wayland test client for window manager tests.
//!
//! Maps one xdg_toplevel (optionally with a parent or a child dialog) or one
//! zwlr_layer_surface_v1, draws it with wl_shm and reports configures and
//! input events on stdout, one line each. With xdg-foreign it can export its
//! window, or attach it to a window another client exported. With
//! xdg-toplevel-icon it sets its window's icon, and can change it later.
//! With canoe's xx-iconlet-v1 it draws that icon with a surface of its own,
//! declares the window an iconlet and reports where its icon is.

use std::io::Write as _;
use std::os::fd::AsFd;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use wayland_client::backend::WaylandError;
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_output::{self, WlOutput},
    wl_pointer::{self, WlPointer},
    wl_registry::WlRegistry,
    wl_seat::{self, WlSeat},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};
use wayland_protocols::xdg::foreign::zv2::client::{
    zxdg_exported_v2::{self, ZxdgExportedV2},
    zxdg_exporter_v2::ZxdgExporterV2,
    zxdg_imported_v2::{self, ZxdgImportedV2},
    zxdg_importer_v2::ZxdgImporterV2,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols::xdg::toplevel_icon::v1::client::{
    xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1, xdg_toplevel_icon_v1::XdgToplevelIconV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use xkbcommon::xkb;

type Result<T> = std::result::Result<T, String>;

/// Client side of canoe's experimental xx-iconlet-v1.
#[allow(clippy::all)]
mod iconlet {
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_protocols::xdg::shell::client::*;

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::xdg::shell::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../src/compositor/protocols/xx-iconlet-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../src/compositor/protocols/xx-iconlet-v1.xml");
}
use iconlet::{
    xx_icon_surface_v1::{self, XxIconSurfaceV1},
    xx_iconic_toplevel_v1::{self, XxIconicToplevelV1},
    xx_iconlet_manager_v1::XxIconletManagerV1,
};

const USAGE: &str = "\
usage: wltest [OPTIONS]

Maps one surface and runs until killed or closed by the compositor.
Connects to $WAYLAND_DISPLAY.

Toplevel (default):
  --title T              window title (UTF-8)
  --retitle T@MS         change the title to T MS after the first configure
                         (repeatable)
  --app-id A             app id (default: wltest)
  --size WxH             requested size (default 400x300)
  --color RRGGBB         fill color (default 3070b0); border is a darker shade
  --min WxH / --max WxH  size limits; --fixed WxH sets size, min and max
  --decoration MODE      none (default: no decoration object), csd, ssd, or
                         unset (create the object, never call set_mode)
  --request fullscreen|maximize   request a state after the first configure
  --request-delay MS     delay before that request (default 0)

Parent / dialog:
  --parent-app-id A, --parent-title T, --parent-size WxH, --parent-color C
                         first map a parent toplevel with these properties,
                         then this window as its child (set_parent)
  --dialog               this window is the parent; also map a child dialog
  --dialog-title T, --dialog-size WxH, --dialog-color C
                         child properties (defaults: dialog, 240x160, d08030)
  In two-window modes output lines are prefixed with [parent] or [child].

xdg-foreign:
  --export FILE          export the window once it is configured and write
                         its handle to FILE
  --import-parent FILE   wait for FILE (up to 10 s), import the handle in it
                         and make that window this window's parent
  --import-delay MS      set that parent MS after the first configure
                         instead of before the first commit

xdg-toplevel-icon (repeatable; applies to the window --title etc. describe):
  --icon ICON[@MS]       set the icon before the first commit, or MS after
                         the first configure. ICON is RRGGBB (images of 24
                         and 48 px, and 96 px at scale 2, in that color with
                         a white top left quarter), name=NAME (an icon
                         theme name), empty (an icon with neither) or none
                         (reset to the default icon)

xx-iconlet-v1 (canoe's; reports icon: shown X Y WxH | icon: hidden |
icon: activated | icon surface: WxH):
  --iconlet              declare the window an iconlet before its first commit
  --icon-surface RRGGBB[@MS]
                         draw the window's icon with a surface of its own, in
                         that color with a white top left quarter, from before
                         the first commit, or in a new color MS after the
                         first configure (repeatable)

Layer surface:
  --layer background|bottom|top|overlay   create a layer surface instead
  --anchor LIST          comma list of top,bottom,left,right
  --size WxH             0 in a dimension means stretch (needs both anchors)
  --exclusive N          exclusive zone (default 0)
  --keyboard none|exclusive|on-demand
  --output NAME          put it on the output with this wl_output name

Output: configure: WxH states=[...] | decoration: client_side|server_side |
  layer configure: WxH | pointer enter X Y | pointer leave |
  pointer button BTN pressed|released X Y | keyboard enter | keyboard leave |
  key SYM pressed|released | closed | exported: HANDLE | import destroyed |
  icon: ICON | title: T";

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Deco {
    None,
    Csd,
    Ssd,
    Unset,
}

#[derive(Clone)]
struct WinSpec {
    title: String,
    app_id: String,
    size: (u32, u32),
    color: u32,
    min: (u32, u32),
    max: (u32, u32),
}

struct Options {
    main: WinSpec,
    deco: Deco,
    request: Option<String>,
    request_delay: u64,
    parent: Option<WinSpec>,
    dialog: Option<WinSpec>,
    layer: Option<zwlr_layer_shell_v1::Layer>,
    anchor: zwlr_layer_surface_v1::Anchor,
    exclusive: i32,
    keyboard: zwlr_layer_surface_v1::KeyboardInteractivity,
    output: Option<String>,
    export: Option<String>,
    import_parent: Option<String>,
    import_delay: Option<u64>,
    /// Icons to set, each with its delay after the first configure (`None`:
    /// before the first commit).
    icons: Vec<(IconSpec, Option<u64>)>,
    /// Titles to change to, each with its delay after the first configure.
    retitles: Vec<(String, u64)>,
    iconlet: bool,
    /// Colors to draw the icon surface in, each with its delay after the
    /// first configure (`None`: from the start).
    icon_surfaces: Vec<(u32, Option<u64>)>,
}

#[derive(Clone, PartialEq)]
enum IconSpec {
    Color(u32),
    Name(String),
    Empty,
    None,
}

impl std::fmt::Display for IconSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IconSpec::Color(c) => write!(f, "{c:06x}"),
            IconSpec::Name(n) => write!(f, "name={n}"),
            IconSpec::Empty => write!(f, "empty"),
            IconSpec::None => write!(f, "none"),
        }
    }
}

fn parse_icon(s: &str) -> Result<(IconSpec, Option<u64>)> {
    let (icon, delay) = match s.rsplit_once('@') {
        Some((icon, ms)) => (
            icon,
            Some(
                ms.parse()
                    .map_err(|_| format!("--icon: bad delay {ms:?}"))?,
            ),
        ),
        None => (s, None),
    };
    let icon = match icon {
        "none" => IconSpec::None,
        "empty" => IconSpec::Empty,
        _ => match icon.strip_prefix("name=") {
            Some(name) => IconSpec::Name(name.into()),
            None => IconSpec::Color(parse_color(icon)?),
        },
    };
    Ok((icon, delay))
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected WxH, got {s:?}"))?;
    let p = |v: &str| v.parse::<u32>().map_err(|_| format!("bad size {s:?}"));
    Ok((p(w)?, p(h)?))
}

fn parse_color(s: &str) -> Result<u32> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return Err(format!("expected RRGGBB, got {s:?}"));
    }
    u32::from_str_radix(s, 16).map_err(|_| format!("bad color {s:?}"))
}

fn parse_args() -> Result<Options> {
    let mut o = Options {
        main: WinSpec {
            title: "wltest".into(),
            app_id: "wltest".into(),
            size: (400, 300),
            color: 0x3070b0,
            min: (0, 0),
            max: (0, 0),
        },
        deco: Deco::None,
        request: None,
        request_delay: 0,
        parent: None,
        dialog: None,
        layer: None,
        anchor: zwlr_layer_surface_v1::Anchor::empty(),
        exclusive: 0,
        keyboard: zwlr_layer_surface_v1::KeyboardInteractivity::None,
        output: None,
        export: None,
        import_parent: None,
        import_delay: None,
        icons: Vec::new(),
        retitles: Vec::new(),
        iconlet: false,
        icon_surfaces: Vec::new(),
    };
    let parent_default = || WinSpec {
        title: "parent".into(),
        app_id: "wltest-parent".into(),
        size: (600, 400),
        color: 0x40a060,
        min: (0, 0),
        max: (0, 0),
    };
    let dialog_default = |main: &WinSpec| WinSpec {
        title: "dialog".into(),
        app_id: main.app_id.clone(),
        size: (240, 160),
        color: 0xd08030,
        min: (0, 0),
        max: (0, 0),
    };
    let mut want_dialog = false;
    let mut dialog_mods: Vec<(String, String)> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        if flag == "--iconlet" {
            o.iconlet = true;
            continue;
        }
        if flag == "--dialog" {
            want_dialog = true;
            continue;
        }
        const VALUED: &[&str] = &[
            "--title",
            "--app-id",
            "--size",
            "--color",
            "--min",
            "--max",
            "--fixed",
            "--decoration",
            "--request",
            "--request-delay",
            "--parent-app-id",
            "--parent-title",
            "--parent-size",
            "--parent-color",
            "--dialog-title",
            "--dialog-size",
            "--dialog-color",
            "--layer",
            "--anchor",
            "--exclusive",
            "--keyboard",
            "--output",
            "--export",
            "--import-parent",
            "--import-delay",
            "--icon",
            "--retitle",
            "--icon-surface",
        ];
        if !VALUED.contains(&flag.as_str()) {
            return Err(format!("unknown option {flag:?} (see --help)"));
        }
        let val = match inline {
            Some(v) => v,
            None => args.next().ok_or_else(|| format!("{flag} needs a value"))?,
        };
        match flag.as_str() {
            "--title" => o.main.title = val,
            "--app-id" => o.main.app_id = val,
            "--size" => o.main.size = parse_size(&val)?,
            "--color" => o.main.color = parse_color(&val)?,
            "--min" => o.main.min = parse_size(&val)?,
            "--max" => o.main.max = parse_size(&val)?,
            "--fixed" => {
                let s = parse_size(&val)?;
                o.main.size = s;
                o.main.min = s;
                o.main.max = s;
            }
            "--decoration" => {
                o.deco = match val.as_str() {
                    "none" => Deco::None,
                    "csd" => Deco::Csd,
                    "ssd" => Deco::Ssd,
                    "unset" => Deco::Unset,
                    _ => return Err(format!("--decoration: none|csd|ssd|unset, got {val:?}")),
                }
            }
            "--request" => match val.as_str() {
                "fullscreen" | "maximize" => o.request = Some(val),
                _ => return Err(format!("--request: fullscreen|maximize, got {val:?}")),
            },
            "--request-delay" => {
                o.request_delay = val.parse().map_err(|_| format!("bad delay {val:?}"))?
            }
            "--parent-app-id" | "--parent-title" | "--parent-size" | "--parent-color" => {
                let p = o.parent.get_or_insert_with(parent_default);
                match flag.as_str() {
                    "--parent-app-id" => p.app_id = val,
                    "--parent-title" => p.title = val,
                    "--parent-size" => p.size = parse_size(&val)?,
                    _ => p.color = parse_color(&val)?,
                }
            }
            "--dialog-title" | "--dialog-size" | "--dialog-color" => {
                want_dialog = true;
                dialog_mods.push((flag, val));
            }
            "--layer" => {
                use zwlr_layer_shell_v1::Layer;
                o.layer = Some(match val.as_str() {
                    "background" => Layer::Background,
                    "bottom" => Layer::Bottom,
                    "top" => Layer::Top,
                    "overlay" => Layer::Overlay,
                    _ => return Err(format!("--layer: unknown layer {val:?}")),
                })
            }
            "--anchor" => {
                use zwlr_layer_surface_v1::Anchor;
                for a in val.split(',').filter(|s| !s.is_empty()) {
                    o.anchor |= match a {
                        "top" => Anchor::Top,
                        "bottom" => Anchor::Bottom,
                        "left" => Anchor::Left,
                        "right" => Anchor::Right,
                        _ => return Err(format!("--anchor: unknown edge {a:?}")),
                    };
                }
            }
            "--exclusive" => {
                o.exclusive = val
                    .parse()
                    .map_err(|_| format!("bad exclusive zone {val:?}"))?
            }
            "--keyboard" => {
                use zwlr_layer_surface_v1::KeyboardInteractivity as K;
                o.keyboard = match val.as_str() {
                    "none" => K::None,
                    "exclusive" => K::Exclusive,
                    "on-demand" | "on_demand" => K::OnDemand,
                    _ => return Err(format!("--keyboard: none|exclusive|on-demand, got {val:?}")),
                }
            }
            "--output" => o.output = Some(val),
            "--export" => o.export = Some(val),
            "--import-parent" => o.import_parent = Some(val),
            "--import-delay" => {
                o.import_delay = Some(val.parse().map_err(|_| format!("bad delay {val:?}"))?)
            }
            "--icon" => o.icons.push(parse_icon(&val)?),
            "--icon-surface" => {
                let (color, delay) = match val.rsplit_once('@') {
                    Some((color, ms)) => (
                        color,
                        Some(
                            ms.parse()
                                .map_err(|_| format!("--icon-surface: bad delay {ms:?}"))?,
                        ),
                    ),
                    None => (val.as_str(), None),
                };
                o.icon_surfaces.push((parse_color(color)?, delay));
            }
            "--retitle" => {
                let (title, ms) = val
                    .rsplit_once('@')
                    .ok_or_else(|| format!("--retitle: expected T@MS, got {val:?}"))?;
                let ms = ms
                    .parse()
                    .map_err(|_| format!("--retitle: bad delay {ms:?}"))?;
                o.retitles.push((title.into(), ms));
            }
            _ => return Err(format!("unknown option {flag:?} (see --help)")),
        }
    }
    if want_dialog {
        if o.parent.is_some() {
            return Err("--dialog and --parent-* are mutually exclusive".into());
        }
        let mut d = dialog_default(&o.main);
        for (flag, val) in dialog_mods {
            match flag.as_str() {
                "--dialog-title" => d.title = val,
                "--dialog-size" => d.size = parse_size(&val)?,
                _ => d.color = parse_color(&val)?,
            }
        }
        o.dialog = Some(d);
    }
    if o.layer.is_some() && (o.parent.is_some() || o.dialog.is_some()) {
        return Err("--layer cannot be combined with parent/dialog options".into());
    }
    if o.layer.is_some() && (o.export.is_some() || o.import_parent.is_some()) {
        return Err("--layer cannot be combined with --export or --import-parent".into());
    }
    if o.import_parent.is_some() && o.parent.is_some() {
        return Err("--import-parent and --parent-* are mutually exclusive".into());
    }
    if o.import_delay.is_some() && o.import_parent.is_none() {
        return Err("--import-delay needs --import-parent".into());
    }
    if o.layer.is_some() && !o.icons.is_empty() {
        return Err("--layer cannot be combined with --icon".into());
    }
    if o.layer.is_some() && !o.retitles.is_empty() {
        return Err("--layer cannot be combined with --retitle".into());
    }
    let iconic = o.iconlet || !o.icon_surfaces.is_empty();
    if iconic && (o.layer.is_some() || o.parent.is_some() || o.dialog.is_some()) {
        return Err(
            "--iconlet and --icon-surface need a single toplevel (no --layer, --parent-*, --dialog)"
                .into(),
        );
    }
    Ok(o)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

enum Role {
    Toplevel {
        xdg: XdgSurface,
        top: XdgToplevel,
        deco: Option<ZxdgToplevelDecorationV1>,
    },
    Layer(ZwlrLayerSurfaceV1),
}

struct Win {
    label: Option<&'static str>,
    surface: WlSurface,
    role: Role,
    spec: WinSpec,
    size: (u32, u32),
    pending_size: (i32, i32),
    pending_states: Vec<&'static str>,
    configured: bool,
    deco_mode: Option<zxdg_toplevel_decoration_v1::Mode>,
    alive: bool,
}

/// The window's icon through xx-iconlet-v1.
struct Iconic {
    /// Kept for as long as the window lives; destroying it would end the
    /// iconlet.
    _object: XxIconicToplevelV1,
    /// The icon surface, its role object, its size and color.
    surface: Option<(WlSurface, XxIconSurfaceV1, (i32, i32), u32)>,
}

struct State {
    shm: WlShm,
    icon_manager: Option<XdgToplevelIconManagerV1>,
    iconic: Option<Iconic>,
    windows: Vec<Win>,
    outputs: Vec<(WlOutput, Option<String>)>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    pointer_pos: (f64, f64),
    pointer_win: Option<usize>,
    keyboard_win: Option<usize>,
    xkb_ctx: xkb::Context,
    xkb_state: Option<xkb::State>,
}

impl State {
    fn say(&self, win: Option<usize>, msg: std::fmt::Arguments) {
        let label = win.and_then(|i| self.windows.get(i)).and_then(|w| w.label);
        let mut out = std::io::stdout().lock();
        let _ = match label {
            Some(l) => writeln!(out, "[{l}] {msg}"),
            None => writeln!(out, "{msg}"),
        };
        let _ = out.flush();
    }

    fn win_of(&self, surface: &WlSurface) -> Option<usize> {
        self.windows
            .iter()
            .position(|w| w.alive && &w.surface == surface)
    }

    fn draw(&mut self, idx: usize, qh: &QueueHandle<Self>) -> Result<()> {
        let w = &self.windows[idx];
        let (width, height) = w.size;
        let (width, height) = (width.max(1), height.max(1));
        let stride = width * 4;
        let len = (stride * height) as usize;
        let fill = 0xff00_0000 | w.spec.color;
        let darken = |c: u32| -> u32 {
            let ch = |s: u32| (((c >> s) & 0xff) * 5 / 10) << s;
            0xff00_0000 | ch(16) | ch(8) | ch(0)
        };
        let border = darken(w.spec.color);
        let mut data = Vec::with_capacity(len);
        for y in 0..height {
            for x in 0..width {
                let edge = x < 2 || y < 2 || x + 2 >= width || y + 2 >= height;
                data.extend_from_slice(&(if edge { border } else { fill }).to_le_bytes());
            }
        }
        let fd = rustix::fs::memfd_create("wltest-buffer", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|e| format!("memfd_create: {e}"))?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&data)
            .map_err(|e| format!("writing buffer: {e}"))?;
        let pool = self.shm.create_pool(file.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        pool.destroy();
        w.surface.attach(Some(&buffer), 0, 0);
        w.surface.damage_buffer(0, 0, width as i32, height as i32);
        w.surface.commit();
        Ok(())
    }

    /// Set `icon` on window `idx`, to be applied by its next commit. The
    /// icon object and its buffers are destroyed right away, which the
    /// protocol allows once the icon is set.
    fn set_icon(&self, idx: usize, icon: &IconSpec, qh: &QueueHandle<Self>) -> Result<()> {
        let Some(manager) = self.icon_manager.as_ref() else {
            return Ok(());
        };
        let Role::Toplevel { top, .. } = &self.windows[idx].role else {
            return Err("--icon needs a toplevel".into());
        };
        let mut buffers = Vec::new();
        let object = match icon {
            IconSpec::None => None,
            IconSpec::Empty => Some(manager.create_icon(qh, ())),
            IconSpec::Name(name) => {
                let object = manager.create_icon(qh, ());
                object.set_name(name.clone());
                Some(object)
            }
            IconSpec::Color(color) => {
                let object = manager.create_icon(qh, ());
                for (size, scale) in [(24, 1), (48, 1), (96, 2)] {
                    let buffer = self.icon_buffer(size, *color, qh)?;
                    object.add_buffer(&buffer, scale);
                    buffers.push(buffer);
                }
                Some(object)
            }
        };
        manager.set_icon(top, object.as_ref());
        if let Some(object) = object {
            object.destroy();
        }
        for buffer in buffers {
            buffer.destroy();
        }
        self.say(Some(idx), format_args!("icon: {icon}"));
        Ok(())
    }

    /// A square icon image `size` pixels across: `color` with a white top
    /// left quarter and a darker border.
    fn icon_buffer(&self, size: u32, color: u32, qh: &QueueHandle<Self>) -> Result<WlBuffer> {
        let fill = 0xff00_0000 | color;
        let ch = |s: u32| (((color >> s) & 0xff) / 2) << s;
        let border = 0xff00_0000 | ch(16) | ch(8) | ch(0);
        let edge = (size / 12).max(1);
        let mut data = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                let px = if x < edge || y < edge || x + edge >= size || y + edge >= size {
                    border
                } else if x < size / 2 && y < size / 2 {
                    0xffff_ffff
                } else {
                    fill
                };
                data.extend_from_slice(&px.to_le_bytes());
            }
        }
        let fd = rustix::fs::memfd_create("wltest-icon", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|e| format!("memfd_create: {e}"))?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&data)
            .map_err(|e| format!("writing icon: {e}"))?;
        let pool = self
            .shm
            .create_pool(file.as_fd(), data.len() as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            size as i32,
            size as i32,
            size as i32 * 4,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        pool.destroy();
        Ok(buffer)
    }

    /// Draw the icon surface in its size and color.
    fn draw_icon_surface(&self, qh: &QueueHandle<Self>) -> Result<()> {
        let Some((surface, _, (w, h), color)) =
            self.iconic.as_ref().and_then(|i| i.surface.as_ref())
        else {
            return Ok(());
        };
        if *w <= 0 || *w != *h {
            return Ok(());
        }
        let buffer = self.icon_buffer(*w as u32, *color, qh)?;
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, *w, *h);
        surface.commit();
        buffer.destroy();
        Ok(())
    }

    fn close(&mut self, idx: usize) {
        self.say(Some(idx), format_args!("closed"));
        let w = &mut self.windows[idx];
        w.alive = false;
        match &w.role {
            Role::Toplevel { xdg, top, deco } => {
                if let Some(d) = deco {
                    d.destroy();
                }
                top.destroy();
                xdg.destroy();
            }
            Role::Layer(ls) => ls.destroy(),
        }
        w.surface.destroy();
    }
}

fn clamp_dim(v: u32, min: u32, max: u32) -> u32 {
    let v = if max > 0 { v.min(max) } else { v };
    v.max(min).max(1)
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<XdgToplevel, usize> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let names = states
                    .chunks_exact(4)
                    .map(|c| match u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) {
                        1 => "maximized",
                        2 => "fullscreen",
                        3 => "resizing",
                        4 => "activated",
                        5 => "tiled_left",
                        6 => "tiled_right",
                        7 => "tiled_top",
                        8 => "tiled_bottom",
                        9 => "suspended",
                        10 => "constrained_left",
                        11 => "constrained_right",
                        12 => "constrained_top",
                        13 => "constrained_bottom",
                        _ => "unknown",
                    })
                    .collect();
                let w = &mut state.windows[*idx];
                w.pending_size = (width, height);
                w.pending_states = names;
            }
            xdg_toplevel::Event::Close => state.close(*idx),
            _ => {}
        }
    }
}

impl Dispatch<XdgSurface, usize> for State {
    fn event(
        state: &mut Self,
        xdg: &XdgSurface,
        event: xdg_surface::Event,
        idx: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            let w = &mut state.windows[*idx];
            if !w.alive {
                return;
            }
            xdg.ack_configure(serial);
            let (cw, ch) = w.pending_size;
            let width = if cw > 0 { cw as u32 } else { w.spec.size.0 };
            let height = if ch > 0 { ch as u32 } else { w.spec.size.1 };
            w.size = (
                clamp_dim(width, w.spec.min.0, w.spec.max.0),
                clamp_dim(height, w.spec.min.1, w.spec.max.1),
            );
            w.configured = true;
            let line = format!(
                "configure: {cw}x{ch} states=[{}]",
                w.pending_states.join(",")
            );
            state.say(Some(*idx), format_args!("{line}"));
            if let Err(e) = state.draw(*idx, qh) {
                eprintln!("wltest: {e}");
            }
        }
    }
}

impl Dispatch<ZxdgToplevelDecorationV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure {
            mode: WEnum::Value(mode),
        } = event
        {
            if state.windows[*idx].deco_mode != Some(mode) {
                state.windows[*idx].deco_mode = Some(mode);
                let name = match mode {
                    zxdg_toplevel_decoration_v1::Mode::ClientSide => "client_side",
                    zxdg_toplevel_decoration_v1::Mode::ServerSide => "server_side",
                    _ => "unknown",
                };
                state.say(Some(*idx), format_args!("decoration: {name}"));
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        ls: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        idx: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                ls.ack_configure(serial);
                let w = &mut state.windows[*idx];
                w.size = (
                    if width > 0 { width } else { w.spec.size.0 },
                    if height > 0 { height } else { w.spec.size.1 },
                );
                w.configured = true;
                state.say(
                    Some(*idx),
                    format_args!("layer configure: {width}x{height}"),
                );
                if let Err(e) = state.draw(*idx, qh) {
                    eprintln!("wltest: {e}");
                }
            }
            zwlr_layer_surface_v1::Event::Closed => state.close(*idx),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgExportedV2, String> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgExportedV2,
        event: zxdg_exported_v2::Event,
        path: &String,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_exported_v2::Event::Handle { handle } = event {
            // Write it atomically: the importer waits for the file to appear.
            let tmp = format!("{path}.tmp");
            if let Err(e) = std::fs::write(&tmp, &handle).and_then(|()| std::fs::rename(&tmp, path)) {
                eprintln!("wltest: writing {path}: {e}");
            }
            state.say(None, format_args!("exported: {handle}"));
        }
    }
}

impl Dispatch<ZxdgImportedV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgImportedV2,
        event: zxdg_imported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_imported_v2::Event::Destroyed = event {
            state.say(None, format_args!("import destroyed"));
        }
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        buffer: &WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs[*idx].1 = Some(name);
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            // Capabilities come and go, e.g. when wlinput creates and destroys
            // its virtual devices on a headless compositor.
            let has_pointer = caps.contains(wl_seat::Capability::Pointer);
            if has_pointer && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            } else if !has_pointer {
                if let Some(p) = state.pointer.take() {
                    p.release();
                    state.pointer_win = None;
                }
            }
            let has_keyboard = caps.contains(wl_seat::Capability::Keyboard);
            if has_keyboard && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            } else if !has_keyboard {
                if let Some(k) = state.keyboard.take() {
                    k.release();
                    state.keyboard_win = None;
                }
            }
        }
    }
}

fn button_name(b: u32) -> String {
    match b {
        0x110 => "left".into(),
        0x111 => "right".into(),
        0x112 => "middle".into(),
        b => b.to_string(),
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_win = state.win_of(&surface);
                state.pointer_pos = (surface_x, surface_y);
                state.say(
                    state.pointer_win,
                    format_args!("pointer enter {surface_x} {surface_y}"),
                );
            }
            wl_pointer::Event::Leave { .. } => {
                state.say(state.pointer_win, format_args!("pointer leave"));
                state.pointer_win = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_pos = (surface_x, surface_y);
            }
            wl_pointer::Event::Button {
                button, state: bs, ..
            } => {
                let pressed = matches!(bs, WEnum::Value(wl_pointer::ButtonState::Pressed));
                let (x, y) = state.pointer_pos;
                state.say(
                    state.pointer_win,
                    format_args!(
                        "pointer button {} {} {x} {y}",
                        button_name(button),
                        if pressed { "pressed" } else { "released" }
                    ),
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                let mut buf = vec![0u8; size as usize];
                let mut read = 0;
                while read < buf.len() {
                    match rustix::io::pread(&fd, &mut buf[read..], read as u64) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => read += n,
                    }
                }
                buf.truncate(read);
                while buf.last() == Some(&0) {
                    buf.pop();
                }
                let keymap = String::from_utf8(buf).ok().and_then(|s| {
                    xkb::Keymap::new_from_string(
                        &state.xkb_ctx,
                        s,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                });
                state.xkb_state = keymap.map(|k| xkb::State::new(&k));
            }
            wl_keyboard::Event::Enter { surface, .. } => {
                state.keyboard_win = state.win_of(&surface);
                state.say(state.keyboard_win, format_args!("keyboard enter"));
            }
            wl_keyboard::Event::Leave { .. } => {
                state.say(state.keyboard_win, format_args!("keyboard leave"));
                state.keyboard_win = None;
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(s) = state.xkb_state.as_mut() {
                    s.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key { key, state: ks, .. } => {
                let pressed = matches!(ks, WEnum::Value(wl_keyboard::KeyState::Pressed));
                let name = match &state.xkb_state {
                    Some(s) => xkb::keysym_get_name(s.key_get_one_sym(xkb::Keycode::new(key + 8))),
                    None => format!("code{key}"),
                };
                state.say(
                    state.keyboard_win,
                    format_args!(
                        "key {name} {}",
                        if pressed { "pressed" } else { "released" }
                    ),
                );
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ZxdgDecorationManagerV1);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ZxdgExporterV2);
delegate_noop!(State: ZxdgImporterV2);
delegate_noop!(State: ignore XdgToplevelIconManagerV1);
delegate_noop!(State: XdgToplevelIconV1);
delegate_noop!(State: XxIconletManagerV1);

impl Dispatch<XxIconicToplevelV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &XxIconicToplevelV1,
        event: xx_iconic_toplevel_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xx_iconic_toplevel_v1::Event::IconGeometry {
                x,
                y,
                width,
                height,
                ..
            } => state.say(None, format_args!("icon: shown {x} {y} {width}x{height}")),
            xx_iconic_toplevel_v1::Event::IconHidden => {
                state.say(None, format_args!("icon: hidden"))
            }
            xx_iconic_toplevel_v1::Event::Activate { token } => {
                let token = if token.is_empty() { " (no token)" } else { "" };
                state.say(None, format_args!("icon: activated{token}"));
            }
        }
    }
}

impl Dispatch<XxIconSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &XxIconSurfaceV1,
        event: xx_icon_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let xx_icon_surface_v1::Event::Configure { width, height } = event;
        state.say(None, format_args!("icon surface: {width}x{height}"));
        if let Some((_, _, size, _)) = state.iconic.as_mut().and_then(|i| i.surface.as_mut()) {
            *size = (width, height);
        }
        if let Err(e) = state.draw_icon_surface(qh) {
            eprintln!("wltest: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Setup and main loop
// ---------------------------------------------------------------------------

struct App {
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    globals: GlobalList,
    compositor: WlCompositor,
    /// For the next toplevel: whether it is an iconlet, and the first
    /// color of its icon surface.
    iconic: Option<(bool, Option<u32>)>,
}

fn top_of(win: &Win) -> XdgToplevel {
    match &win.role {
        Role::Toplevel { top, .. } => top.clone(),
        Role::Layer(_) => unreachable!("only toplevels get an iconic object"),
    }
}

fn bind<I>(
    app_globals: &GlobalList,
    qh: &QueueHandle<State>,
    v: std::ops::RangeInclusive<u32>,
) -> Result<I>
where
    I: Proxy + 'static,
    State: Dispatch<I, ()>,
{
    app_globals
        .bind(qh, v, ())
        .map_err(|_| format!("compositor does not support {}", I::interface().name))
}

impl App {
    fn create_toplevel(
        &mut self,
        label: Option<&'static str>,
        spec: &WinSpec,
        deco: Deco,
        parent: Option<&XdgToplevel>,
        foreign_parent: Option<&ZxdgImportedV2>,
        icons: &[IconSpec],
    ) -> Result<usize> {
        let wm: XdgWmBase = bind(&self.globals, &self.qh, 1..=6)?;
        let idx = self.state.windows.len();
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = wm.get_xdg_surface(&surface, &self.qh, idx);
        let top = xdg.get_toplevel(&self.qh, idx);
        top.set_title(spec.title.clone());
        top.set_app_id(spec.app_id.clone());
        if spec.min != (0, 0) {
            top.set_min_size(spec.min.0 as i32, spec.min.1 as i32);
        }
        if spec.max != (0, 0) {
            top.set_max_size(spec.max.0 as i32, spec.max.1 as i32);
        }
        if let Some(p) = parent {
            top.set_parent(Some(p));
        }
        if let Some(imported) = foreign_parent {
            imported.set_parent_of(&surface);
        }
        let deco = if deco == Deco::None {
            None
        } else {
            let mgr: ZxdgDecorationManagerV1 = bind(&self.globals, &self.qh, 1..=1)?;
            let d = mgr.get_toplevel_decoration(&top, &self.qh, idx);
            match deco {
                Deco::Csd => d.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide),
                Deco::Ssd => d.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide),
                _ => {}
            }
            Some(d)
        };
        self.state.windows.push(Win {
            label,
            surface: surface.clone(),
            role: Role::Toplevel { xdg, top, deco },
            spec: spec.clone(),
            size: spec.size,
            pending_size: (0, 0),
            pending_states: Vec::new(),
            configured: false,
            deco_mode: None,
            alive: true,
        });
        for icon in icons {
            self.state.set_icon(idx, icon, &self.qh)?;
        }
        if let Some((iconlet, color)) = self.iconic.take() {
            let manager: XxIconletManagerV1 = bind(&self.globals, &self.qh, 1..=1)?;
            let object =
                manager.get_iconic_toplevel(&top_of(&self.state.windows[idx]), &self.qh, ());
            if iconlet {
                object.set_iconlet();
            }
            let surface = color.map(|color| {
                let surface = self.compositor.create_surface(&self.qh, ());
                let role = object.get_icon_surface(&surface, &self.qh, ());
                (surface, role, (0, 0), color)
            });
            self.state.iconic = Some(Iconic {
                _object: object,
                surface,
            });
        }
        surface.commit();
        Ok(idx)
    }

    fn create_layer(&mut self, opts: &Options, layer: zwlr_layer_shell_v1::Layer) -> Result<usize> {
        let shell: ZwlrLayerShellV1 = bind(&self.globals, &self.qh, 1..=4)?;
        let output = match &opts.output {
            None => None,
            Some(name) => Some(
                self.state
                    .outputs
                    .iter()
                    .find(|(_, n)| n.as_deref() == Some(name.as_str()))
                    .map(|(o, _)| o.clone())
                    .ok_or_else(|| {
                        let names: Vec<_> = self
                            .state
                            .outputs
                            .iter()
                            .filter_map(|(_, n)| n.clone())
                            .collect();
                        format!("no output named {name:?} (have: {})", names.join(", "))
                    })?,
            ),
        };
        let idx = self.state.windows.len();
        let surface = self.compositor.create_surface(&self.qh, ());
        let ls = shell.get_layer_surface(
            &surface,
            output.as_ref(),
            layer,
            "wltest".into(),
            &self.qh,
            idx,
        );
        ls.set_size(opts.main.size.0, opts.main.size.1);
        ls.set_anchor(opts.anchor);
        ls.set_exclusive_zone(opts.exclusive);
        if opts.keyboard == zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand
            && shell.version() < 4
        {
            return Err("--keyboard on-demand needs zwlr_layer_shell_v1 version 4".into());
        }
        ls.set_keyboard_interactivity(opts.keyboard);
        surface.commit();
        self.state.windows.push(Win {
            label: None,
            surface,
            role: Role::Layer(ls),
            spec: opts.main.clone(),
            size: opts.main.size,
            pending_size: (0, 0),
            pending_states: Vec::new(),
            configured: false,
            deco_mode: None,
            alive: true,
        });
        Ok(idx)
    }

    fn wait_configured(&mut self, idx: usize) -> Result<()> {
        while !self.state.windows[idx].configured {
            if !self.state.windows[idx].alive {
                return Err("window closed before its first configure".into());
            }
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(wl_err)?;
        }
        Ok(())
    }

    fn dispatch_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.queue.flush().map_err(wl_err)?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(wl_err)?;
        if let Some(guard) = self.queue.prepare_read() {
            let readable = {
                let fd = guard.connection_fd();
                let mut fds = [rustix::event::PollFd::new(
                    &fd,
                    rustix::event::PollFlags::IN,
                )];
                let ts = timeout.map(|t| rustix::event::Timespec {
                    tv_sec: t.as_secs() as _,
                    tv_nsec: t.subsec_nanos() as _,
                });
                match rustix::event::poll(&mut fds, ts.as_ref()) {
                    Ok(n) => n > 0,
                    Err(rustix::io::Errno::INTR) => false,
                    Err(e) => return Err(format!("poll: {e}")),
                }
            };
            if readable {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(wl_err(e)),
                }
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(wl_err)?;
        Ok(())
    }
}

/// The xdg-foreign handle in `path`, waiting up to 10 s for the file.
fn wait_for_handle(path: &str) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match std::fs::read_to_string(path) {
            Ok(handle) if !handle.trim().is_empty() => return Ok(handle.trim().to_string()),
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => return Err(format!("no handle in {path} after 10 s")),
        }
    }
}

fn wl_err(e: impl std::fmt::Display) -> String {
    format!("wayland: {e}")
}

fn run(opts: Options) -> Result<()> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err("WAYLAND_DISPLAY is not set; refusing to guess a compositor".into());
    }
    let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
    let (globals, queue) = registry_queue_init::<State>(&conn).map_err(wl_err)?;
    let qh = queue.handle();
    let compositor: WlCompositor = bind(&globals, &qh, 4..=6)?;
    let shm: WlShm = bind(&globals, &qh, 1..=1)?;
    // The seat is optional: without one the client simply gets no input.
    let _seat: Option<WlSeat> = globals.bind(&qh, 3..=7, ()).ok();

    let mut state = State {
        shm,
        icon_manager: None,
        iconic: None,
        windows: Vec::new(),
        outputs: Vec::new(),
        pointer: None,
        keyboard: None,
        pointer_pos: (0.0, 0.0),
        pointer_win: None,
        keyboard_win: None,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_state: None,
    };
    let output_globals: Vec<(u32, u32)> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == WlOutput::interface().name)
            .map(|g| (g.name, g.version))
            .collect()
    });
    for (name, version) in output_globals {
        let idx = state.outputs.len();
        let output: WlOutput = globals.registry().bind(name, version.min(4), &qh, idx);
        state.outputs.push((output, None));
    }

    let mut app = App {
        queue,
        qh,
        state,
        globals,
        compositor,
        iconic: (opts.iconlet || !opts.icon_surfaces.is_empty()).then(|| {
            let first = opts.icon_surfaces.iter().find(|(_, delay)| delay.is_none());
            let first = first
                .or(opts.icon_surfaces.first())
                .map(|(color, _)| *color);
            (opts.iconlet, first)
        }),
    };
    app.queue.roundtrip(&mut app.state).map_err(wl_err)?;
    if !opts.icons.is_empty() {
        // Without it the window maps all the same, so that scenarios run
        // on such compositors too.
        app.state.icon_manager = bind(&app.globals, &app.qh, 1..=1)
            .map_err(|e| eprintln!("wltest: {e}; ignoring --icon"))
            .ok();
    }
    let early_icons: Vec<IconSpec> = opts
        .icons
        .iter()
        .filter(|(_, delay)| delay.is_none())
        .map(|(icon, _)| icon.clone())
        .collect();
    let mut late_icons: Vec<(u64, IconSpec)> = opts
        .icons
        .iter()
        .filter_map(|(icon, delay)| delay.map(|ms| (ms, icon.clone())))
        .collect();
    late_icons.sort_by_key(|(ms, _)| *ms);
    late_icons.reverse();
    let mut retitles = opts.retitles.clone();
    retitles.sort_by_key(|(_, ms)| *ms);
    retitles.reverse();
    let mut icon_colors: Vec<(u64, u32)> = opts
        .icon_surfaces
        .iter()
        .filter_map(|(color, delay)| delay.map(|ms| (ms, *color)))
        .collect();
    icon_colors.sort_by_key(|(ms, _)| *ms);
    icon_colors.reverse();

    let imported = match &opts.import_parent {
        Some(path) => {
            let handle = wait_for_handle(path)?;
            let importer: ZxdgImporterV2 = bind(&app.globals, &app.qh, 1..=1)?;
            Some(importer.import_toplevel(handle, &app.qh, ()))
        }
        None => None,
    };
    // Set right away unless delayed.
    let early_parent = imported.as_ref().filter(|_| opts.import_delay.is_none());

    let main_idx = if let Some(layer) = opts.layer {
        app.create_layer(&opts, layer)?
    } else if let Some(parent) = &opts.parent {
        let p = app.create_toplevel(Some("parent"), parent, opts.deco, None, None, &[])?;
        app.wait_configured(p)?;
        let Role::Toplevel { top, .. } = &app.state.windows[p].role else {
            unreachable!()
        };
        let top = top.clone();
        app.create_toplevel(
            Some("child"),
            &opts.main,
            opts.deco,
            Some(&top),
            None,
            &early_icons,
        )?
    } else if let Some(dialog) = &opts.dialog {
        let p = app.create_toplevel(
            Some("parent"),
            &opts.main,
            opts.deco,
            None,
            None,
            &early_icons,
        )?;
        app.wait_configured(p)?;
        let Role::Toplevel { top, .. } = &app.state.windows[p].role else {
            unreachable!()
        };
        let top = top.clone();
        app.create_toplevel(Some("child"), dialog, opts.deco, Some(&top), None, &[])?;
        p
    } else {
        app.create_toplevel(
            None,
            &opts.main,
            opts.deco,
            None,
            early_parent,
            &early_icons,
        )?
    };
    if let Some(path) = &opts.export {
        app.wait_configured(main_idx)?;
        let exporter: ZxdgExporterV2 = bind(&app.globals, &app.qh, 1..=1)?;
        let surface = app.state.windows[main_idx].surface.clone();
        let _exported = exporter.export_toplevel(&surface, &app.qh, path.clone());
    }
    let mut late_parent = imported.filter(|_| opts.import_delay.is_some());

    let mut request = opts.request.clone();
    let mut main_configured_at = None;
    loop {
        if main_configured_at.is_none() && app.state.windows[main_idx].configured {
            main_configured_at = Some(Instant::now());
        }
        if app.state.windows.iter().all(|w| !w.alive) {
            return Ok(());
        }
        let mut timeout: Option<Duration> = None;
        if let (Some((ms, icon)), Some(t0)) = (late_icons.last(), main_configured_at) {
            let due = t0 + Duration::from_millis(*ms);
            let now = Instant::now();
            if now >= due {
                if app.state.windows[main_idx].alive {
                    app.state.set_icon(main_idx, icon, &app.qh)?;
                    app.state.windows[main_idx].surface.commit();
                }
                late_icons.pop();
                continue;
            }
            timeout = Some(due - now);
        }
        if let (Some((ms, color)), Some(t0)) = (icon_colors.last(), main_configured_at) {
            let due = t0 + Duration::from_millis(*ms);
            let now = Instant::now();
            if now >= due {
                if let Some((_, _, _, c)) =
                    app.state.iconic.as_mut().and_then(|i| i.surface.as_mut())
                {
                    *c = *color;
                }
                app.state
                    .say(None, format_args!("icon surface: {color:06x}"));
                app.state.draw_icon_surface(&app.qh)?;
                icon_colors.pop();
                continue;
            }
            timeout = Some(timeout.map_or(due - now, |t| t.min(due - now)));
        }
        if let (Some((title, ms)), Some(t0)) = (retitles.last(), main_configured_at) {
            let due = t0 + Duration::from_millis(*ms);
            let now = Instant::now();
            if now >= due {
                let w = &app.state.windows[main_idx];
                if let (true, Role::Toplevel { top, .. }) = (w.alive, &w.role) {
                    top.set_title(title.clone());
                    app.state.say(Some(main_idx), format_args!("title: {title}"));
                }
                retitles.pop();
                continue;
            }
            timeout = Some(timeout.map_or(due - now, |t| t.min(due - now)));
        }
        if let (Some(imported), Some(t0)) = (&late_parent, main_configured_at) {
            let due = t0 + Duration::from_millis(opts.import_delay.unwrap_or(0));
            let now = Instant::now();
            if now >= due {
                let w = &app.state.windows[main_idx];
                if w.alive {
                    imported.set_parent_of(&w.surface);
                }
                late_parent = None;
                continue;
            }
            timeout = Some(timeout.map_or(due - now, |t| t.min(due - now)));
        }
        if let (Some(req), Some(t0)) = (&request, main_configured_at) {
            let due = t0 + Duration::from_millis(opts.request_delay);
            let now = Instant::now();
            if now >= due {
                let w = &app.state.windows[main_idx];
                if let (true, Role::Toplevel { top, .. }) = (w.alive, &w.role) {
                    if req == "fullscreen" {
                        top.set_fullscreen(None);
                    } else {
                        top.set_maximized();
                    }
                }
                request = None;
                continue;
            }
            timeout = Some(timeout.map_or(due - now, |t| t.min(due - now)));
        }
        app.dispatch_timeout(timeout)?;
    }
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("wltest: {e}");
            return ExitCode::from(2);
        }
    };
    match run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wltest: {e}");
            ExitCode::FAILURE
        }
    }
}
