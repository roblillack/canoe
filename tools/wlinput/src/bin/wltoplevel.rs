//! wltoplevel: inspect and control windows through
//! zwlr_foreign_toplevel_manager_v1, like a taskbar, for compositor tests.
//!
//! `list` prints every toplevel once, `watch` prints a toplevel each time
//! its state is complete (`done`), the actions send one request to the
//! toplevels matching a pattern, and `check` runs some of the protocol's
//! rules.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use wayland_client::backend::WaylandError;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1, EVT_TOPLEVEL_OPCODE},
};

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: wltoplevel list
       wltoplevel watch [SECONDS]
       wltoplevel ACTION PATTERN [OUTPUT]
       wltoplevel check

Connects to $WAYLAND_DISPLAY.
  list     print each toplevel: index, app_id, title, states, outputs and
           the index of its parent
  watch    print a toplevel each time it changes, and 'closed N' when it
           goes away, for SECONDS (default: until killed)
  ACTION   activate, minimize, unminimize, maximize, unmaximize, fullscreen
           (on OUTPUT if given), unfullscreen or close, sent to every
           toplevel whose app_id or title contains PATTERN; exits 1 if none
  check    check stop/finished, a second manager seeing the same toplevels,
           and the invalid_rectangle error; exits 1 on failure";

#[derive(Debug, Default, Clone)]
struct Toplevel {
    handle: Option<ZwlrForeignToplevelHandleV1>,
    app_id: String,
    title: String,
    states: Vec<u32>,
    outputs: Vec<WlOutput>,
    parent: Option<ZwlrForeignToplevelHandleV1>,
    done: bool,
    closed: bool,
}

#[derive(Default)]
struct App {
    outputs: Vec<(WlOutput, Option<String>)>,
    toplevels: Vec<Toplevel>,
    finished: usize,
    /// Print each toplevel on `done` (watch).
    print: bool,
}

impl App {
    fn index(&self, handle: &ZwlrForeignToplevelHandleV1) -> Option<usize> {
        self.toplevels
            .iter()
            .position(|t| t.handle.as_ref() == Some(handle))
    }

    fn line(&self, i: usize) -> String {
        let t = &self.toplevels[i];
        let states: Vec<&str> = t
            .states
            .iter()
            .map(|s| match s {
                0 => "maximized",
                1 => "minimized",
                2 => "activated",
                3 => "fullscreen",
                _ => "?",
            })
            .collect();
        let outputs: Vec<String> = t
            .outputs
            .iter()
            .map(|o| {
                self.outputs
                    .iter()
                    .find(|(wl, _)| wl == o)
                    .and_then(|(_, name)| name.clone())
                    .unwrap_or_else(|| "?".into())
            })
            .collect();
        let parent = match &t.parent {
            Some(p) => self.index(p).map_or("?".into(), |i| i.to_string()),
            None => "-".into(),
        };
        format!(
            "{i} app_id={:?} title={:?} states=[{}] outputs=[{}] parent={parent}",
            t.app_id,
            t.title,
            states.join(","),
            outputs.join(",")
        )
    }

