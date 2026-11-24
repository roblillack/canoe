//! Local IPC between the running canoe and its commands (`canoe reload`,
//! ...) or `canoe --share-picker`.
//!
//! The two talk over a Unix socket in `$XDG_RUNTIME_DIR`, named after the
//! Wayland display, so a command reaches the canoe that `WAYLAND_DISPLAY`
//! names. The protocol is line based. The client sends a header naming what
//! it wants, then the request's lines, then shuts down its write side. Canoe
//! answers and closes the connection.
//!
//! - [`SHARE_HEADER`]: xdg-desktop-portal-wlr asks a "chooser" program what
//!   to share. Canoe is that program (`canoe --share-picker`), but the
//!   question has to be answered by the running canoe, which knows the
//!   windows and can show the picker. The lines are what the portal offered,
//!   one each; the answer is the chosen line, or nothing if the user
//!   declined.
//! - [`COMMAND_HEADER`]: a command like `canoe reload`, one argument per
//!   line. The
//!   answer's first line is `ok` or `error`; the rest is the output or what
//!   went wrong.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use crate::canoe::share_picker::ShareRequestId;

/// First line of a screen-sharing request.
const SHARE_HEADER: &str = "canoe-share-picker 1";
/// First line of a command like `canoe reload`.
const COMMAND_HEADER: &str = "canoe-command 1";
/// Requests larger than this are dropped.
const MAX_REQUEST: usize = 1 << 20;

/// The socket the canoe on `$WAYLAND_DISPLAY` listens on.
fn socket_path() -> Option<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let display = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
    // WAYLAND_DISPLAY may be an absolute path; only its last part names it.
    let display = PathBuf::from(display);
    let display = display.file_name()?.to_string_lossy().into_owned();
    Some(PathBuf::from(runtime_dir).join(format!("canoe-{display}.sock")))
}

/// A request from `canoe --share-picker`: the portal's offer, one line each.
#[derive(Debug)]
pub struct ShareRequest {
    pub id: ShareRequestId,
    pub lines: Vec<String>,
}

/// A command like `canoe reload`, answered with [`Server::reply_command`].
#[derive(Debug)]
pub struct CommandRequest {
    pub id: ShareRequestId,
    pub args: Vec<String>,
}

/// What a client asked for.
#[derive(Debug)]
pub enum Request {
    Share(ShareRequest),
    Command(CommandRequest),
}

struct Connection {
    id: ShareRequestId,
    stream: UnixStream,
    buf: Vec<u8>,
    /// The request was read in full and handed out; waiting for the answer.
    waiting: bool,
}

/// What the compositor polls for.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    connections: Vec<Connection>,
    next_id: ShareRequestId,
}

impl Server {
    pub fn bind() -> io::Result<Self> {
        let path = socket_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
        Self::bind_at(path)
    }

