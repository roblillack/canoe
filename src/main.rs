//! Canoe - a stacking Wayland compositor and window manager.

mod binding;
mod canoe;
mod compositor;
mod config;
mod ipc;
mod rule;
mod wm;

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use nix::fcntl::OFlag;
use nix::libc;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::pipe2;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;

use canoe::Context;
use compositor::State;

static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn forward_signal(sig: libc::c_int) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = sig as u8;
    unsafe {
        libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
    }
}

/// Export `WAYLAND_DISPLAY`, `DISPLAY` and `XDG_CURRENT_DESKTOP` to the systemd user
/// manager and the D-Bus activation environment, so services started on demand
/// (xdg-desktop-portal and its backends) connect to this session. The XCursor
/// theme is exported as well, if set, so their cursors match.
fn update_activation_environment(context: &Context) {
    let argv = activation_environment_argv(
        &context.config.env,
        &context.config.xcursor_theme_resolved(),
        |name| std::env::var(name).ok(),
    );
    if let Some(argv) = argv {
        context.spawn(&argv);
    }
}

/// Build the `dbus-update-activation-environment` command line.
///
/// Each variable gets the value programs launched by canoe see: the `[env]`
/// table (`config_env`) overrides canoe's own environment (`lookup`). Values
/// are always passed explicitly as `VAR=VAL`, so the result does not depend on
/// which variables canoe itself inherited. `XCURSOR_THEME`/`XCURSOR_SIZE` are
/// taken from the resolved `xcursor` theme and only exported where it sets
/// them. Returns `None` without a display to export.
fn activation_environment_argv(
    config_env: &HashMap<String, String>,
    xcursor: &config::XCursorTheme,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<Vec<String>> {
    let effective = |name: &str| {
        config_env
            .get(name)
            .cloned()
            .or_else(|| lookup(name))
            .filter(|value| !value.is_empty())
    };
    let display = effective("WAYLAND_DISPLAY")?;
    // xdg-desktop-portal picks its backends by desktop name.
    let desktop = effective("XDG_CURRENT_DESKTOP").unwrap_or_else(|| "canoe".to_string());
    let mut argv = vec![
        "dbus-update-activation-environment".to_string(),
        "--systemd".to_string(),
        format!("WAYLAND_DISPLAY={display}"),
        format!("XDG_CURRENT_DESKTOP={desktop}"),
    ];
    // Xwayland's display, for services that start X11 programs.
    if let Some(x11_display) = effective("DISPLAY") {
        argv.push(format!("DISPLAY={x11_display}"));
    }
    if let Some(ref name) = xcursor.name {
        argv.push(format!("XCURSOR_THEME={name}"));
    }
    if let Some(size) = xcursor.size {
        argv.push(format!("XCURSOR_SIZE={size}"));
    }
    Some(argv)
}

const USAGE: &str = "\
canoe - a stacking Wayland compositor

Usage: canoe [OPTIONS]

Options:
  -n, --no-config   Ignore ~/.config/canoe/canoe.toml and use built-in defaults
      --backend <udev|winit|headless>
                    Where to show the session: on the monitors of this seat
                    (udev, the default on a text console), in a window of the
                    running session (winit, the default inside one), or
                    nowhere, for tests (headless; outputs from
                    CANOE_HEADLESS_OUTPUTS, e.g. 1920x1080@1.5,1280x720)
      --no-xwayland Don't start Xwayland, so X11 programs don't run
      --share-picker
                    Act as the screen-sharing chooser of xdg-desktop-portal-wlr:
                    let the running canoe ask what to share, print the answer
  -h, --help        Print this help and exit
  -V, --version     Print version and exit

Send SIGHUP (e.g. `pkill -HUP canoe`) to re-read the configuration file.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Udev,
    Winit,
    Headless,
}

/// Options parsed from the command line.
struct CliArgs {
    skip_config: bool,
    backend: BackendKind,
    xwayland: bool,
}

