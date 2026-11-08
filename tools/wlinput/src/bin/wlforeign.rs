//! wlforeign: check a compositor's xdg-foreign-unstable-v2.
//!
//! `check` maps toplevels from two clients, exports and imports them, and
//! watches the parents a taskbar (wlr-foreign-toplevel-management) sees:
//! handles, `destroyed` for unknown and revoked handles, parents set and
//! cleared by every way of ending a relationship, the protocol errors, and
//! that a parent loop is ignored.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::fd::AsFd;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_registry::WlRegistry, wl_shm,
    wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::xdg::foreign::zv2::client::{
    zxdg_exported_v2::{self, ZxdgExportedV2},
    zxdg_exporter_v2::ZxdgExporterV2,
    zxdg_imported_v2::{self, ZxdgImportedV2},
    zxdg_importer_v2::ZxdgImporterV2,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::XdgToplevel,
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{ZwlrForeignToplevelManagerV1, EVT_TOPLEVEL_OPCODE},
};

type Result<T> = std::result::Result<T, String>;

const USAGE: &str = "\
usage: wlforeign check

Connects to $WAYLAND_DISPLAY and checks xdg-foreign-unstable-v2; parents
are observed through zwlr_foreign_toplevel_manager_v1. Exits 1 on failure.";

/// How long to wait for the taskbar to see a change.
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct App {
    configures: HashMap<ObjectId, u32>,
    handles: HashMap<ObjectId, String>,
    destroyed: Vec<ObjectId>,
    /// Taskbar view: title and parent of each toplevel handle.
    toplevels: HashMap<ObjectId, (String, Option<ObjectId>)>,
}

impl App {
    /// The title of the parent the taskbar sees for the window `title`:
    /// `None` without a window of that title, `Some("")` without a parent.
    fn parent_of(&self, title: &str) -> Option<String> {
        let (_, parent) = self.toplevels.values().find(|(t, _)| t == title)?;
        Some(match parent {
            Some(id) => self
                .toplevels
                .get(id)
                .map_or_else(|| "?".into(), |(t, _)| t.clone()),
            None => String::new(),
        })
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
        xdg: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            app.configures.insert(xdg.id(), serial);
        }
    }
}

impl Dispatch<ZxdgExportedV2, ()> for App {
    fn event(
        app: &mut Self,
        exported: &ZxdgExportedV2,
        event: zxdg_exported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_exported_v2::Event::Handle { handle } = event {
            app.handles.insert(exported.id(), handle);
        }
    }
}

