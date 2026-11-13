//! sway's IPC (`emulate_sway`), answered with what canoe shows.
//!
//! Some screenshot tools ask sway where things are: shotman runs
//! `swaymsg -t get_tree` for the active window, grimshot for windows and the
//! focused output. Canoe answers the queries on a socket of its own, named
//! by `SWAYSOCK` (set for the programs it launches), so `swaymsg` and other
//! clients of sway's IPC find it.
//!
//! The wire format is i3's: the magic string `i3-ipc`, the payload's length
//! and the message type (both 32-bit, host byte order), then the JSON
//! payload. Replies carry the request's type.
//!
//! The tree is canoe's layout told the way sway tells a floating layout:
//! the root holds the outputs, each output one workspace (named after its
//! position, left to right), and each workspace its windows as floating
//! containers, bottom to top. Minimized windows stay in their workspace,
//! invisible. Queries are answered; commands fail and subscriptions are
//! refused, so tools that drive sway or follow its events don't work.

use std::cell::Cell;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use serde_json::{json, Value};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use smithay::utils::Transform;

use super::{Shell, State};
use crate::canoe;

const MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_LEN: usize = 14;
/// Larger requests close the connection.
const MAX_PAYLOAD: usize = 1 << 20;
/// The sway release whose IPC this imitates.
const SWAY_VERSION: (u32, u32, u32) = (1, 10, 0);

const RUN_COMMAND: u32 = 0;
const GET_WORKSPACES: u32 = 1;
const SUBSCRIBE: u32 = 2;
const GET_OUTPUTS: u32 = 3;
const GET_TREE: u32 = 4;
const GET_MARKS: u32 = 5;
const GET_BAR_CONFIG: u32 = 6;
const GET_VERSION: u32 = 7;
const GET_BINDING_MODES: u32 = 8;
const GET_CONFIG: u32 = 9;
const SEND_TICK: u32 = 10;
const SYNC: u32 = 11;
const GET_BINDING_STATE: u32 = 12;
const GET_INPUTS: u32 = 100;
const GET_SEATS: u32 = 101;

/// A rectangle in global logical coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    fn new((x, y, width, height): (i32, i32, i32, i32)) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn json(self) -> Value {
        json!({ "x": self.x, "y": self.y, "width": self.width, "height": self.height })
    }

    fn contains_center_of(self, other: Rect) -> bool {
        let (cx, cy) = (other.x + other.width / 2, other.y + other.height / 2);
        cx >= self.x && cx < self.x + self.width && cy >= self.y && cy < self.y + self.height
    }
}

/// An output as the IPC tells it.
#[derive(Debug, Clone, Default)]
pub struct OutputInfo {
    pub id: canoe::OutputId,
    pub name: String,
    pub make: String,
    pub model: String,
    pub serial: String,
    pub rect: Rect,
    /// What windows may cover, without panels.
    pub usable: Rect,
    pub scale: f64,
    pub transform: &'static str,
    /// Physical size and refresh rate (mHz) of the current mode.
    pub mode: Option<(i32, i32, i32)>,
    pub modes: Vec<(i32, i32, i32)>,
    pub focused: bool,
}

/// A window as the IPC tells it.
#[derive(Debug, Clone, Default)]
pub struct WindowInfo {
    pub id: canoe::WindowId,
    pub title: String,
    pub app_id: Option<String>,
    pub pid: i32,
    /// The X11 window, for windows of X11 programs.
    pub x11_window: Option<u32>,
    /// What it covers, frame included.
    pub frame: Rect,
    /// Its content.
    pub content: Rect,
    pub border: i32,
    pub csd: bool,
    pub focused: bool,
    pub visible: bool,
    pub fullscreen: bool,
    /// The output it is on.
    pub output: Option<canoe::OutputId>,
}

/// What the IPC tells about, gathered from the compositor and the window
/// manager. Outputs left to right, windows bottom to top.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub outputs: Vec<OutputInfo>,
    pub windows: Vec<WindowInfo>,
}