/// Parse command-line arguments, exiting the process for --help/--version or on
/// an unknown flag. Kept deliberately tiny so we avoid a full arg-parsing dep.
fn parse_args() -> CliArgs {
    let nested =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    let mut args = CliArgs {
        skip_config: false,
        xwayland: true,
        backend: if nested {
            BackendKind::Winit
        } else {
            BackendKind::Udev
        },
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_string(), Some(value.to_string()))
            }
            _ => (arg.clone(), None),
        };
        match flag.as_str() {
            "-n" | "--no-config" => args.skip_config = true,
            "--no-xwayland" => args.xwayland = false,
            "--share-picker" => std::process::exit(ipc::run_share_picker()),
            "--backend" => {
                let value = inline.or_else(|| iter.next()).unwrap_or_default();
                args.backend = match value.as_str() {
                    "udev" | "drm" => BackendKind::Udev,
                    "winit" | "nested" => BackendKind::Winit,
                    "headless" => BackendKind::Headless,
                    other => {
                        eprintln!("canoe: unknown backend '{other}'\n\n{USAGE}");
                        std::process::exit(2);
                    }
                };
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("canoe {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => {
                eprintln!("canoe: unrecognized argument '{other}'\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    args
}

/// Forward SIGINT/SIGTERM/SIGQUIT/SIGHUP/SIGCHLD into the event loop.
fn install_signal_handlers() -> Result<OwnedFd, Box<dyn std::error::Error>> {
    let (signal_read_fd, signal_write_fd) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?;
    SIGNAL_WRITE_FD.store(signal_write_fd.as_raw_fd(), Ordering::Relaxed);
    // The write end must outlive the signal handler that references its fd.
    std::mem::forget(signal_write_fd);
    let action = SigAction::new(
        SigHandler::Handler(forward_signal),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for sig in [
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGQUIT,
        Signal::SIGCHLD,
        Signal::SIGHUP,
    ] {
        unsafe { sigaction(sig, &action)? };
    }
    Ok(signal_read_fd)
}

fn handle_signals(state: &mut State, fd: &OwnedFd) {
    let mut buf = [0u8; 32];
    loop {
        match nix::unistd::read(fd.as_raw_fd(), &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &byte in &buf[..n] {
                    match Signal::try_from(byte as i32) {
                        Ok(Signal::SIGINT) | Ok(Signal::SIGTERM) | Ok(Signal::SIGQUIT) => {
                            state.loop_signal.stop();
                        }
                        Ok(Signal::SIGHUP) => {
                            state.context.borrow_mut().reload_config();
                            state.schedule_wm();
                        }
                        Ok(Signal::SIGCHLD) => loop {
                            match nix::sys::wait::waitpid(
                                None,
                                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                            ) {
                                Ok(nix::sys::wait::WaitStatus::StillAlive) => break,
                                Ok(_) => continue,
                                Err(_) => break,
                            }
                        },
                        _ => {}
                    }
                }
                if n < buf.len() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();

    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let mut state = State::new(
        display,
        event_loop.handle(),
        event_loop.get_signal(),
        args.skip_config,
    )?;

    // Programs canoe launches connect to it. Set before the backend starts,
    // since a nested session must still reach its host until then.
    let socket_name = state.socket_name.clone();

    match args.backend {
        BackendKind::Headless => compositor::backend::headless::init(&mut state)?,
        #[cfg(feature = "winit")]
        BackendKind::Winit => compositor::backend::winit::init(&mut state)?,
        #[cfg(not(feature = "winit"))]
        BackendKind::Winit => return Err("canoe was built without the winit feature".into()),
        BackendKind::Udev => compositor::backend::udev::init(&mut state)?,
    }
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);
    std::env::remove_var("DISPLAY");
    // X11 programs get Xwayland (which sets DISPLAY).
    if args.xwayland {
        state.start_xwayland();
    }
    eprintln!(
        "canoe: listening on WAYLAND_DISPLAY={}",
        socket_name.to_string_lossy()
    );

    let signal_fd = install_signal_handlers()?;
    let signal_source = Generic::new(signal_fd, Interest::READ, Mode::Level);
    event_loop
        .handle()
        .insert_source(signal_source, |_, fd, state| {
            handle_signals(state, fd);
            Ok(PostAction::Continue)
        })?;

    // Answer screen-sharing requests from `canoe --share-picker`.
    match ipc::ShareServer::bind() {
        Ok(server) => state.share_server = Some(server),
        Err(e) => eprintln!("canoe: screen-sharing picker unavailable: {e}"),
    }
    // Connections wake the loop up; while requests are in flight (they are
    // short-lived) a timer looks after them.
    if let Some(fd) = state
        .share_server
        .as_ref()
        .and_then(|s| s.listener_fd().ok())
    {
        event_loop.handle().insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            |_, _, state| {
                compositor::wm_glue::poll_share_server(state);
                if state.share_timer.is_none() {
                    let timer = Timer::from_duration(Duration::from_millis(50));
                    state.share_timer = state
                        .loop_handle
                        .insert_source(timer, |_, _, state| {
                            compositor::wm_glue::poll_share_server(state);
                            let busy = state
                                .share_server
                                .as_ref()
                                .is_some_and(|s| s.has_connections())
                                || state.context.borrow().next_share_nudge().is_some();
                            if busy {
                                TimeoutAction::ToDuration(Duration::from_millis(50))
                            } else {
                                state.share_timer = None;
                                TimeoutAction::Drop
                            }
                        })
                        .ok();
                }
                Ok(PostAction::Continue)
            },
        )?;
    }

    // Let D-Bus activated services (the screencast portal, above all) find
    // this session's display.
    if state.context.borrow().config.update_activation_environment {
        update_activation_environment(&state.context.borrow());
    }

    state.run_wm();
    event_loop.run(None, &mut state, |state| {
        if !state.requests.is_empty() {
            state.schedule_wm();
        }
        state.refresh();
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_from(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    fn config_env(vars: &[(&str, &str)]) -> HashMap<String, String> {
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn no_xcursor() -> config::XCursorTheme {
        config::XCursorTheme::default()
    }

    #[test]
    fn test_activation_environment_uses_own_environment() {
        let argv = activation_environment_argv(
            &HashMap::new(),
            &no_xcursor(),
            lookup_from(&[
                ("WAYLAND_DISPLAY", "wayland-1"),
                ("XDG_CURRENT_DESKTOP", "foo"),
            ]),
        )
        .expect("argv");
        assert_eq!(
            argv[2..],
            ["WAYLAND_DISPLAY=wayland-1", "XDG_CURRENT_DESKTOP=foo"]
        );
    }

    #[test]
    fn test_activation_environment_defaults_desktop_to_canoe() {
        let argv = activation_environment_argv(
            &HashMap::new(),
            &no_xcursor(),
            lookup_from(&[("WAYLAND_DISPLAY", "w")]),
        )
        .expect("argv");
        assert_eq!(argv[3], "XDG_CURRENT_DESKTOP=canoe");
    }

    #[test]
    fn test_activation_environment_prefers_config_env() {
        // [env] wins whether or not canoe inherited the variable itself.
        let env = config_env(&[("XDG_CURRENT_DESKTOP", "river:wlroots")]);
        for own in [
            lookup_from(&[("WAYLAND_DISPLAY", "w"), ("XDG_CURRENT_DESKTOP", "river")]),
            lookup_from(&[("WAYLAND_DISPLAY", "w")]),
        ] {
            let argv = activation_environment_argv(&env, &no_xcursor(), own).expect("argv");
            assert_eq!(argv[3], "XDG_CURRENT_DESKTOP=river:wlroots");
        }
    }

    #[test]
    fn test_activation_environment_exports_xcursor_theme() {
        let own = lookup_from(&[("WAYLAND_DISPLAY", "w")]);
        let argv = activation_environment_argv(&HashMap::new(), &no_xcursor(), &own).expect("argv");
        assert_eq!(argv.len(), 4, "nothing to export without a theme");

        let xcursor = config::XCursorTheme {
            name: Some("Adwaita".to_string()),
            size: Some(32),
        };
        let argv = activation_environment_argv(&HashMap::new(), &xcursor, &own).expect("argv");
        assert_eq!(argv[4..], ["XCURSOR_THEME=Adwaita", "XCURSOR_SIZE=32"]);

        let xcursor = config::XCursorTheme {
            name: None,
            size: Some(32),
        };
        let argv = activation_environment_argv(&HashMap::new(), &xcursor, &own).expect("argv");
        assert_eq!(argv[4..], ["XCURSOR_SIZE=32"]);
    }

    #[test]
    fn test_activation_environment_exports_xwayland_display() {
        let argv = activation_environment_argv(
            &HashMap::new(),
            &no_xcursor(),
            lookup_from(&[("WAYLAND_DISPLAY", "w"), ("DISPLAY", ":1")]),
        )
        .expect("argv");
        assert_eq!(argv[4..], ["DISPLAY=:1"]);
    }

    #[test]
    fn test_activation_environment_needs_a_display() {
        assert!(
            activation_environment_argv(&HashMap::new(), &no_xcursor(), lookup_from(&[])).is_none()
        );
        let env = config_env(&[("WAYLAND_DISPLAY", "")]);
        assert!(activation_environment_argv(
            &env,
            &no_xcursor(),
            lookup_from(&[("WAYLAND_DISPLAY", "w")])
        )
        .is_none());
    }
}
