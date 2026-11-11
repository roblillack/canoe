//! wlprimary: show or choose the primary output with canoe's
//! xx-primary-output-v1.

use std::process::ExitCode;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

#[allow(dead_code, non_camel_case_types, non_upper_case_globals, clippy::all)]
mod protocol {
    use wayland_client;

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        wayland_scanner::generate_interfaces!(
            "../../src/compositor/protocols/xx-primary-output-v1.xml"
        );
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!(
        "../../src/compositor/protocols/xx-primary-output-v1.xml"
    );
}

use protocol::xx_primary_output_manager_v1::{self, XxPrimaryOutputManagerV1};

const USAGE: &str = "\
usage: wlprimary
       wlprimary set NAME
       wlprimary unset
       wlprimary watch

Connects to $WAYLAND_DISPLAY.
  (none)   print the primary output's name, or 'none'
  set      make output NAME primary, print the primary output afterwards;
           exits 1 if NAME didn't become primary
  unset    have no primary output, print 'none' afterwards
  watch    print the primary output, and again whenever it changes";

#[derive(Default)]
struct App {
    /// Every `primary` event, in order.
    events: Vec<Option<String>>,
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

impl Dispatch<XxPrimaryOutputManagerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &XxPrimaryOutputManagerV1,
        event: xx_primary_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let xx_primary_output_manager_v1::Event::Primary { name } = event;
        app.events.push(name);
    }
}

fn show(name: &Option<String>) -> &str {
    name.as_deref().unwrap_or("none")
}

fn run(args: &[String]) -> Result<bool, String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("cannot connect: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<App>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let manager: XxPrimaryOutputManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| "the compositor has no xx_primary_output_manager_v1".to_string())?;
    let mut app = App::default();
    let roundtrip = |queue: &mut wayland_client::EventQueue<App>, app: &mut App| {
        queue
            .roundtrip(app)
            .map(|_| ())
            .map_err(|e| format!("roundtrip: {e}"))
    };
    roundtrip(&mut queue, &mut app)?;
    let current = app.events.last().cloned().flatten();

    match args {
        [] => {
            println!("{}", show(&current));
            Ok(true)
        }
        [cmd, name] if cmd == "set" => {
            app.events.clear();
            manager.set_primary(name.clone());
            roundtrip(&mut queue, &mut app)?;
            let after = app.events.last().cloned().flatten();
            println!("{}", show(&after));
            Ok(after.as_deref() == Some(name.as_str()))
        }
        [cmd] if cmd == "unset" => {
            app.events.clear();
            manager.unset_primary();
            roundtrip(&mut queue, &mut app)?;
            let after = app.events.last().cloned().flatten();
            println!("{}", show(&after));
            Ok(after.is_none())
        }
        [cmd] if cmd == "watch" => {
            println!("{}", show(&current));
            app.events.clear();
            loop {
                queue
                    .blocking_dispatch(&mut app)
                    .map_err(|e| format!("dispatch: {e}"))?;
                for name in app.events.drain(..) {
                    println!("{}", show(&name));
                }
            }
        }
        _ => Err(USAGE.to_string()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("wlprimary: {err}");
            ExitCode::from(2)
        }
    }
}