// Node ids are unique across the tree: 1 for the root, then by kind.
fn window_node_id(id: canoe::WindowId) -> u64 {
    id as u64 * 4 + 4
}
fn output_node_id(id: canoe::OutputId) -> u64 {
    id as u64 * 4 + 5
}
fn workspace_node_id(id: canoe::OutputId) -> u64 {
    id as u64 * 4 + 6
}
const ROOT_NODE_ID: u64 = 1;

impl Snapshot {
    /// The output a window belongs to: the one it is on, else the one its
    /// center is on, else the first.
    fn output_of(&self, window: &WindowInfo) -> Option<&OutputInfo> {
        window
            .output
            .and_then(|id| self.outputs.iter().find(|o| o.id == id))
            .or_else(|| {
                self.outputs
                    .iter()
                    .find(|o| o.rect.contains_center_of(window.frame))
            })
            .or_else(|| self.outputs.first())
    }

    fn windows_on(&self, output: &OutputInfo) -> Vec<&WindowInfo> {
        self.windows
            .iter()
            .filter(|w| self.output_of(w).is_some_and(|o| o.id == output.id))
            .collect()
    }

    fn workspace_name(&self, output: &OutputInfo) -> String {
        let index = self.outputs.iter().position(|o| o.id == output.id);
        (index.unwrap_or(0) + 1).to_string()
    }

    /// The focused output: the window manager's current one, else the one
    /// with the focused window, else the first.
    fn focused_output(&self) -> Option<&OutputInfo> {
        self.outputs
            .iter()
            .find(|o| o.focused)
            .or_else(|| {
                self.windows
                    .iter()
                    .find(|w| w.focused)
                    .and_then(|w| self.output_of(w))
            })
            .or_else(|| self.outputs.first())
    }

    fn window_focused(&self) -> bool {
        self.windows.iter().any(|w| w.focused)
    }

    fn window_json(&self, window: &WindowInfo) -> Value {
        let frame = window.frame;
        let content = window.content;
        let window_rect = Rect {
            x: content.x - frame.x,
            y: content.y - frame.y,
            width: content.width,
            height: content.height,
        };
        let deco_rect = Rect {
            x: 0,
            y: 0,
            width: frame.width,
            height: (window_rect.y - window.border).max(0),
        };
        let mut node = json!({
            "id": window_node_id(window.id),
            "type": "floating_con",
            "name": window.title,
            "rect": frame.json(),
            "window_rect": window_rect.json(),
            "deco_rect": deco_rect.json(),
            "geometry": Rect { x: 0, y: 0, width: content.width, height: content.height }.json(),
            "focused": window.focused,
            "visible": window.visible,
            "urgent": false,
            "sticky": false,
            "fullscreen_mode": if window.fullscreen { 1 } else { 0 },
            "layout": "none",
            "orientation": "none",
            "percent": null,
            "border": if window.csd { "csd" } else { "normal" },
            "current_border_width": window.border,
            "marks": [],
            "focus": [],
            "nodes": [],
            "floating_nodes": [],
            "pid": window.pid,
            "app_id": window.app_id,
            "shell": if window.x11_window.is_some() { "xwayland" } else { "xdg_shell" },
            "inhibit_idle": false,
            "idle_inhibitors": { "user": "none", "application": "none" },
            "max_render_time": 0,
            "allow_tearing": false,
        });
        if let Some(x11_window) = window.x11_window {
            node["window"] = json!(x11_window);
            node["window_properties"] = json!({
                "title": window.title,
                "class": window.app_id,
                "instance": window.app_id,
            });
        } else {
            node["window"] = Value::Null;
        }
        node
    }

