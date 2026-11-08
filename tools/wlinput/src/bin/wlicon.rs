//! wlicon: check a compositor's xdg-toplevel-icon-v1.
//!
//! `check` binds the manager and reads the preferred sizes, sets icons of
//! every kind (images, a name, empty, none) on toplevels, before and after
//! they map, and checks the protocol errors. Whether the compositor shows
//! the icons can't be seen through the protocol; e2e scenario 21 looks at
//! them.

use std::io::Write as _;
use std::os::fd::AsFd;
use std::process::ExitCode;

use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_registry::WlRegistry, wl_shm,
    wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::XdgToplevel,
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols::xdg::toplevel_icon::v1::client::{
    xdg_toplevel_icon_manager_v1::{self, XdgToplevelIconManagerV1},
    xdg_toplevel_icon_v1::XdgToplevelIconV1,
};

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: wlicon check

Connects to $WAYLAND_DISPLAY and checks xdg-toplevel-icon-v1. Exits 1 on
failure.";

#[derive(Default)]
struct App {
    sizes: Vec<i32>,
    done: bool,
    configure: Option<u32>,
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

impl Dispatch<XdgWmBase, ()> for App {
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

impl Dispatch<XdgSurface, ()> for App {
    fn event(
        app: &mut Self,
        _: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            app.configure = Some(serial);
        }
    }
}

impl Dispatch<XdgToplevelIconManagerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &XdgToplevelIconManagerV1,
        event: xdg_toplevel_icon_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel_icon_manager_v1::Event::IconSize { size } => app.sizes.push(size),
            xdg_toplevel_icon_manager_v1::Event::Done => app.done = true,
            _ => {}
        }
    }
}

delegate_noop!(App: WlCompositor);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: ignore WlShm);
delegate_noop!(App: WlShmPool);
delegate_noop!(App: ignore WlBuffer);
delegate_noop!(App: ignore XdgToplevel);
delegate_noop!(App: XdgToplevelIconV1);

struct Client {
    conn: Connection,
    queue: EventQueue<App>,
    app: App,
    globals: GlobalList,
    manager: XdgToplevelIconManagerV1,
}

/// A toplevel, mapped or not.
struct Window {
    surface: WlSurface,
    toplevel: XdgToplevel,
    _xdg: XdgSurface,
}

impl Client {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
        let (globals, mut queue) =
            registry_queue_init::<App>(&conn).map_err(|e| format!("registry: {e}"))?;
        let manager = globals
            .bind(&queue.handle(), 1..=1, ())
            .map_err(|_| "compositor does not support xdg_toplevel_icon_manager_v1")?;
        let mut app = App::default();
        queue
            .roundtrip(&mut app)
            .map_err(|e| format!("roundtrip: {e}"))?;
        Ok(Self {
            conn,
            queue,
            app,
            globals,
            manager,
        })
    }

    fn bind<I>(&self, version: u32) -> Result<I>
    where
        I: Proxy + 'static,
        App: Dispatch<I, ()>,
    {
        self.globals
            .bind(&self.queue.handle(), version..=version, ())
            .map_err(|_| format!("compositor does not support {}", I::interface().name))
    }

    /// Roundtrip, returning the protocol error that ended the connection.
    fn roundtrip_error(&mut self) -> Option<String> {
        let _ = self.queue.roundtrip(&mut self.app);
        self.conn
            .protocol_error()
            .map(|e| format!("{}@{}: {}", e.object_interface, e.code, e.message))
    }

    /// A toplevel that made its initial commit; `map` also gives it a buffer.
    fn toplevel(&mut self, title: &str, map: bool) -> Result<Window> {
        let qh = self.queue.handle();
        let compositor: WlCompositor = self.bind(4)?;
        let wm: XdgWmBase = self.bind(1)?;
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_title(title.into());
        toplevel.set_app_id("wlicon".into());
        surface.commit();
        if map {
            self.app.configure = None;
            while self.app.configure.is_none() {
                self.queue
                    .blocking_dispatch(&mut self.app)
                    .map_err(|e| format!("dispatch: {e}"))?;
            }
            xdg.ack_configure(self.app.configure.unwrap());
            let buffer = self.buffer(100, 100, 0x80)?;
            surface.attach(Some(&buffer), 0, 0);
            surface.commit();
        }
        Ok(Window {
            surface,
            toplevel,
            _xdg: xdg,
        })
    }

    /// A wl_shm ARGB buffer filled with `byte`.
    fn buffer(&self, width: i32, height: i32, byte: u8) -> Result<WlBuffer> {
        let qh = self.queue.handle();
        let shm: WlShm = self.bind(1)?;
        let len = width * height * 4;
        let fd = rustix::fs::memfd_create("wlicon", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|e| format!("memfd_create: {e}"))?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&vec![byte; len as usize])
            .map_err(|e| format!("writing buffer: {e}"))?;
        let pool = shm.create_pool(file.as_fd(), len, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            &qh,
            (),
        );
        pool.destroy();
        Ok(buffer)
    }

    fn icon(&self) -> XdgToplevelIconV1 {
        self.manager.create_icon(&self.queue.handle(), ())
    }
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

    /// Check that `client`'s requests so far were fine.
    fn fine(&mut self, client: &mut Client, what: &str) {
        let err = client.roundtrip_error();
        self.check(
            err.is_none(),
            &format!("{what} ({})", err.as_deref().unwrap_or("no error")),
        );
    }

    /// Check that `client`'s requests ended in the `xdg_toplevel_icon_v1`
    /// error `code`.
    fn error(&mut self, client: &mut Client, code: u32, what: &str) {
        let err = client.roundtrip_error();
        let want = format!("xdg_toplevel_icon_v1@{code}:");
        self.check(
            err.as_deref().is_some_and(|e| e.starts_with(&want)),
            &format!("{what} ({})", err.as_deref().unwrap_or("no error")),
        );
    }
}