impl Dispatch<ZxdgImportedV2, ()> for App {
    fn event(
        app: &mut Self,
        imported: &ZxdgImportedV2,
        event: zxdg_imported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_imported_v2::Event::Destroyed = event {
            app.destroyed.push(imported.id());
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        _: <ZwlrForeignToplevelManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
        let entry = app.toplevels.entry(handle.id()).or_default();
        match event {
            Event::Title { title } => entry.0 = title,
            Event::Parent { parent } => entry.1 = parent.map(|p| p.id()),
            Event::Closed => {
                app.toplevels.remove(&handle.id());
            }
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
delegate_noop!(App: ZxdgExporterV2);
delegate_noop!(App: ZxdgImporterV2);

struct Client {
    conn: Connection,
    queue: EventQueue<App>,
    app: App,
    globals: GlobalList,
}

/// A mapped toplevel.
struct Window {
    surface: WlSurface,
    toplevel: XdgToplevel,
    xdg: XdgSurface,
}

impl Client {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().map_err(|e| format!("connect: {e}"))?;
        let (globals, queue) =
            registry_queue_init::<App>(&conn).map_err(|e| format!("registry: {e}"))?;
        Ok(Self {
            conn,
            queue,
            app: App::default(),
            globals,
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

    fn roundtrip(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.app)
            .map(|_| ())
            .map_err(|e| format!("roundtrip: {e}"))
    }

    /// Roundtrip, returning the protocol error that ended the connection.
    fn roundtrip_error(&mut self) -> Option<String> {
        let _ = self.queue.roundtrip(&mut self.app);
        self.conn
            .protocol_error()
            .map(|e| format!("{}: {}", e.object_interface, e.message))
    }

    /// Map a 100x100 toplevel titled `title`.
    fn map(&mut self, title: &str) -> Result<Window> {
        let qh = self.queue.handle();
        let compositor: WlCompositor = self.bind(4)?;
        let wm: XdgWmBase = self.bind(1)?;
        let shm: WlShm = self.bind(1)?;
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_title(title.into());
        surface.commit();
        // On River the window manager configures it asynchronously.
        let id = xdg.id();
        self.wait(|app| app.configures.contains_key(&id))?;
        let serial = self
            .app
            .configures
            .get(&xdg.id())
            .copied()
            .ok_or("no configure")?;
        xdg.ack_configure(serial);
        let (w, h) = (100, 100);
        let fd = rustix::fs::memfd_create("wlforeign", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(|e| format!("memfd_create: {e}"))?;
        let mut file = std::fs::File::from(fd);
        file.write_all(&vec![0x80u8; (w * h * 4) as usize])
            .map_err(|e| format!("writing buffer: {e}"))?;
        let pool = shm.create_pool(file.as_fd(), w * h * 4, &qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &qh, ());
        pool.destroy();
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        self.roundtrip()?;
        Ok(Window {
            surface,
            toplevel,
            xdg,
        })
    }

    /// Export `surface` and return the export and its handle.
    fn export(&mut self, surface: &WlSurface) -> Result<(ZxdgExportedV2, Option<String>)> {
        let exporter: ZxdgExporterV2 = self.bind(1)?;
        let exported = exporter.export_toplevel(surface, &self.queue.handle(), ());
        self.roundtrip()?;
        let handle = self.app.handles.get(&exported.id()).cloned();
        Ok((exported, handle))
    }

    fn import(&mut self, handle: &str) -> Result<ZxdgImportedV2> {
        let importer: ZxdgImporterV2 = self.bind(1)?;
        let imported = importer.import_toplevel(handle.into(), &self.queue.handle(), ());
        self.roundtrip()?;
        Ok(imported)
    }

    fn destroyed(&self, imported: &ZxdgImportedV2) -> bool {
        self.app.destroyed.contains(&imported.id())
    }

    /// Dispatch until `done` holds or `TIMEOUT` passes.
    fn wait(&mut self, done: impl Fn(&App) -> bool) -> Result<bool> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.roundtrip()?;
            if done(&self.app) {
                return Ok(true);
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(false);
            };
            self.conn.flush().map_err(|e| format!("flush: {e}"))?;
            if let Some(guard) = self.queue.prepare_read() {
                let fd = guard.connection_fd();
                let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
                let left = left.min(Duration::from_millis(50));
                let ts = rustix::event::Timespec {
                    tv_sec: left.as_secs() as i64,
                    tv_nsec: left.subsec_nanos() as i64,
                };
                if rustix::event::poll(&mut fds, Some(&ts)).is_ok_and(|n| n > 0) {
                    match guard.read() {
                        Ok(_) | Err(WaylandError::Io(_)) => {}
                        Err(err) => return Err(format!("read: {err}")),
                    }
                }
            }
        }
    }
}

struct Checker {
    failures: usize,
    taskbar: Client,
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

    /// Check that the taskbar sees `parent` (empty: none) as `child`'s
    /// parent.
    fn parent(&mut self, child: &str, parent: &str, what: &str) -> Result<()> {
        let want = Some(parent.to_string());
        let ok = self.taskbar.wait(|app| app.parent_of(child) == want)?;
        let seen = self.taskbar.app.parent_of(child);
        let seen = match seen.as_deref() {
            None => "no such window".into(),
            Some("") => "no parent".into(),
            Some(p) => format!("parent {p}"),
        };
        self.check(ok, &format!("{what} ({child}: {seen})"));
        Ok(())
    }
}

fn check() -> Result<bool> {
    let mut taskbar = Client::connect()?;
    let _manager: ZwlrForeignToplevelManagerV1 = taskbar.bind(3)?;
    taskbar.roundtrip()?;
    let mut c = Checker {
        failures: 0,
        taskbar,
    };
    // The app exports, the portal imports.
    let mut app = Client::connect()?;
    let mut portal = Client::connect()?;
    let main = app.map("wlforeign-main")?;
    let dialog = portal.map("wlforeign-dialog")?;
    let other = portal.map("wlforeign-other")?;

    let (export1, handle1) = app.export(&main.surface)?;
    let (_export2, handle2) = app.export(&main.surface)?;
    c.check(
        handle1.as_ref().is_some_and(|h| !h.is_empty()),
        "an export gets a handle",
    );
    c.check(
        handle1.is_some() && handle1 != handle2,
        "exporting a surface again gives another handle",
    );
    let (Some(handle1), Some(handle2)) = (handle1, handle2) else {
        return Ok(false);
    };

    let unknown = portal.import("no-such-handle")?;
    c.check(
        portal.destroyed(&unknown),
        "importing an unknown handle gives destroyed",
    );
    unknown.set_parent_of(&dialog.surface);
    c.check(
        portal.roundtrip_error().is_none(),
        "an inert import ignores set_parent_of",
    );
    c.parent("wlforeign-dialog", "", "an inert import sets no parent")?;

    // Ended by destroying the import.
    let import1 = portal.import(&handle1)?;
    c.check(!portal.destroyed(&import1), "importing a handle works");
    import1.set_parent_of(&dialog.surface);
    import1.set_parent_of(&other.surface);
    portal.roundtrip()?;
    c.parent("wlforeign-dialog", "wlforeign-main", "set_parent_of sets the parent")?;
    c.parent("wlforeign-other", "wlforeign-main", "one import can parent several windows")?;
    import1.destroy();
    portal.roundtrip()?;
    c.parent("wlforeign-dialog", "", "destroying the import clears the parent")?;
    c.parent("wlforeign-other", "", "of every window it was set on")?;

    // Ended by destroying the export; the other handle stays valid.
    let import1 = portal.import(&handle1)?;
    let import1b = portal.import(&handle1)?;
    let import2 = portal.import(&handle2)?;
    import1.set_parent_of(&dialog.surface);
    import1b.set_parent_of(&other.surface);
    portal.roundtrip()?;
    c.parent("wlforeign-other", "wlforeign-main", "a handle can be imported twice")?;
    export1.destroy();
    app.roundtrip()?;
    portal.roundtrip()?;
    c.check(
        portal.destroyed(&import1) && portal.destroyed(&import1b),
        "destroying the export sends destroyed to its imports",
    );
    c.check(
        !portal.destroyed(&import2),
        "imports of another export of the surface stay",
    );
    c.parent("wlforeign-dialog", "", "destroying the export clears the parent")?;
    c.parent("wlforeign-other", "", "of every import's windows")?;
    import1.set_parent_of(&dialog.surface);
    portal.roundtrip()?;
    c.parent("wlforeign-dialog", "", "a revoked import ignores set_parent_of")?;

    // A parent the child replaced on its own isn't cleared.
    import2.set_parent_of(&dialog.surface);
    portal.roundtrip()?;
    c.parent("wlforeign-dialog", "wlforeign-main", "the other handle still works")?;
    dialog.toplevel.set_parent(Some(&other.toplevel));
    portal.roundtrip()?;
    c.parent("wlforeign-dialog", "wlforeign-other", "the child can pick another parent")?;
    import2.destroy();
    portal.roundtrip()?;
    c.parent(
        "wlforeign-dialog",
        "wlforeign-other",
        "destroying the import keeps a parent it didn't set",
    )?;
    dialog.toplevel.set_parent(None);
    portal.roundtrip()?;
    c.parent("wlforeign-dialog", "", "the child can drop its parent")?;

    // Ended by destroying the exported toplevel.
    let extra = app.map("wlforeign-extra")?;
    let (_export3, handle3) = app.export(&extra.surface)?;
    let import3 = portal.import(handle3.as_deref().unwrap_or("-"))?;
    import3.set_parent_of(&dialog.surface);
    portal.roundtrip()?;
    c.parent("wlforeign-dialog", "wlforeign-extra", "a second exported window works")?;
    extra.toplevel.destroy();
    extra.xdg.destroy();
    app.roundtrip()?;
    portal.roundtrip()?;
    c.check(
        portal.destroyed(&import3),
        "destroying the exported toplevel sends destroyed",
    );
    c.parent("wlforeign-dialog", "", "and clears the parent")?;

    // Ended by the exporting client going away.
    let (_export4, handle4) = app.export(&main.surface)?;
    let import4 = portal.import(handle4.as_deref().unwrap_or("-"))?;
    import4.set_parent_of(&dialog.surface);
    portal.roundtrip()?;
    c.parent("wlforeign-dialog", "wlforeign-main", "a new export of the surface works")?;
    drop(app);
    portal.roundtrip()?;
    let gone = c.taskbar.wait(|app| app.parent_of("wlforeign-main").is_none())?;
    portal.roundtrip()?;
    c.check(
        gone && portal.destroyed(&import4),
        "the exporting client exiting sends destroyed",
    );
    c.parent("wlforeign-dialog", "", "and clears the parent")?;

    // Protocol errors, each on a client of its own.
    let mut bad = Client::connect()?;
    let compositor: WlCompositor = bad.bind(4)?;
    let plain = compositor.create_surface(&bad.queue.handle(), ());
    let exporter: ZxdgExporterV2 = bad.bind(1)?;
    exporter.export_toplevel(&plain, &bad.queue.handle(), ());
    let err = bad.roundtrip_error();
    c.check(
        err.as_deref().is_some_and(|e| e.starts_with("zxdg_exporter_v2")),
        &format!(
            "exporting a surface without a role is an error ({})",
            err.as_deref().unwrap_or("none")
        ),
    );

    let mut owner = Client::connect()?;
    let owned = owner.map("wlforeign-owner")?;
    let (_export5, handle5) = owner.export(&owned.surface)?;
    let handle5 = handle5.unwrap_or_default();

    let mut bad = Client::connect()?;
    let compositor: WlCompositor = bad.bind(4)?;
    let plain = compositor.create_surface(&bad.queue.handle(), ());
    let imported = bad.import(&handle5)?;
    imported.set_parent_of(&plain);
    let err = bad.roundtrip_error();
    c.check(
        err.as_deref().is_some_and(|e| e.starts_with("zxdg_imported_v2")),
        &format!(
            "set_parent_of a surface without a role is an error ({})",
            err.as_deref().unwrap_or("none")
        ),
    );

    // The owner makes its own window the child of a window that is its
    // child through xdg-foreign: a loop.
    let mut bad = Client::connect()?;
    let looped = bad.map("wlforeign-loop")?;
    let (_export6, handle6) = bad.export(&looped.surface)?;
    let to_loop = bad.import(&handle5)?;
    to_loop.set_parent_of(&looped.surface);
    bad.roundtrip()?;
    // The protocol doesn't say; like wlroots, ignore it rather than
    // disconnect the client.
    let back = owner.import(handle6.as_deref().unwrap_or("-"))?;
    back.set_parent_of(&owned.surface);
    let err = owner.roundtrip_error();
    c.check(
        err.is_none(),
        &format!(
            "a parent loop is no error ({})",
            err.as_deref().unwrap_or("none")
        ),
    );
    c.parent("wlforeign-owner", "", "and is ignored")?;
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
            eprintln!("wlforeign: {err}");
            ExitCode::from(2)
        }
    }
}