    fn workspace_json(&self, output: &OutputInfo, with_nodes: bool) -> Value {
        let focused_output = self.focused_output().map(|o| o.id);
        let windows = self.windows_on(output);
        let mut node = json!({
            "id": workspace_node_id(output.id),
            "type": "workspace",
            "name": self.workspace_name(output),
            "num": self.outputs.iter().position(|o| o.id == output.id).unwrap_or(0) + 1,
            "rect": output.usable.json(),
            "focused": Some(output.id) == focused_output,
            "visible": true,
            "urgent": false,
            "output": output.name,
            "layout": "splith",
            "orientation": "horizontal",
            "representation": null,
            "border": "none",
            "current_border_width": 0,
            "percent": null,
            "marks": [],
            "sticky": false,
            "fullscreen_mode": 1,
        });
        if with_nodes {
            // Topmost first, like sway's focus order.
            let focus: Vec<u64> = windows.iter().rev().map(|w| window_node_id(w.id)).collect();
            node["focus"] = json!(focus);
            node["nodes"] = json!([]);
            node["floating_nodes"] =
                Value::Array(windows.iter().map(|w| self.window_json(w)).collect());
            // sway marks the workspace focused only if nothing in it is.
            node["focused"] =
                json!(Some(output.id) == focused_output && !windows.iter().any(|w| w.focused));
        }
        node
    }

    fn output_json(&self, output: &OutputInfo, with_nodes: bool) -> Value {
        let focused = self.focused_output().map(|o| o.id) == Some(output.id);
        let mode = |(width, height, refresh): (i32, i32, i32)| json!({ "width": width, "height": height, "refresh": refresh });
        let mut node = json!({
            "id": output_node_id(output.id),
            "type": "output",
            "name": output.name,
            "make": output.make,
            "model": output.model,
            "serial": output.serial,
            "active": true,
            "dpms": true,
            "power": true,
            "primary": false,
            "scale": output.scale,
            "scale_filter": "linear",
            "subpixel_hinting": "unknown",
            "transform": output.transform,
            "adaptive_sync_status": "disabled",
            "max_render_time": "off",
            "allow_tearing": false,
            "current_workspace": self.workspace_name(output),
            "modes": output.modes.iter().copied().map(mode).collect::<Vec<_>>(),
            "current_mode": output.mode.map(mode),
            "rect": output.rect.json(),
            "focused": focused,
            "layout": "output",
            "orientation": "none",
            "percent": null,
            "urgent": false,
            "marks": [],
            "border": "none",
            "current_border_width": 0,
            "sticky": false,
            "fullscreen_mode": 0,
            "focus": [workspace_node_id(output.id)],
            "nodes": [],
            "floating_nodes": [],
        });
        if with_nodes {
            node["nodes"] = json!([self.workspace_json(output, true)]);
            // The output is the focused node only if nothing in it is.
            node["focused"] = json!(false);
        }
        node
    }

    fn tree_json(&self) -> Value {
        let mut rect = Rect::default();
        for output in &self.outputs {
            let right = (rect.x + rect.width).max(output.rect.x + output.rect.width);
            let bottom = (rect.y + rect.height).max(output.rect.y + output.rect.height);
            rect.x = rect.x.min(output.rect.x);
            rect.y = rect.y.min(output.rect.y);
            rect.width = right - rect.x;
            rect.height = bottom - rect.y;
        }
        let focused_output = self.focused_output().map(|o| o.id);
        let mut focus: Vec<u64> = self.outputs.iter().map(|o| output_node_id(o.id)).collect();
        if let Some(id) = focused_output {
            focus.retain(|&n| n != output_node_id(id));
            focus.insert(0, output_node_id(id));
        }
        json!({
            "id": ROOT_NODE_ID,
            "type": "root",
            "name": "root",
            "rect": rect.json(),
            "focused": false,
            "urgent": false,
            "layout": "splith",
            "orientation": "horizontal",
            "percent": null,
            "border": "none",
            "current_border_width": 0,
            "marks": [],
            "sticky": false,
            "fullscreen_mode": 0,
            "focus": focus,
            "nodes": self.outputs.iter().map(|o| self.output_json(o, true)).collect::<Vec<_>>(),
            "floating_nodes": [],
        })
    }

    fn workspaces_json(&self) -> Value {
        Value::Array(
            self.outputs
                .iter()
                .map(|o| self.workspace_json(o, false))
                .collect(),
        )
    }