fn check() -> Result<bool> {
    let mut c = Checker { failures: 0 };
    let mut app = Client::connect()?;
    c.check(app.app.done, "the manager sends done when bound");
    c.check(
        app.app.sizes.iter().all(|&s| s > 0),
        &format!("preferred sizes are positive ({:?})", app.app.sizes),
    );

    // Before the first commit with a buffer.
    let early = app.toplevel("wlicon-early", false)?;
    let icon = app.icon();
    let images: Vec<WlBuffer> = [16, 48, 96]
        .into_iter()
        .map(|size| app.buffer(size, size, 0xc0))
        .collect::<Result<_>>()?;
    for (buffer, scale) in images.iter().zip([1, 1, 2]) {
        icon.add_buffer(buffer, scale);
    }
    // The same size and scale again replaces the first.
    icon.add_buffer(&images[1], 1);
    icon.set_name("wlicon".into());
    app.manager.set_icon(&early.toplevel, Some(&icon));
    // Allowed right after set_icon, before the commit applies it.
    icon.destroy();
    for buffer in &images {
        buffer.destroy();
    }
    early.surface.commit();
    c.fine(&mut app, "setting images and a name before mapping");

    let window = app.toplevel("wlicon-mapped", true)?;
    let named = app.icon();
    named.set_name("utilities-terminal".into());
    app.manager.set_icon(&window.toplevel, Some(&named));
    app.manager.set_icon(&early.toplevel, Some(&named));
    window.surface.commit();
    early.surface.commit();
    c.fine(&mut app, "one icon set on two mapped toplevels");

    let empty = app.icon();
    app.manager.set_icon(&window.toplevel, Some(&empty));
    window.surface.commit();
    c.fine(&mut app, "setting an empty icon");
    app.manager.set_icon(&window.toplevel, None);
    window.surface.commit();
    c.fine(&mut app, "resetting the icon");
    // Without a commit it stays pending; the toplevel then goes away.
    app.manager.set_icon(&window.toplevel, Some(&named));
    window.toplevel.destroy();
    c.fine(&mut app, "destroying a toplevel with an icon pending");

    // Protocol errors, each on a client of its own.
    let mut bad = Client::connect()?;
    let window = bad.toplevel("wlicon-bad", false)?;
    let icon = bad.icon();
    icon.set_name("a".into());
    bad.manager.set_icon(&window.toplevel, Some(&icon));
    icon.set_name("b".into());
    c.error(&mut bad, 2, "set_name after set_icon is immutable");

    let mut bad = Client::connect()?;
    let window = bad.toplevel("wlicon-bad", false)?;
    let icon = bad.icon();
    bad.manager.set_icon(&window.toplevel, Some(&icon));
    let buffer = bad.buffer(16, 16, 0xff)?;
    icon.add_buffer(&buffer, 1);
    c.error(&mut bad, 2, "add_buffer after set_icon is immutable");

    let mut bad = Client::connect()?;
    let icon = bad.icon();
    let buffer = bad.buffer(16, 32, 0xff)?;
    icon.add_buffer(&buffer, 1);
    c.error(&mut bad, 1, "a buffer that isn't square is invalid_buffer");

    let mut bad = Client::connect()?;
    let window = bad.toplevel("wlicon-bad", false)?;
    let icon = bad.icon();
    let buffer = bad.buffer(16, 16, 0xff)?;
    icon.add_buffer(&buffer, 1);
    buffer.destroy();
    bad.manager.set_icon(&window.toplevel, Some(&icon));
    c.error(
        &mut bad,
        3,
        "a buffer destroyed before set_icon is no_buffer",
    );

    Ok(c.failures == 0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("check") => check(),
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("wlicon: {err}");
            ExitCode::from(2)
        }
    }
}
