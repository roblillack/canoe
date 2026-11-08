//! wlgamma: exercise zwlr_gamma_control_manager_v1 for compositor tests.
//!
//! `check` runs the protocol's rules against the first output (gamma size,
//! exclusive control, release on destroy and on client exit, bad tables);
//! `set` applies scaled ramps like a night-light tool and reports events.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: wlgamma check
       wlgamma sizes
       wlgamma set [--output NAME] R G B [SECONDS]

Connects to $WAYLAND_DISPLAY.
  check    check the protocol's rules on the first output; exits 1 on failure
  sizes    print each output's name and gamma size, or 'failed'
  set      apply linear ramps scaled by R, G and B (0.0 to 1.0) to the output
           (default: the first) and keep them for SECONDS (default: until
           killed or the control fails); prints 'gamma_size N' and 'failed'";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Event {
    Size(u32),
    Failed,
}

#[derive(Default)]
struct App {
    outputs: Vec<(WlOutput, Option<String>)>,
    events: HashMap<ObjectId, Vec<Event>>,
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

impl Dispatch<ZwlrGammaControlV1, ()> for App {
    fn event(
        app: &mut Self,
        control: &ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => Event::Size(size),
            zwlr_gamma_control_v1::Event::Failed => Event::Failed,
            _ => return,
        };
        app.events.entry(control.id()).or_default().push(event);
    }
}

delegate_noop!(App: ignore ZwlrGammaControlManagerV1);

/// A connection with the gamma manager and the outputs bound.
struct Client {
    conn: Connection,
    queue: EventQueue<App>,
    app: App,
    manager: ZwlrGammaControlManagerV1,
}

impl Client {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
        let (globals, queue) =
            registry_queue_init::<App>(&conn).map_err(|e| format!("registry: {e}"))?;
        let qh = queue.handle();
        let manager: ZwlrGammaControlManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| "no zwlr_gamma_control_manager_v1".to_string())?;
        let mut app = App::default();
        for global in globals.contents().clone_list() {
            if global.interface == WlOutput::interface().name {
                let output: WlOutput =
                    globals
                        .registry()
                        .bind(global.name, global.version.min(4), &qh, ());
                app.outputs.push((output, None));
            }
        }
        if app.outputs.is_empty() {
            return Err("no outputs".into());
        }
        let mut client = Client {
            conn,
            queue,
            app,
            manager,
        };
        client.roundtrip()?;
        Ok(client)
    }

    fn roundtrip(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.app)
            .map(|_| ())
            .map_err(|e| format!("roundtrip: {e}"))
    }

    fn control(&mut self, output: usize) -> Result<ZwlrGammaControlV1> {
        let output = self.app.outputs[output].0.clone();
        let control = self
            .manager
            .get_gamma_control(&output, &self.queue.handle(), ());
        self.roundtrip()?;
        Ok(control)
    }

    fn events(&self, control: &ZwlrGammaControlV1) -> Vec<Event> {
        self.app
            .events
            .get(&control.id())
            .cloned()
            .unwrap_or_default()
    }

    /// The gamma size the control was given, if it didn't fail.
    fn size(&self, control: &ZwlrGammaControlV1) -> Option<u32> {
        match self.events(control).as_slice() {
            [Event::Size(size)] => Some(*size),
            _ => None,
        }
    }

    fn failed(&self, control: &ZwlrGammaControlV1) -> bool {
        self.events(control).contains(&Event::Failed)
    }

    /// The protocol error the compositor sent, if any.
    fn protocol_error(&self) -> Option<String> {
        match self.conn.protocol_error() {
            Some(err) => Some(format!(
                "{}@{}: {}",
                err.object_interface, err.object_id, err.message
            )),
            None => None,
        }
    }
}

/// Ramps of `size` entries, linear but scaled by `factors`.
fn ramps(size: u32, factors: [f64; 3]) -> Vec<u16> {
    let size = size as usize;
    let mut table = Vec::with_capacity(3 * size);
    for factor in factors {
        for i in 0..size {
            let value = i as f64 / (size - 1).max(1) as f64 * 65535.0 * factor;
            table.push(value.round().clamp(0.0, 65535.0) as u16);
        }
    }
    table
}

/// A memfd holding `table`, written (not mapped), so its offset is at the
/// end: the compositor has to read from the start.
fn table_fd(table: &[u16]) -> Result<OwnedFd> {
    let fd = rustix::fs::memfd_create("wlgamma", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| format!("memfd_create: {e}"))?;
    let bytes: Vec<u8> = table.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let mut written = 0;
    while written < bytes.len() {
        written += rustix::io::write(&fd, &bytes[written..]).map_err(|e| format!("write: {e}"))?;
    }
    Ok(fd)
}

struct Checker {
    failures: usize,
}

impl Checker {
    fn check(&mut self, ok: bool, what: &str) {
        if ok {
            println!("ok: {what}");
        } else {
            println!("FAIL: {what}");
            self.failures += 1;
        }
    }
}