    fn outputs_json(&self) -> Value {
        Value::Array(
            self.outputs
                .iter()
                .map(|o| self.output_json(o, false))
                .collect(),
        )
    }

    fn seats_json(&self) -> Value {
        let focus = self
            .windows
            .iter()
            .find(|w| w.focused)
            .map(|w| window_node_id(w.id))
            .unwrap_or(0);
        json!([{ "name": "seat0", "capabilities": 3, "focus": focus, "devices": [] }])
    }

    /// The reply to a message of `kind` with `payload`.
    pub fn reply(&self, kind: u32, payload: &[u8]) -> Value {
        let _ = self.window_focused();
        match kind {
            RUN_COMMAND => {
                let commands = String::from_utf8_lossy(payload);
                let count = commands
                    .split([';', ','])
                    .filter(|c| !c.trim().is_empty())
                    .count()
                    .max(1);
                let failure = json!({
                    "success": false,
                    "parse_error": false,
                    "error": "canoe answers sway's queries but doesn't run its commands",
                });
                Value::Array(vec![failure; count])
            }
            GET_WORKSPACES => self.workspaces_json(),
            SUBSCRIBE => json!({ "success": false }),
            GET_OUTPUTS => self.outputs_json(),
            GET_TREE => self.tree_json(),
            GET_MARKS => json!([]),
            GET_BAR_CONFIG => {
                if payload.is_empty() {
                    json!([])
                } else {
                    json!({ "success": false, "error": "No bar with that ID" })
                }
            }
            GET_VERSION => {
                let (major, minor, patch) = SWAY_VERSION;
                json!({
                    "major": major,
                    "minor": minor,
                    "patch": patch,
                    "human_readable": format!(
                        "{major}.{minor}.{patch} (canoe {}, emulating sway's IPC)",
                        env!("CARGO_PKG_VERSION")
                    ),
                    "loaded_config_file_name": "",
                })
            }
            GET_BINDING_MODES => json!(["default"]),
            GET_CONFIG => json!({ "config": "" }),
            SEND_TICK => json!({ "success": true }),
            SYNC => json!({ "success": false }),
            GET_BINDING_STATE => json!({ "name": "default" }),
            GET_INPUTS => json!([]),
            GET_SEATS => self.seats_json(),
            _ => json!({ "success": false, "error": "Unknown message type" }),
        }
    }
}

/// Split the first complete message off `buf`: its type and payload.
/// `Err` if `buf` doesn't start like one.
fn take_message(buf: &mut Vec<u8>) -> Result<Option<(u32, Vec<u8>)>, ()> {
    if buf.len() < HEADER_LEN {
        return if MAGIC.starts_with(&buf[..buf.len().min(MAGIC.len())]) {
            Ok(None)
        } else {
            Err(())
        };
    }
    if &buf[..6] != MAGIC {
        return Err(());
    }
    let len = u32::from_ne_bytes(buf[6..10].try_into().unwrap()) as usize;
    let kind = u32::from_ne_bytes(buf[10..14].try_into().unwrap());
    if len > MAX_PAYLOAD {
        return Err(());
    }
    if buf.len() < HEADER_LEN + len {
        return Ok(None);
    }
    let payload = buf[HEADER_LEN..HEADER_LEN + len].to_vec();
    buf.drain(..HEADER_LEN + len);
    Ok(Some((kind, payload)))
}

/// A framed message of `kind` carrying `payload`.
fn frame(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(payload);
    out
}

fn transform_name(transform: Transform) -> &'static str {
    match transform {
        Transform::Normal => "normal",
        Transform::_90 => "90",
        Transform::_180 => "180",
        Transform::_270 => "270",
        Transform::Flipped => "flipped",
        Transform::Flipped90 => "flipped-90",
        Transform::Flipped180 => "flipped-180",
        Transform::Flipped270 => "flipped-270",
    }
}

