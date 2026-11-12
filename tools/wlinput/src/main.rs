//! wlinput: scripted input injection and toplevel inspection for Wayland
//! compositor tests.
//!
//! Only standard protocols are used (zwlr_virtual_pointer_v1,
//! zwp_virtual_keyboard_v1, ext_foreign_toplevel_list_v1, wl_output and
//! zxdg_output_v1) so the same scripts run against River and other
//! compositors that implement them.

use std::collections::HashMap;
use std::io::{BufRead, Write as _};
use std::os::fd::AsFd;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: wlinput [COMMAND [ARG...]]          run one command
       wlinput 'CMD ARGS' 'CMD ARGS' ...   run several commands (first arg contains a space)
       wlinput [-]                         read commands from stdin, one per line

Connects to $WAYLAND_DISPLAY. Commands:
  move X Y                        absolute pointer motion (global logical coordinates)
  rel DX DY                       relative pointer motion
  drag X1 Y1 X2 Y2 [STEPS] [BTN]  move, press, move in STEPS steps (20), release
  btn down|up|click BTN           BTN: left, right, middle or numeric evdev code
  scroll DY [DX]                  wheel scroll by DY (and DX) detents; positive = down/right
  key COMBO                       press and release e.g. super+shift+Return, Escape, a
  keydown KEY / keyup KEY         hold / release a key or modifier (super, shift, ctrl, alt)
  type TEXT                       type plain ASCII text
  sleep MS                        wait MS milliseconds
  toplevels                       list toplevels: identifier<TAB>app_id<TAB>title
  wait-toplevel APP_ID [MS]       wait (default 5000ms) until a toplevel with APP_ID exists;
                                  prints its identifier
  wait-gone APP_ID [MS]           wait until no toplevel with APP_ID exists
  devices [MS]                    create the virtual pointer and keyboard now, then wait
                                  MS (100) so clients can bind the new seat capabilities
  outputs                         print the logical output layout

Blank lines and lines starting with # are ignored. Keys still held at exit are released.
After input was sent, wlinput waits $WLINPUT_LINGER_MS (default 50) before exiting so
the compositor can deliver events it held back before the virtual devices go away.";

// Linux evdev button codes.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

// ---------------------------------------------------------------------------
// Wayland state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Output {
    geometry_pos: (i32, i32),
    transform_swaps: bool,
    mode_size: (i32, i32),
    scale: i32,
    logical_pos: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
    name: Option<String>,
}

impl Output {
    /// Logical rectangle (x, y, width, height) in the global compositor space.
    fn logical_rect(&self) -> (i32, i32, i32, i32) {
        let (mut w, mut h) = self.mode_size;
        if self.transform_swaps {
            std::mem::swap(&mut w, &mut h);
        }
        let scale = self.scale.max(1);
        let (w, h) = self.logical_size.unwrap_or((w / scale, h / scale));
        let (x, y) = self.logical_pos.unwrap_or(self.geometry_pos);
        (x, y, w, h)
    }
}

#[derive(Default, Clone)]
struct Toplevel {
    identifier: String,
    app_id: String,
    title: String,
    done: bool,
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    toplevels: HashMap<ObjectId, Toplevel>,
    toplevel_order: Vec<ObjectId>,
}