fn check() -> Result<bool> {
    let mut c = Checker { failures: 0 };
    let mut one = Client::connect()?;
    let mut two = Client::connect()?;

    let a = one.control(0)?;
    let size = one.size(&a);
    c.check(
        size.is_some_and(|s| s > 0),
        &format!("first control gets a gamma size ({:?})", one.events(&a)),
    );
    let Some(size) = size else {
        return Ok(false);
    };

    a.set_gamma(table_fd(&ramps(size, [1.0, 0.8, 0.6]))?.as_fd());
    one.roundtrip()?;
    c.check(!one.failed(&a), "setting a valid table keeps the control");

    let b = one.control(0)?;
    c.check(
        one.events(&b) == [Event::Failed],
        "a second control of the same client fails",
    );
    let x = two.control(0)?;
    c.check(
        two.events(&x) == [Event::Failed],
        "another client's control fails while one exists",
    );
    b.destroy();
    x.destroy();

    if one.app.outputs.len() > 1 {
        let other = one.control(1)?;
        c.check(
            one.size(&other).is_some(),
            "a control of another output works meanwhile",
        );
        other.destroy();
    }

    a.destroy();
    one.roundtrip()?;
    let d = two.control(0)?;
    c.check(
        two.size(&d) == Some(size),
        "destroying the control frees the output",
    );

    // The compositor reads the whole table from the start of the file.
    d.set_gamma(table_fd(&ramps(size, [1.0, 1.0, 1.0])[..3 * size as usize - 1])?.as_fd());
    let result = two.queue.roundtrip(&mut two.app);
    let error = two.protocol_error();
    c.check(
        result.is_err()
            && error
                .as_deref()
                .is_some_and(|e| e.contains("zwlr_gamma_control_v1")),
        &format!(
            "a short table is a protocol error ({})",
            error.as_deref().unwrap_or("none")
        ),
    );
    drop(two);

    let e = one.control(0)?;
    c.check(
        one.size(&e) == Some(size),
        "a disconnected client's control frees the output",
    );

    // A pipe can't be read from the start.
    let (reader, writer) = rustix::pipe::pipe().map_err(|e| format!("pipe: {e}"))?;
    e.set_gamma(reader.as_fd());
    one.roundtrip()?;
    drop(writer);
    c.check(one.failed(&e), "an unreadable table fails the control");
    e.set_gamma(table_fd(&ramps(size, [1.0, 1.0, 1.0]))?.as_fd());
    one.roundtrip()?;
    c.check(
        one.protocol_error().is_none(),
        "a failed control ignores requests",
    );
    e.destroy();

    let f = one.control(0)?;
    c.check(
        one.size(&f) == Some(size),
        "a failed control frees the output",
    );
    f.destroy();
    one.roundtrip()?;

    Ok(c.failures == 0)
}

fn sizes() -> Result<()> {
    let mut client = Client::connect()?;
    for index in 0..client.app.outputs.len() {
        let control = client.control(index)?;
        let name = client.app.outputs[index]
            .1
            .clone()
            .unwrap_or_else(|| format!("#{index}"));
        match client.size(&control) {
            Some(size) => println!("{name}\t{size}"),
            None => println!("{name}\tfailed"),
        }
        control.destroy();
        client.roundtrip()?;
    }
    Ok(())
}

fn set(args: &[String]) -> Result<()> {
    let mut args = args.iter();
    let mut output_name = None;
    let mut numbers = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--output" {
            output_name = Some(args.next().ok_or("--output needs a name")?.clone());
        } else {
            numbers.push(
                arg.parse::<f64>()
                    .map_err(|_| format!("bad number {arg:?}"))?,
            );
        }
    }
    let (factors, seconds) = match numbers.as_slice() {
        [r, g, b] => ([*r, *g, *b], None),
        [r, g, b, s] => ([*r, *g, *b], Some(*s)),
        _ => return Err(USAGE.into()),
    };

    let mut client = Client::connect()?;
    let index = match output_name {
        Some(name) => client
            .app
            .outputs
            .iter()
            .position(|(_, n)| n.as_deref() == Some(name.as_str()))
            .ok_or(format!("no output {name}"))?,
        None => 0,
    };
    let control = client.control(index)?;
    let Some(size) = client.size(&control) else {
        println!("failed");
        return Ok(());
    };
    println!("gamma_size {size}");
    control.set_gamma(table_fd(&ramps(size, factors))?.as_fd());
    client.roundtrip()?;

    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
    loop {
        if client.failed(&control) {
            println!("failed");
            return Ok(());
        }
        if let Some(err) = client.protocol_error() {
            return Err(format!("protocol error: {err}"));
        }
        let timeout = match deadline {
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(left) => left.min(Duration::from_millis(100)),
                None => break,
            },
            None => Duration::from_millis(100),
        };
        client.conn.flush().map_err(|e| format!("flush: {e}"))?;
        if let Some(guard) = client.queue.prepare_read() {
            let fd = guard.connection_fd();
            let mut fds = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN,
            )];
            let ts = rustix::event::Timespec {
                tv_sec: timeout.as_secs() as i64,
                tv_nsec: timeout.subsec_nanos() as i64,
            };
            let readable = rustix::event::poll(&mut fds, Some(&ts)).is_ok_and(|n| n > 0);
            if readable {
                match guard.read() {
                    Ok(_) | Err(WaylandError::Io(_)) => {}
                    Err(err) => return Err(format!("read: {err}")),
                }
            }
        }
        client
            .queue
            .dispatch_pending(&mut client.app)
            .map_err(|e| format!("dispatch: {e}"))?;
    }
    control.destroy();
    client.roundtrip()?;
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("check") => check().map(|ok| {
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }),
        Some("sizes") => sizes().map(|_| ExitCode::SUCCESS),
        Some("set") => set(&args[1..]).map(|_| ExitCode::SUCCESS),
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("wlgamma: {err}");
            ExitCode::from(2)
        }
    }
}