/// The running emulation.
pub struct SwayIpc {
    path: PathBuf,
    listener: RegistrationToken,
    /// Cleared on stop, so open connections close.
    active: Rc<Cell<bool>>,
}

impl SwayIpc {
    /// Listen on a socket in `$XDG_RUNTIME_DIR` named like sway's, and point
    /// `SWAYSOCK` at it.
    pub fn start(handle: &LoopHandle<'static, State>) -> io::Result<Self> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
        let path = PathBuf::from(runtime_dir).join(format!(
            "sway-ipc.{}.{}.sock",
            unsafe { nix::libc::getuid() },
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let active = Rc::new(Cell::new(true));
        let handle_clone = handle.clone();
        let active_clone = active.clone();
        let token = handle
            .insert_source(
                Generic::new(listener, Interest::READ, Mode::Level),
                move |_, listener, _| {
                    loop {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                accept(&handle_clone, stream, active_clone.clone());
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) => {
                                eprintln!("canoe: sway IPC: {e}");
                                break;
                            }
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| io::Error::other(e.to_string()))?;
        std::env::set_var("SWAYSOCK", &path);
        Ok(Self {
            path,
            listener: token,
            active,
        })
    }

    pub fn stop(self, handle: &LoopHandle<'static, State>) {
        handle.remove(self.listener);
        self.active.set(false);
        if std::env::var_os("SWAYSOCK").is_some_and(|s| s == self.path.as_os_str()) {
            std::env::remove_var("SWAYSOCK");
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for SwayIpc {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Answer `stream`'s messages as they come.
fn accept(handle: &LoopHandle<'static, State>, stream: UnixStream, active: Rc<Cell<bool>>) {
    // Readable when the callback runs, so reads don't wait; replies are
    // written in full, but a client that doesn't read is given up on.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let mut buf = Vec::new();
    let result = handle.insert_source(
        Generic::new(stream, Interest::READ, Mode::Level),
        move |_, stream, state| {
            if !active.get() {
                return Ok(PostAction::Remove);
            }
            let mut chunk = [0u8; 4096];
            let n = match (&**stream).read(&mut chunk) {
                Ok(0) => return Ok(PostAction::Remove),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
                Err(_) => return Ok(PostAction::Remove),
            };
            buf.extend_from_slice(&chunk[..n]);
            loop {
                match take_message(&mut buf) {
                    Ok(Some((kind, payload))) => {
                        let reply = state.sway_snapshot().reply(kind, &payload);
                        let reply = frame(kind, reply.to_string().as_bytes());
                        if (&**stream).write_all(&reply).is_err() {
                            return Ok(PostAction::Remove);
                        }
                    }
                    Ok(None) => return Ok(PostAction::Continue),
                    Err(()) => return Ok(PostAction::Remove),
                }
            }
        },
    );
    if let Err(e) = result {
        eprintln!("canoe: sway IPC: {e}");
    }
}

impl State {
    /// Start or stop the sway IPC emulation as the configuration says.
    pub fn apply_sway_emulation(&mut self) {
        let wanted = self.context.borrow().config.emulate_sway;
        match (wanted, self.sway_ipc.is_some()) {
            (true, false) => match SwayIpc::start(&self.loop_handle) {
                Ok(ipc) => self.sway_ipc = Some(ipc),
                Err(e) => eprintln!("canoe: sway IPC emulation unavailable: {e}"),
            },
            (false, true) => {
                if let Some(ipc) = self.sway_ipc.take() {
                    ipc.stop(&self.loop_handle);
                }
            }
            _ => {}
        }
    }

    /// What the sway IPC tells about right now.
    fn sway_snapshot(&self) -> Snapshot {
        let context = self.context.borrow();
        let ui = &context.config.ui;

        let mut outputs: Vec<OutputInfo> = self
            .outputs
            .iter()
            .filter(|o| o.displayed())
            .filter_map(|managed| {
                let canoe_output = context.outputs.get(&managed.id)?.borrow();
                let output = &managed.output;
                let props = output.physical_properties();
                let current = output.current_mode();
                let modes = output
                    .modes()
                    .iter()
                    .map(|m| (m.size.w, m.size.h, m.refresh))
                    .collect();
                Some(OutputInfo {
                    id: managed.id,
                    name: output.name(),
                    make: props.make,
                    model: props.model,
                    serial: "Unknown".to_string(),
                    rect: Rect::new((
                        canoe_output.x,
                        canoe_output.y,
                        canoe_output.width,
                        canoe_output.height,
                    )),
                    usable: Rect::new(canoe_output.usable_area()),
                    scale: output.current_scale().fractional_scale(),
                    transform: transform_name(output.current_transform()),
                    mode: current.map(|m| (m.size.w, m.size.h, m.refresh)),
                    modes,
                    focused: context.current_output == Some(managed.id),
                })
            })
            .collect();
        outputs.sort_by_key(|o| (o.rect.x, o.rect.y));

        // Bottom to top: minimized windows first, then as stacked.
        let stacked: Vec<canoe::WindowId> = context.stacking_order().into_iter().rev().collect();
        let mut ids: Vec<canoe::WindowId> = context
            .windows
            .keys()
            .copied()
            .filter(|id| !stacked.contains(id))
            .collect();
        ids.sort();
        ids.extend(stacked);

        let windows = ids
            .into_iter()
            .filter_map(|id| {
                let w = context.windows.get(&id)?.borrow();
                let managed = self.window_by_id(id)?;
                if w.width <= 0 || w.height <= 0 || w.position_undefined {
                    return None;
                }
                let fullscreen = !matches!(w.fullscreen, canoe::window::FullscreenState::None);
                // A window fullscreen on an output covers it; its position
                // and output stay those it goes back to.
                let fullscreen_output = match &w.fullscreen {
                    canoe::window::FullscreenState::Output(output) => output
                        .upgrade()
                        .map(|o| o.borrow().id)
                        .and_then(|id| outputs.iter().find(|o| o.id == id)),
                    _ => None,
                };
                let (frame, content) = match fullscreen_output {
                    Some(output) => (output.rect, output.rect),
                    None => (
                        Rect::new(w.frame_rect(ui)),
                        Rect::new((w.x, w.y, w.width, w.height)),
                    ),
                };
                let csd = !managed.ssd || fullscreen;
                Some(WindowInfo {
                    id,
                    title: w.title.clone().unwrap_or_default(),
                    app_id: w.app_id.clone(),
                    pid: w.pid,
                    x11_window: match &managed.shell {
                        Shell::X11(x11) => Some(x11.window_id()),
                        Shell::Xdg(_) => None,
                    },
                    frame,
                    content,
                    border: if csd { 0 } else { content.x - frame.x },
                    csd,
                    focused: context.focused_window == Some(id) && !w.hidden,
                    visible: !w.hidden,
                    fullscreen,
                    output: fullscreen_output.map(|o| o.id).or_else(|| {
                        w.output
                            .as_ref()
                            .and_then(|o| o.upgrade())
                            .map(|o| o.borrow().id)
                    }),
                })
            })
            .collect();
        Snapshot { outputs, windows }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        let output = |id, x, focused| OutputInfo {
            id,
            name: format!("DP-{id}"),
            rect: Rect::new((x, 0, 1280, 720)),
            usable: Rect::new((x, 30, 1280, 690)),
            scale: 1.0,
            transform: "normal",
            mode: Some((1280, 720, 60000)),
            modes: vec![(1280, 720, 60000)],
            focused,
            ..Default::default()
        };
        let window = |id, x, focused| WindowInfo {
            id,
            title: format!("w{id}"),
            app_id: Some("foot".into()),
            pid: 42,
            frame: Rect::new((x - 5, 75, 410, 330)),
            content: Rect::new((x, 100, 400, 300)),
            border: 5,
            focused,
            visible: true,
            ..Default::default()
        };
        Snapshot {
            // Given right to left; the tree goes by output order as given.
            outputs: vec![output(0, 0, false), output(1, 1280, true)],
            windows: vec![window(7, 100, false), window(8, 1400, true)],
        }
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = frame(GET_TREE, b"");
        buf.extend(frame(RUN_COMMAND, b"exit"));
        buf.extend_from_slice(&MAGIC[..3]);
        assert_eq!(take_message(&mut buf), Ok(Some((GET_TREE, vec![]))));
        assert_eq!(
            take_message(&mut buf),
            Ok(Some((RUN_COMMAND, b"exit".to_vec())))
        );
        assert_eq!(take_message(&mut buf), Ok(None));
        let mut bad = b"GET / HTTP/1.1\r\n".to_vec();
        assert_eq!(take_message(&mut bad), Err(()));
    }

    #[test]
    fn tree_puts_windows_on_their_outputs_workspaces() {
        let tree = snapshot().reply(GET_TREE, b"");
        assert_eq!(tree["type"], "root");
        assert_eq!(tree["rect"]["width"], 2560);
        let outputs = tree["nodes"].as_array().unwrap();
        assert_eq!(outputs.len(), 2);
        let workspace = &outputs[1]["nodes"][0];
        assert_eq!(workspace["name"], "2");
        let windows = workspace["floating_nodes"].as_array().unwrap();
        assert_eq!(windows.len(), 1);
        let window = &windows[0];
        assert_eq!(window["name"], "w8");
        assert_eq!(window["focused"], true);
        assert_eq!(window["rect"]["x"], 1395);
        assert_eq!(window["window_rect"]["x"], 5);
        assert_eq!(window["window_rect"]["y"], 25);
        assert_eq!(window["deco_rect"]["height"], 20);
        // Only the window is focused.
        assert_eq!(workspace["focused"], false);
        assert_eq!(outputs[1]["focused"], false);
    }

    #[test]
    fn exactly_one_node_is_focused() {
        fn count(node: &Value) -> usize {
            let own = usize::from(node["focused"] == true);
            let children = ["nodes", "floating_nodes"]
                .iter()
                .filter_map(|k| node[k].as_array())
                .flatten()
                .map(count)
                .sum::<usize>();
            own + children
        }
        let mut snap = snapshot();
        assert_eq!(count(&snap.reply(GET_TREE, b"")), 1);
        // Without a focused window, the workspace of the focused output is.
        for window in &mut snap.windows {
            window.focused = false;
        }
        let tree = snap.reply(GET_TREE, b"");
        assert_eq!(count(&tree), 1);
        assert_eq!(tree["nodes"][1]["nodes"][0]["focused"], true);
    }

    #[test]
    fn outputs_and_workspaces_tell_focus() {
        let snap = snapshot();
        let outputs = snap.reply(GET_OUTPUTS, b"");
        assert_eq!(outputs[0]["focused"], false);
        assert_eq!(outputs[1]["focused"], true);
        assert_eq!(outputs[1]["current_workspace"], "2");
        assert_eq!(outputs[1]["current_mode"]["refresh"], 60000);
        let workspaces = snap.reply(GET_WORKSPACES, b"");
        assert_eq!(workspaces[1]["focused"], true);
        assert_eq!(workspaces[1]["output"], "DP-1");
        assert_eq!(workspaces[1]["rect"]["y"], 30);
    }

    #[test]
    fn commands_fail_one_by_one() {
        let reply = snapshot().reply(RUN_COMMAND, b"focus left; kill");
        assert_eq!(reply.as_array().unwrap().len(), 2);
        assert_eq!(reply[0]["success"], false);
        assert_eq!(
            snapshot().reply(SUBSCRIBE, b"[\"window\"]")["success"],
            false
        );
    }

    #[test]
    fn x11_windows_tell_their_window() {
        let mut snap = snapshot();
        snap.windows[0].x11_window = Some(0x400003);
        let tree = snap.reply(GET_TREE, b"");
        let window = &tree["nodes"][0]["nodes"][0]["floating_nodes"][0];
        assert_eq!(window["shell"], "xwayland");
        assert_eq!(window["window"], 0x400003);
    }
}