    fn bind_at(path: PathBuf) -> io::Result<Self> {
        // If another canoe still answers on the socket, taking it over (or
        // removing it on exit) would cut that canoe off from its screen
        // sharing and commands.
        if UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another canoe is answering on this display",
            ));
        }
        // Nobody answers, so a socket file left there is stale and would
        // make bind fail.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        Ok(Self {
            listener,
            path,
            connections: Vec::new(),
            next_id: 1,
        })
    }

    /// A duplicate of the listening socket, to wait for connections on.
    pub fn listener_fd(&self) -> io::Result<std::os::fd::OwnedFd> {
        self.listener.as_fd().try_clone_to_owned()
    }

    /// Whether requests are being received or answered.
    pub fn has_connections(&self) -> bool {
        !self.connections.is_empty()
    }

    /// File descriptors to poll, and whether to poll each for input: the
    /// listener and connections still sending their request are; connections
    /// waiting for an answer are polled only for hangups. Connections come
    /// with their request id, the listener with `None`.
    pub fn poll_fds(&self) -> Vec<(BorrowedFd<'_>, bool, Option<ShareRequestId>)> {
        let mut fds = vec![(self.listener.as_fd(), true, None)];
        for conn in &self.connections {
            fds.push((conn.stream.as_fd(), !conn.waiting, Some(conn.id)));
        }
        fds
    }

    /// Accept new connections and read what's there. `hung_up` names the
    /// connections polling found closed. Returns requests that arrived in
    /// full, and the ids of waiting requests whose client went away.
    pub fn dispatch(&mut self, hung_up: &[ShareRequestId]) -> (Vec<Request>, Vec<ShareRequestId>) {
        let gone: Vec<ShareRequestId> = self
            .connections
            .iter()
            .filter(|c| c.waiting && hung_up.contains(&c.id))
            .map(|c| c.id)
            .collect();
        self.connections.retain(|c| !gone.contains(&c.id));

        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let id = self.next_id;
                    self.next_id += 1;
                    self.connections.push(Connection {
                        id,
                        stream,
                        buf: Vec::new(),
                        waiting: false,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }

        let mut requests = Vec::new();
        let mut broken = Vec::new();
        for conn in self.connections.iter_mut().filter(|c| !c.waiting) {
            let mut chunk = [0u8; 4096];
            loop {
                match conn.stream.read(&mut chunk) {
                    Ok(0) => {
                        match parse_request(conn.id, &conn.buf) {
                            Some(request) => {
                                conn.waiting = true;
                                conn.buf = Vec::new();
                                requests.push(request);
                            }
                            None => broken.push(conn.id),
                        }
                        break;
                    }
                    Ok(n) => {
                        conn.buf.extend_from_slice(&chunk[..n]);
                        if conn.buf.len() > MAX_REQUEST {
                            broken.push(conn.id);
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        broken.push(conn.id);
                        break;
                    }
                }
            }
        }
        self.connections.retain(|c| !broken.contains(&c.id));

        (requests, gone)
    }

    /// The connection of request `id`, taken out of the ones polled.
    fn take(&mut self, id: ShareRequestId) -> Option<Connection> {
        let pos = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(pos))
    }

    /// Answer a command with what to print, or with what went wrong, and
    /// close its connection.
    pub fn reply_command(&mut self, id: ShareRequestId, result: Result<String, String>) {
        let Some(conn) = self.take(id) else {
            return;
        };
        let (status, text) = match result {
            Ok(text) => ("ok", text),
            Err(text) => ("error", text),
        };
        let mut answer = format!("{status}\n{text}");
        if !answer.ends_with('\n') {
            answer.push('\n');
        }
        // An answer larger than the socket's buffer goes out as the client
        // reads it; give it a moment, but don't let it hold the session up.
        let _ = conn.stream.set_nonblocking(false);
        let _ = conn
            .stream
            .set_write_timeout(Some(Duration::from_millis(200)));
        let _ = (&conn.stream).write_all(answer.as_bytes());
    }

    /// Answer a screen-sharing request with the chosen line, or `None` if the
    /// user declined, and close its connection.
    pub fn reply(&mut self, id: ShareRequestId, choice: Option<&str>) {
        let Some(conn) = self.take(id) else {
            return;
        };
        if let Some(choice) = choice {
            // A single short line fits the socket buffer, so a non-blocking
            // write completes; if it doesn't, the client sees a decline.
            let _ = (&conn.stream).write_all(format!("{choice}\n").as_bytes());
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn parse_request(id: ShareRequestId, buf: &[u8]) -> Option<Request> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.lines();
    let header = lines.next()?;
    let lines: Vec<String> = lines
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    match header {
        SHARE_HEADER => Some(Request::Share(ShareRequest { id, lines })),
        COMMAND_HEADER => Some(Request::Command(CommandRequest { id, args: lines })),
        _ => None,
    }
}

/// Send `header` and `body` to the running canoe and return its answer.
fn exchange(header: &str, body: &str) -> Result<String, String> {
    let path = socket_path().ok_or("XDG_RUNTIME_DIR is not set; cannot reach the running canoe")?;
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
        format!(
            "cannot reach a canoe on WAYLAND_DISPLAY={display} at {}: {e}",
            path.display()
        )
    })?;

    let mut request = String::with_capacity(header.len() + 1 + body.len());
    request.push_str(header);
    request.push('\n');
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.shutdown(std::net::Shutdown::Write))
        .map_err(|e| format!("sending the request to canoe failed: {e}"))?;

    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .map_err(|e| format!("reading canoe's answer failed: {e}"))?;
    Ok(answer)
}