    fn open(&self) -> impl Iterator<Item = (usize, &Toplevel)> {
        self.toplevels.iter().enumerate().filter(|(_, t)| !t.closed)
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for App {
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

impl Dispatch<WlOutput, ()> for App {
    fn event(
        app: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(entry) = app.outputs.iter_mut().find(|(o, _)| o == output) {
                entry.1 = Some(name);
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                app.toplevels.push(Toplevel {
                    handle: Some(toplevel),
                    ..Default::default()
                });
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => app.finished += 1,
            _ => {}
        }
    }

    event_created_child!(App, ZwlrForeignToplevelManagerV1, [
        EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        let Some(i) = app.index(handle) else {
            return;
        };
        let t = &mut app.toplevels[i];
        match event {
            Event::Title { title } => t.title = title,
            Event::AppId { app_id } => t.app_id = app_id,
            Event::OutputEnter { output } => t.outputs.push(output),
            Event::OutputLeave { output } => t.outputs.retain(|o| o != &output),
            Event::State { state } => {
                t.states = state
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
            Event::Parent { parent } => t.parent = parent,
            Event::Done => {
                t.done = true;
                if app.print {
                    println!("{}", app.line(i));
                }
            }
            Event::Closed => {
                t.closed = true;
                handle.destroy();
                if app.print {
                    println!("closed {i}");
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(App: ignore WlSeat);
delegate_noop!(App: ignore WlCompositor);
delegate_noop!(App: ignore WlSurface);

struct Session {
    conn: Connection,
    queue: EventQueue<App>,
    app: App,
    manager: ZwlrForeignToplevelManagerV1,
    seat: Option<WlSeat>,
    compositor: Option<WlCompositor>,
}

fn connect() -> Result<Session> {
    let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<App>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let mut app = App::default();
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" {
            let output: WlOutput =
                globals
                    .registry()
                    .bind(global.name, global.version.min(4), &qh, ());
            app.outputs.push((output, None));
        }
    }
    let manager: ZwlrForeignToplevelManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .map_err(|e| format!("zwlr_foreign_toplevel_manager_v1: {e}"))?;
    let seat = globals.bind(&qh, 1..=1, ()).ok();
    let compositor = globals.bind(&qh, 1..=1, ()).ok();
    queue
        .roundtrip(&mut app)
        .map_err(|e| format!("roundtrip: {e}"))?;
    queue
        .roundtrip(&mut app)
        .map_err(|e| format!("roundtrip: {e}"))?;
    Ok(Session {
        conn,
        queue,
        app,
        manager,
        seat,
        compositor,
    })
}

impl Session {
    fn roundtrip(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.app)
            .map(|_| ())
            .map_err(|e| format!("roundtrip: {e}"))
    }

    /// Dispatch events for `duration` (forever if `None`).
    fn run_for(&mut self, duration: Option<Duration>) -> Result<()> {
        let start = Instant::now();
        loop {
            let left = match duration {
                Some(d) => match d.checked_sub(start.elapsed()) {
                    Some(left) => left,
                    None => return Ok(()),
                },
                None => Duration::from_secs(3600),
            };
            self.queue
                .dispatch_pending(&mut self.app)
                .map_err(|e| format!("dispatch: {e}"))?;
            self.conn.flush().map_err(|e| format!("flush: {e}"))?;
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let fd = guard.connection_fd();
            let mut fds = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN,
            )];
            let timeout = rustix::event::Timespec {
                tv_sec: left.as_secs() as i64,
                tv_nsec: left.subsec_nanos() as i64,
            };
            let ready =
                rustix::event::poll(&mut fds, Some(&timeout)).map_err(|e| format!("poll: {e}"))?;
            if ready > 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(format!("read: {e}")),
                }
            }
        }
    }
}

fn list() -> Result<()> {
    let mut s = connect()?;
    for (i, _) in s.app.open() {
        println!("{}", s.app.line(i));
    }
    s.manager.stop();
    s.roundtrip()
}

fn watch(seconds: Option<f64>) -> Result<()> {
    let mut s = connect()?;
    for (i, _) in s.app.open() {
        println!("{}", s.app.line(i));
    }
    s.app.print = true;
    s.run_for(seconds.map(Duration::from_secs_f64))
}

fn action(name: &str, pattern: &str, output: Option<&str>) -> Result<bool> {
    let mut s = connect()?;
    let output = match output {
        Some(name) => Some(
            s.app
                .outputs
                .iter()
                .find(|(_, n)| n.as_deref() == Some(name))
                .map(|(o, _)| o.clone())
                .ok_or_else(|| format!("no output {name}"))?,
        ),
        None => None,
    };
    let matching: Vec<ZwlrForeignToplevelHandleV1> = s
        .app
        .open()
        .filter(|(_, t)| t.app_id.contains(pattern) || t.title.contains(pattern))
        .filter_map(|(_, t)| t.handle.clone())
        .collect();
    for handle in &matching {
        match name {
            "activate" => {
                let seat = s.seat.as_ref().ok_or("no wl_seat")?;
                handle.activate(seat);
            }
            "minimize" => handle.set_minimized(),
            "unminimize" => handle.unset_minimized(),
            "maximize" => handle.set_maximized(),
            "unmaximize" => handle.unset_maximized(),
            "fullscreen" => handle.set_fullscreen(output.as_ref()),
            "unfullscreen" => handle.unset_fullscreen(),
            "close" => handle.close(),
            _ => return Err(format!("unknown action {name}")),
        }
    }
    s.roundtrip()?;
    Ok(!matching.is_empty())
}

fn check() -> Result<bool> {
    let mut ok = true;
    let mut fail = |msg: String| {
        println!("FAIL {msg}");
        ok = false;
    };

    // Every toplevel arrives complete, and a second manager sees the same.
    let mut a = connect()?;
    let incomplete = a.app.open().filter(|(_, t)| !t.done).count();
    if incomplete > 0 {
        fail(format!(
            "{incomplete} toplevels without done after a roundtrip"
        ));
    }
    let first: Vec<(String, String)> = a
        .app
        .open()
        .map(|(_, t)| (t.app_id.clone(), t.title.clone()))
        .collect();
    let mut b = connect()?;
    let second: Vec<(String, String)> = b
        .app
        .open()
        .map(|(_, t)| (t.app_id.clone(), t.title.clone()))
        .collect();
    if first != second {
        fail(format!("managers disagree: {first:?} vs {second:?}"));
    }
    println!("toplevels {}", first.len());

    // stop is answered by finished, once.
    a.manager.stop();
    a.roundtrip()?;
    if a.app.finished != 1 {
        fail(format!("finished {} times after stop", a.app.finished));
    }

    // A negative rectangle is a protocol error.
    let handle = b.app.open().find_map(|(_, t)| t.handle.clone());
    if let (Some(handle), Some(compositor)) = (handle, b.compositor.clone()) {
        let surface = compositor.create_surface(&b.queue.handle(), ());
        handle.set_rectangle(&surface, 0, 0, 10, 10);
        handle.set_rectangle(&surface, 0, 0, 0, 0);
        if let Err(e) = b.roundtrip() {
            fail(format!("valid rectangles rejected: {e}"));
        }
        handle.set_rectangle(&surface, 0, 0, -1, 10);
        match b.roundtrip() {
            Err(_) => match b.conn.protocol_error() {
                Some(e)
                    if e.code == 0 && e.object_interface == "zwlr_foreign_toplevel_handle_v1" =>
                {
                    println!("invalid_rectangle ok")
                }
                other => fail(format!("unexpected error {other:?}")),
            },
            Ok(()) => fail("negative rectangle accepted".into()),
        }
    } else {
        println!("no toplevel, rectangle check skipped");
    }
    Ok(ok)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["list"] => list().map(|_| true),
        ["watch"] => watch(None).map(|_| true),
        ["watch", secs] => match secs.parse() {
            Ok(secs) => watch(Some(secs)).map(|_| true),
            Err(_) => Err(USAGE.into()),
        },
        ["check"] => check(),
        [name, pattern] => action(name, pattern, None),
        [name, pattern, output] => action(name, pattern, Some(output)),
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("no matching toplevel");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
