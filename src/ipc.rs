//! Local IPC between the window manager and `canoe --share-picker`.
//!
//! xdg-desktop-portal-wlr asks a "chooser" program what to share. Canoe is
//! that program (`canoe --share-picker`), but the question has to be answered
//! by the running window manager, which knows the windows and can show the
//! picker. The two talk over a Unix socket in `$XDG_RUNTIME_DIR`, named after
//! the Wayland display.
//!
//! The protocol is line based. The client sends [`HEADER`], then one line per
//! thing the portal offered, then shuts down its write side. The window
//! manager answers with the chosen line, or nothing if the user declined, and
//! closes the connection.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use crate::canoe::share_picker::ShareRequestId;

/// First line of every request.
const HEADER: &str = "canoe-share-picker 1";
/// Requests larger than this are dropped.
const MAX_REQUEST: usize = 1 << 20;

/// The socket the window manager on `$WAYLAND_DISPLAY` listens on.
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

struct Connection {
    id: ShareRequestId,
    stream: UnixStream,
    buf: Vec<u8>,
    /// The request was read in full and handed out; waiting for the answer.
    waiting: bool,
}

/// What the window manager polls for.
pub struct ShareServer {
    listener: UnixListener,
    path: PathBuf,
    connections: Vec<Connection>,
    next_id: ShareRequestId,
}

impl ShareServer {
    pub fn bind() -> io::Result<Self> {
        let path = socket_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
        Self::bind_at(path)
    }

    fn bind_at(path: PathBuf) -> io::Result<Self> {
        // This runs before River tells us whether it accepts us as the
        // window manager. If another canoe still answers on the socket, it is
        // the one River uses, and taking the socket over (or removing it on
        // exit) would break its screen sharing.
        if UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another window manager is answering on this display",
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
    pub fn dispatch(
        &mut self,
        hung_up: &[ShareRequestId],
    ) -> (Vec<ShareRequest>, Vec<ShareRequestId>) {
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
                        match parse_request(&conn.buf) {
                            Some(lines) => {
                                conn.waiting = true;
                                conn.buf = Vec::new();
                                requests.push(ShareRequest { id: conn.id, lines });
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

    /// Answer a request with the chosen line, or `None` if the user declined,
    /// and close its connection.
    pub fn reply(&mut self, id: ShareRequestId, choice: Option<&str>) {
        let Some(pos) = self.connections.iter().position(|c| c.id == id) else {
            return;
        };
        let conn = self.connections.remove(pos);
        if let Some(choice) = choice {
            // A single short line fits the socket buffer, so a non-blocking
            // write completes; if it doesn't, the client sees a decline.
            let _ = (&conn.stream).write_all(format!("{choice}\n").as_bytes());
        }
    }
}

impl Drop for ShareServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn parse_request(buf: &[u8]) -> Option<Vec<String>> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.lines();
    if lines.next()? != HEADER {
        return None;
    }
    Some(
        lines
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// `canoe --share-picker`: a chooser for xdg-desktop-portal-wlr.
///
/// Reads the portal's offer from stdin (empty for a "simple" chooser), lets
/// the running window manager ask the user, and prints the choice. Printing
/// nothing tells the portal the user declined.
pub fn run_share_picker() -> i32 {
    use std::io::IsTerminal;

    let mut offer = String::new();
    if !io::stdin().is_terminal() {
        if let Err(e) = io::stdin().read_to_string(&mut offer) {
            eprintln!("canoe: reading the offer from stdin failed: {e}");
            return 1;
        }
    }

    let Some(path) = socket_path() else {
        eprintln!("canoe: XDG_RUNTIME_DIR is not set; cannot reach the window manager");
        return 1;
    };
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!(
                "canoe: cannot reach the window manager at {}: {e}",
                path.display()
            );
            return 1;
        }
    };

    let mut request = String::with_capacity(HEADER.len() + 1 + offer.len());
    request.push_str(HEADER);
    request.push('\n');
    request.push_str(&offer);
    if stream.write_all(request.as_bytes()).is_err()
        || stream.shutdown(std::net::Shutdown::Write).is_err()
    {
        eprintln!("canoe: sending the request to the window manager failed");
        return 1;
    }

    let mut answer = String::new();
    if let Err(e) = stream.read_to_string(&mut answer) {
        eprintln!("canoe: reading the answer from the window manager failed: {e}");
        return 1;
    }
    if let Some(choice) = answer.lines().next().filter(|l| !l.is_empty()) {
        println!("{choice}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests() {
        let lines = parse_request(b"canoe-share-picker 1\nMonitor: DP-1 Dell\n\nWindow: a (b)\n");
        assert_eq!(
            lines,
            Some(vec![
                "Monitor: DP-1 Dell".to_string(),
                "Window: a (b)".to_string()
            ])
        );
        assert_eq!(parse_request(b"canoe-share-picker 1\n"), Some(vec![]));
        assert_eq!(parse_request(b"something else\nMonitor: DP-1\n"), None);
        assert_eq!(parse_request(b""), None);
    }

    #[test]
    fn leaves_the_socket_of_a_running_server_alone() {
        let path = std::env::temp_dir().join(format!("canoe-ipc-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let running = ShareServer::bind_at(path.clone()).unwrap();
        let second = ShareServer::bind_at(path.clone());
        assert_eq!(
            second.err().map(|e| e.kind()),
            Some(io::ErrorKind::AddrInUse)
        );
        assert!(UnixStream::connect(&path).is_ok());

        // A socket nobody listens on any more is replaced.
        drop(running);
        drop(UnixListener::bind(&path).unwrap());
        let replacement = ShareServer::bind_at(path.clone()).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        drop(replacement);
        assert!(!path.exists());
    }
}