/// `canoe --share-picker`: a chooser for xdg-desktop-portal-wlr.
///
/// Reads the portal's offer from stdin (empty for a "simple" chooser), lets
/// the running canoe ask the user, and prints the choice. Printing nothing
/// tells the portal the user declined.
pub fn run_share_picker() -> i32 {
    use std::io::IsTerminal;

    let mut offer = String::new();
    if !io::stdin().is_terminal() {
        if let Err(e) = io::stdin().read_to_string(&mut offer) {
            eprintln!("canoe: reading the offer from stdin failed: {e}");
            return 1;
        }
    }

    let answer = match exchange(SHARE_HEADER, &offer) {
        Ok(answer) => answer,
        Err(e) => {
            eprintln!("canoe: {e}");
            return 1;
        }
    };
    if let Some(choice) = answer.lines().next().filter(|l| !l.is_empty()) {
        println!("{choice}");
    }
    0
}

/// `canoe COMMAND ARGS...` (`args`, never empty): send a command to the
/// running canoe and print its answer. Returns the exit status.
pub fn run_command(args: Vec<String>) -> i32 {
    if args.iter().any(|a| a.contains('\n')) {
        eprintln!("canoe: arguments of commands can't contain line breaks");
        return 2;
    }

    let answer = match exchange(COMMAND_HEADER, &args.join("\n")) {
        Ok(answer) => answer,
        Err(e) => {
            eprintln!("canoe: {e}");
            return 1;
        }
    };
    let (status, text) = answer.split_once('\n').unwrap_or((&answer, ""));
    match status {
        "ok" if args == ["version"] => {
            // Not necessarily this binary's version, after an upgrade.
            let running = text.trim_end();
            let this = format!("canoe {}", env!("CARGO_PKG_VERSION"));
            if running == this {
                println!("{running}");
            } else {
                println!("{running} (this binary: {})", env!("CARGO_PKG_VERSION"));
            }
            0
        }
        "ok" => {
            print!("{text}");
            0
        }
        "error" => {
            eprint!("canoe: {text}");
            1
        }
        // A canoe from before these commands drops the request unanswered.
        _ => {
            eprintln!(
                "canoe: the running canoe doesn't know `canoe {}`; is it older than this one?",
                args[0]
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share_lines(buf: &[u8]) -> Option<Vec<String>> {
        match parse_request(1, buf)? {
            Request::Share(request) => Some(request.lines),
            Request::Command(_) => None,
        }
    }

    fn command_args(buf: &[u8]) -> Option<Vec<String>> {
        match parse_request(1, buf)? {
            Request::Command(request) => Some(request.args),
            Request::Share(_) => None,
        }
    }

    #[test]
    fn parses_requests() {
        let lines = share_lines(b"canoe-share-picker 1\nMonitor: DP-1 Dell\n\nWindow: a (b)\n");
        assert_eq!(
            lines,
            Some(vec![
                "Monitor: DP-1 Dell".to_string(),
                "Window: a (b)".to_string()
            ])
        );
        assert_eq!(share_lines(b"canoe-share-picker 1\n"), Some(vec![]));
        assert!(parse_request(1, b"something else\nMonitor: DP-1\n").is_none());
        assert!(parse_request(1, b"").is_none());

        assert_eq!(
            command_args(b"canoe-command 1\noutputs\nprimary\neDP-1"),
            Some(vec![
                "outputs".to_string(),
                "primary".to_string(),
                "eDP-1".to_string()
            ])
        );
        assert!(parse_request(1, b"canoe-command 2\nversion").is_none());
    }

    #[test]
    fn leaves_the_socket_of_a_running_server_alone() {
        let path = std::env::temp_dir().join(format!("canoe-ipc-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let running = Server::bind_at(path.clone()).unwrap();
        let second = Server::bind_at(path.clone());
        assert_eq!(
            second.err().map(|e| e.kind()),
            Some(io::ErrorKind::AddrInUse)
        );
        assert!(UnixStream::connect(&path).is_ok());

        // A socket nobody listens on any more is replaced.
        drop(running);
        drop(UnixListener::bind(&path).unwrap());
        let replacement = Server::bind_at(path.clone()).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        drop(replacement);
        assert!(!path.exists());
    }
}