impl State {
    fn visible_toplevels(&self) -> impl Iterator<Item = &Toplevel> {
        self.toplevel_order
            .iter()
            .filter_map(|id| self.toplevels.get(id))
            .filter(|t| t.done)
    }
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
        // Globals appearing later are not interesting for a short-lived tool.
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
        let out = &mut state.outputs[*idx];
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                out.geometry_pos = (x, y);
                out.transform_swaps = matches!(
                    transform,
                    WEnum::Value(
                        wl_output::Transform::_90
                            | wl_output::Transform::_270
                            | wl_output::Transform::Flipped90
                            | wl_output::Transform::Flipped270
                    )
                );
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                let current = match flags {
                    WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                    WEnum::Unknown(bits) => bits & 1 != 0,
                };
                if current {
                    out.mode_size = (width, height);
                }
            }
            wl_output::Event::Scale { factor } => out.scale = factor,
            wl_output::Event::Name { name } => out.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let out = &mut state.outputs[*idx];
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => out.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                out.logical_size = Some((width, height))
            }
            zxdg_output_v1::Event::Name { name } => {
                out.name.get_or_insert(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.insert(toplevel.id(), Toplevel::default());
            state.toplevel_order.push(toplevel.id());
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        if let ext_foreign_toplevel_handle_v1::Event::Closed = event {
            state.toplevels.remove(&id);
            state.toplevel_order.retain(|i| *i != id);
            handle.destroy();
            return;
        }
        let Some(t) = state.toplevels.get_mut(&id) else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                t.identifier = identifier
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => t.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => t.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Done => t.done = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ZxdgOutputManagerV1);
delegate_noop!(State: ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ZwlrVirtualPointerV1);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct ResolvedKey {
    /// xkb keycode (evdev code + 8).
    keycode: u32,
    /// Shift must be held for the key to produce the requested keysym.
    needs_shift: bool,
    /// Modifier mask bit this key controls, if it is a modifier key.
    mod_mask: u32,
}

struct Keyboard {
    vk: ZwpVirtualKeyboardV1,
    keymap: xkb::Keymap,
    shift: u32,
    ctrl: u32,
    alt: u32,
    logo: u32,
    /// Currently depressed modifiers, as sent to the compositor.
    depressed: u32,
    /// Keys currently held down (xkb keycodes), with their modifier bits.
    held: Vec<(u32, u32)>,
}

impl Keyboard {
    fn new(vk: ZwpVirtualKeyboardV1) -> Result<Self> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or("failed to compile the xkb keymap (evdev/us)")?;
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let mut bytes = text.into_bytes();
        bytes.push(0);
        let fd = rustix::fs::memfd_create("wlinput-keymap", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|e| format!("memfd_create: {e}"))?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&bytes)
            .map_err(|e| format!("writing keymap: {e}"))?;
        vk.keymap(
            1, // WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1
            file.as_fd(),
            bytes.len() as u32,
        );
        let bit = |name: &str| {
            let idx = keymap.mod_get_index(name);
            if idx == xkb::MOD_INVALID {
                0
            } else {
                1u32 << idx
            }
        };
        Ok(Keyboard {
            shift: bit(xkb::MOD_NAME_SHIFT),
            ctrl: bit(xkb::MOD_NAME_CTRL),
            alt: bit(xkb::MOD_NAME_ALT),
            logo: bit(xkb::MOD_NAME_LOGO),
            vk,
            keymap,
            depressed: 0,
            held: Vec::new(),
        })
    }

    /// Find a keycode producing `sym` with no modifiers or with only Shift.
    fn find_keysym(&self, sym: xkb::Keysym) -> Option<ResolvedKey> {
        let mut state = xkb::State::new(&self.keymap);
        for (needs_shift, mask) in [(false, 0), (true, self.shift)] {
            state.update_mask(mask, 0, 0, 0, 0, 0);
            let min = self.keymap.min_keycode().raw();
            let max = self.keymap.max_keycode().raw();
            for kc in min..=max {
                if state.key_get_one_sym(xkb::Keycode::new(kc)) == sym {
                    return Some(ResolvedKey {
                        keycode: kc,
                        needs_shift,
                        mod_mask: self.modifier_mask_for(sym),
                    });
                }
            }
        }
        None
    }

    fn modifier_mask_for(&self, sym: xkb::Keysym) -> u32 {
        use xkb::Keysym as K;
        match sym {
            K::Shift_L | K::Shift_R => self.shift,
            K::Control_L | K::Control_R => self.ctrl,
            K::Alt_L | K::Alt_R | K::Meta_L | K::Meta_R => self.alt,
            K::Super_L | K::Super_R | K::Hyper_L | K::Hyper_R => self.logo,
            _ => 0,
        }
    }

    fn resolve(&self, name: &str) -> Result<ResolvedKey> {
        let alias = match name.to_ascii_lowercase().as_str() {
            "super" | "logo" | "win" | "mod4" | "meta" => Some("Super_L"),
            "shift" => Some("Shift_L"),
            "ctrl" | "control" => Some("Control_L"),
            "alt" | "mod1" => Some("Alt_L"),
            "enter" => Some("Return"),
            "esc" => Some("Escape"),
            "del" => Some("Delete"),
            "pgup" => Some("Prior"),
            "pgdn" | "pgdown" => Some("Next"),
            _ => None,
        };
        let name = alias.unwrap_or(name);
        let mut sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
        if sym == xkb::Keysym::NoSymbol {
            sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
        }
        if sym == xkb::Keysym::NoSymbol {
            let mut chars = name.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                sym = xkb::utf32_to_keysym(c as u32);
            }
        }
        if sym == xkb::Keysym::NoSymbol {
            return Err(format!("unknown key name: {name:?}"));
        }
        self.find_keysym(sym)
            .ok_or_else(|| format!("key {name:?} is not reachable on the US keymap"))
    }

    fn send_key(&mut self, time: u32, key: ResolvedKey, pressed: bool) {
        self.vk.key(time, key.keycode - 8, pressed as u32);
        if pressed {
            self.held.push((key.keycode, key.mod_mask));
        } else if let Some(pos) = self.held.iter().position(|(kc, _)| *kc == key.keycode) {
            self.held.remove(pos);
        }
        if key.mod_mask != 0 {
            // Like a real keyboard: the modifier key event is followed by the
            // resulting modifier state. A modifier stays depressed while any
            // key carrying it is still held.
            let depressed = self.held.iter().fold(0, |acc, (_, m)| acc | m);
            if depressed != self.depressed {
                self.depressed = depressed;
                self.vk.modifiers(depressed, 0, 0, 0);
            }
        }
    }

    fn release_all(&mut self, time: u32) {
        while let Some(&(keycode, mod_mask)) = self.held.last() {
            self.send_key(
                time,
                ResolvedKey {
                    keycode,
                    needs_shift: false,
                    mod_mask,
                },
                false,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

struct Driver {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    globals: GlobalList,
    seat: Option<WlSeat>,
    pointer: Option<ZwlrVirtualPointerV1>,
    keyboard: Option<Keyboard>,
    toplevel_list: Option<ExtForeignToplevelListV1>,
    start: Instant,
}

fn wl_err(e: impl std::fmt::Display) -> String {
    format!("wayland: {e}")
}

impl Driver {
    fn connect() -> Result<Self> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("WAYLAND_DISPLAY is not set; refusing to guess a compositor".into());
        }
        let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(wl_err)?;
        let qh = queue.handle();
        let mut state = State::default();

        let seat: Option<WlSeat> = globals.bind(&qh, 1..=1, ()).ok();
        let toplevel_list = globals.bind(&qh, 1..=1, ()).ok();

        let xdg_mgr: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 2..=3, ()).ok();
        let registry = globals.registry();
        let output_globals: Vec<(u32, u32)> = globals.contents().with_list(|list| {
            list.iter()
                .filter(|g| g.interface == WlOutput::interface().name)
                .map(|g| (g.name, g.version))
                .collect()
        });
        for (name, version) in output_globals {
            let idx = state.outputs.len();
            state.outputs.push(Output {
                scale: 1,
                ..Default::default()
            });
            let output: WlOutput = registry.bind(name, version.min(4), &qh, idx);
            if let Some(mgr) = &xdg_mgr {
                mgr.get_xdg_output(&output, &qh, idx);
            }
        }

        let mut driver = Driver {
            conn,
            queue,
            qh,
            state,
            globals,
            seat,
            pointer: None,
            keyboard: None,
            toplevel_list,
            start: Instant::now(),
        };
        driver.roundtrip()?;
        // Second roundtrip so toplevel handle events following creation arrive.
        driver.roundtrip()?;
        Ok(driver)
    }

    fn roundtrip(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.state)
            .map(|_| ())
            .map_err(wl_err)
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn seat(&self) -> Result<&WlSeat> {
        self.seat
            .as_ref()
            .ok_or_else(|| "compositor has no wl_seat".to_string())
    }

    fn pointer(&mut self) -> Result<ZwlrVirtualPointerV1> {
        if self.pointer.is_none() {
            let mgr: ZwlrVirtualPointerManagerV1 =
                self.globals.bind(&self.qh, 1..=2, ()).map_err(|_| {
                    "compositor does not support zwlr_virtual_pointer_manager_v1".to_string()
                })?;
            let seat = self.seat()?.clone();
            self.pointer = Some(mgr.create_virtual_pointer(Some(&seat), &self.qh, ()));
        }
        Ok(self.pointer.clone().unwrap())
    }

    fn keyboard(&mut self) -> Result<&mut Keyboard> {
        if self.keyboard.is_none() {
            let mgr: ZwpVirtualKeyboardManagerV1 =
                self.globals.bind(&self.qh, 1..=1, ()).map_err(|_| {
                    "compositor does not support zwp_virtual_keyboard_manager_v1".to_string()
                })?;
            let seat = self.seat()?.clone();
            let vk = mgr.create_virtual_keyboard(&seat, &self.qh, ());
            self.keyboard = Some(Keyboard::new(vk)?);
            self.roundtrip()?;
        }
        Ok(self.keyboard.as_mut().unwrap())
    }

    fn toplevel_list(&self) -> Result<()> {
        if self.toplevel_list.is_none() {
            return Err("compositor does not support ext_foreign_toplevel_list_v1".into());
        }
        Ok(())
    }

    /// Bounding box (min_x, min_y, width, height) of all outputs.
    fn layout(&self) -> Result<(i32, i32, i32, i32)> {
        let rects: Vec<_> = self
            .state
            .outputs
            .iter()
            .map(Output::logical_rect)
            .collect();
        if rects.is_empty() {
            return Err("compositor advertises no wl_output".into());
        }
        let min_x = rects.iter().map(|r| r.0).min().unwrap();
        let min_y = rects.iter().map(|r| r.1).min().unwrap();
        let max_x = rects.iter().map(|r| r.0 + r.2).max().unwrap();
        let max_y = rects.iter().map(|r| r.1 + r.3).max().unwrap();
        if max_x <= min_x || max_y <= min_y {
            return Err("output layout has zero size".into());
        }
        Ok((min_x, min_y, max_x - min_x, max_y - min_y))
    }

    /// Wait for events for at most `timeout`, dispatching whatever arrives.
    fn dispatch_timeout(&mut self, timeout: Duration) -> Result<()> {
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
                let ts = rustix::event::Timespec {
                    tv_sec: timeout.as_secs() as _,
                    tv_nsec: timeout.subsec_nanos() as _,
                };
                match rustix::event::poll(&mut fds, Some(&ts)) {
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

    fn sleep(&mut self, ms: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            self.dispatch_timeout(deadline - now)?;
        }
    }

    fn wait_for(&mut self, timeout_ms: u64, mut pred: impl FnMut(&State) -> bool) -> Result<bool> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        self.roundtrip()?;
        loop {
            if pred(&self.state) {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            self.dispatch_timeout((deadline - now).min(Duration::from_millis(50)))?;
        }
    }

    // --- pointer -----------------------------------------------------------

    fn motion_absolute(&mut self, x: f64, y: f64) -> Result<()> {
        let (min_x, min_y, w, h) = self.layout()?;
        let ptr = self.pointer()?;
        // Use a sub-pixel grid so fractional coordinates survive the integer
        // protocol representation.
        const SUB: f64 = 16.0;
        let px = ((x - min_x as f64) * SUB)
            .round()
            .clamp(0.0, (w as f64) * SUB - 1.0);
        let py = ((y - min_y as f64) * SUB)
            .round()
            .clamp(0.0, (h as f64) * SUB - 1.0);
        let time = self.time();
        ptr.motion_absolute(
            time,
            px as u32,
            py as u32,
            (w as f64 * SUB) as u32,
            (h as f64 * SUB) as u32,
        );
        ptr.frame();
        Ok(())
    }

    fn button(&mut self, button: u32, pressed: bool) -> Result<()> {
        let ptr = self.pointer()?;
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        ptr.button(self.time(), button, state);
        ptr.frame();
        Ok(())
    }

    // --- commands ----------------------------------------------------------

    fn run(&mut self, cmd: &str, rest: &str) -> Result<()> {
        let args: Vec<&str> = rest.split_whitespace().collect();
        let argn = |min: usize, max: usize| -> Result<()> {
            if args.len() < min || args.len() > max {
                Err(format!(
                    "{cmd}: expected {min}..={max} argument(s), got {}",
                    args.len()
                ))
            } else {
                Ok(())
            }
        };
        match cmd {
            "move" => {
                argn(2, 2)?;
                self.motion_absolute(num(args[0])?, num(args[1])?)?;
            }
            "rel" => {
                argn(2, 2)?;
                let (dx, dy) = (num(args[0])?, num(args[1])?);
                let ptr = self.pointer()?;
                ptr.motion(self.time(), dx, dy);
                ptr.frame();
            }
            "drag" => {
                argn(4, 6)?;
                let (x1, y1, x2, y2) = (num(args[0])?, num(args[1])?, num(args[2])?, num(args[3])?);
                let steps: u32 = args
                    .get(4)
                    .map(|s| int(s))
                    .transpose()?
                    .unwrap_or(20)
                    .max(1);
                let btn = args
                    .get(5)
                    .map(|s| button_code(s))
                    .transpose()?
                    .unwrap_or(BTN_LEFT);
                self.motion_absolute(x1, y1)?;
                self.roundtrip()?;
                self.button(btn, true)?;
                self.roundtrip()?;
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    self.motion_absolute(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t)?;
                    self.roundtrip()?;
                }
                self.button(btn, false)?;
            }
            "btn" | "button" => {
                argn(2, 2)?;
                let code = button_code(args[1])?;
                match args[0] {
                    "down" | "press" => self.button(code, true)?,
                    "up" | "release" => self.button(code, false)?,
                    "click" => {
                        self.button(code, true)?;
                        self.roundtrip()?;
                        self.button(code, false)?;
                    }
                    other => return Err(format!("btn: expected down|up|click, got {other:?}")),
                }
            }
            "scroll" => {
                argn(1, 2)?;
                let dy = num(args[0])?;
                let dx = args.get(1).map(|s| num(s)).transpose()?.unwrap_or(0.0);
                let ptr = self.pointer()?;
                let time = self.time();
                ptr.axis_source(wl_pointer::AxisSource::Wheel);
                for (axis, v) in [
                    (wl_pointer::Axis::VerticalScroll, dy),
                    (wl_pointer::Axis::HorizontalScroll, dx),
                ] {
                    if v != 0.0 {
                        ptr.axis_discrete(time, axis, v * 15.0, v.round() as i32);
                    }
                }
                ptr.frame();
            }
            "key" => {
                argn(1, 1)?;
                self.key_combo(args[0])?;
            }
            "keydown" | "keyup" => {
                argn(1, 1)?;
                let time = self.time();
                let kb = self.keyboard()?;
                let key = kb.resolve(args[0])?;
                kb.send_key(time, key, cmd == "keydown");
            }
            "type" => self.type_text(rest)?,
            "sleep" => {
                argn(1, 1)?;
                self.sleep(int(args[0])? as u64)?;
                return Ok(());
            }
            "toplevels" => {
                argn(0, 0)?;
                self.toplevel_list()?;
                self.roundtrip()?;
                let mut out = std::io::stdout().lock();
                for t in self.state.visible_toplevels() {
                    let _ = writeln!(
                        out,
                        "{}\t{}\t{}",
                        clean(&t.identifier),
                        clean(&t.app_id),
                        clean(&t.title)
                    );
                }
                let _ = out.flush();
                return Ok(());
            }
            "wait-toplevel" | "wait-gone" => {
                argn(1, 2)?;
                self.toplevel_list()?;
                let app_id = args[0].to_string();
                let timeout = args.get(1).map(|s| int(s)).transpose()?.unwrap_or(5000) as u64;
                let gone = cmd == "wait-gone";
                let ok = self.wait_for(timeout, |s| {
                    s.visible_toplevels().any(|t| t.app_id == app_id) != gone
                })?;
                if !ok {
                    return Err(format!("{cmd} {app_id}: timed out after {timeout}ms"));
                }
                if !gone {
                    if let Some(t) = self.state.visible_toplevels().find(|t| t.app_id == app_id) {
                        println!("{}", t.identifier);
                    }
                }
                return Ok(());
            }
            "devices" => {
                argn(0, 1)?;
                let settle = args.first().map(|s| int(s)).transpose()?.unwrap_or(100);
                self.pointer()?;
                self.keyboard()?;
                self.roundtrip()?;
                // Give other clients time to bind the seat capabilities that
                // appear with the new devices before any input is sent.
                self.sleep(settle as u64)?;
                return Ok(());
            }
            "outputs" => {
                argn(0, 0)?;
                self.roundtrip()?;
                for o in &self.state.outputs {
                    let (x, y, w, h) = o.logical_rect();
                    println!("{}\t{x},{y}\t{w}x{h}", o.name.as_deref().unwrap_or("?"));
                }
                if let Ok((x, y, w, h)) = self.layout() {
                    println!("layout\t{x},{y}\t{w}x{h}");
                }
                return Ok(());
            }
            "pos" => {
                return Err("pos: the cursor position cannot be queried via these protocols".into())
            }
            _ => return Err(format!("unknown command: {cmd:?}")),
        }
        // Every input command is flushed and synchronised so its effect is
        // ordered before anything another tool does afterwards.
        self.roundtrip()
    }

    fn key_combo(&mut self, combo: &str) -> Result<()> {
        // "ctrl++" / "+" mean the plus key.
        let (mods_part, key_part) = if combo == "+" {
            ("", "plus")
        } else if let Some(m) = combo.strip_suffix("++") {
            (m, "plus")
        } else {
            match combo.rsplit_once('+') {
                Some((m, k)) => (m, k),
                None => ("", combo),
            }
        };
        let time = self.time();
        let kb = self.keyboard()?;
        let mut mods = Vec::new();
        for m in mods_part.split('+').filter(|s| !s.is_empty()) {
            let key = kb.resolve(m)?;
            if key.mod_mask == 0 {
                return Err(format!("{m:?} is not a modifier (in {combo:?})"));
            }
            mods.push(key);
        }
        let key = kb.resolve(key_part)?;
        if key.needs_shift && !mods.iter().any(|m| m.mod_mask == kb.shift) {
            mods.push(kb.resolve("Shift_L")?);
        }
        for m in &mods {
            kb.send_key(time, *m, true);
        }
        kb.send_key(time, key, true);
        kb.send_key(time, key, false);
        for m in mods.iter().rev() {
            kb.send_key(time, *m, false);
        }
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        let kb = self.keyboard()?;
        let mut keys = Vec::new();
        for c in text.chars() {
            let sym = match c {
                '\n' => xkb::Keysym::Return,
                '\t' => xkb::Keysym::Tab,
                c => xkb::utf32_to_keysym(c as u32),
            };
            let key = kb
                .find_keysym(sym)
                .ok_or_else(|| format!("type: character {c:?} is not on the US keymap"))?;
            keys.push(key);
        }
        let shift = kb.resolve("Shift_L")?;
        for key in keys {
            let time = self.time();
            let kb = self.keyboard.as_mut().unwrap();
            if key.needs_shift {
                kb.send_key(time, shift, true);
            }
            kb.send_key(time, key, true);
            kb.send_key(time, key, false);
            if key.needs_shift {
                kb.send_key(time, shift, false);
            }
            // Keep each character a separate burst so clients with key
            // repeat or batching heuristics see distinct presses.
            self.roundtrip()?;
        }
        Ok(())
    }

    fn finish(&mut self) {
        let time = self.time();
        if let Some(kb) = self.keyboard.as_mut() {
            if !kb.held.is_empty() {
                eprintln!("wlinput: releasing keys still held at exit");
                kb.release_all(time);
            }
        }
        let _ = self.roundtrip();
        // Destroying the virtual devices right away can drop input the
        // compositor is still holding back, e.g. River delays key events
        // while the window manager decides on a modifier binding.
        if self.pointer.is_some() || self.keyboard.is_some() {
            let linger = std::env::var("WLINPUT_LINGER_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50);
            let _ = self.sleep(linger);
        }
        let _ = self.conn.flush();
    }
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n'], " ")
}

fn num(s: &str) -> Result<f64> {
    s.parse().map_err(|_| format!("not a number: {s:?}"))
}

fn int(s: &str) -> Result<u32> {
    s.parse()
        .map_err(|_| format!("not a non-negative integer: {s:?}"))
}

fn button_code(s: &str) -> Result<u32> {
    match s {
        "left" => Ok(BTN_LEFT),
        "right" => Ok(BTN_RIGHT),
        "middle" => Ok(BTN_MIDDLE),
        s => match s.strip_prefix("0x") {
            Some(hex) => u32::from_str_radix(hex, 16),
            None => s.parse(),
        }
        .map_err(|_| format!("unknown button: {s:?}")),
    }
}

/// Split a command line into the command word and the raw remainder.
fn split_command(line: &str) -> (&str, &str) {
    let line = line.trim_start();
    match line.find(char::is_whitespace) {
        Some(i) => {
            let rest = &line[i..];
            // `type` keeps its text verbatim apart from the single separator.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            (&line[..i], rest)
        }
        None => (line, ""),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    // Command source: stdin, several quoted commands, or a single command.
    let from_stdin = args.is_empty() || (args.len() == 1 && args[0] == "-");
    let commands: Box<dyn Iterator<Item = std::io::Result<String>>> = if from_stdin {
        Box::new(std::io::stdin().lock().lines())
    } else if args[0].contains(char::is_whitespace) {
        Box::new(args.clone().into_iter().map(Ok))
    } else {
        Box::new(std::iter::once(Ok(args.join(" "))))
    };

    let mut driver = match Driver::connect() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("wlinput: {e}");
            return ExitCode::from(2);
        }
    };

    let mut code = ExitCode::SUCCESS;
    for (lineno, line) in commands.enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("wlinput: reading stdin: {e}");
                code = ExitCode::from(2);
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (cmd, rest) = split_command(&line);
        let rest = if cmd == "type" { rest } else { rest.trim() };
        if let Err(e) = driver.run(cmd, rest) {
            if from_stdin {
                eprintln!("wlinput: line {}: {e}", lineno + 1);
            } else {
                eprintln!("wlinput: {e}");
            }
            code = ExitCode::FAILURE;
            break;
        }
    }
    driver.finish();
    code
}
