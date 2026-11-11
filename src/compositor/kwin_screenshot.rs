//! KWin's screenshot interface on the session bus (`emulate_kwin`).
//!
//! On Wayland, Spectacle takes screenshots only through KWin's D-Bus API,
//! `org.kde.KWin.ScreenShot2` at `/org/kde/KWin/ScreenShot2` on the name
//! `org.kde.KWin`. Canoe offers it, with the screenshots it takes itself
//! (see [`super::capture`]). Like KWin, it also owns the name
//! `org.kde.KWin.ScreenShot2`, which is how Spectacle tells KWin is there.
//!
//! Every method takes a dictionary of options and the write end of a pipe.
//! It replies with a dictionary describing the image (its size, stride,
//! `QImage` format and scale), and the raw pixels are written to the pipe,
//! which the caller reads until it is closed. Spectacle only starts reading
//! once it has the reply, so the pixels are written from a thread of their
//! own.
//!
//! The bus connection lives in zbus's own thread. Requests reach the event
//! loop through a channel and are answered through a oneshot channel each;
//! `CaptureInteractive` is answered once the user picked a window or screen
//! with the share picker.

use std::collections::HashMap;
use std::io::Write;

use futures_channel::oneshot;
use smithay::reexports::calloop::channel::{self, Channel, Event, Sender};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay::utils::{Logical, Rectangle};
use zbus::zvariant::{OwnedFd, OwnedValue, Value};

use super::capture::{Screenshot, WindowCapture};
use super::{LockState, State};
use crate::canoe;
use crate::canoe::share_picker::{ScreenshotRequestId, ShareTarget};

const BUS_NAME: &str = "org.kde.KWin";
/// Spectacle looks for this name before anything else.
const SCREENSHOT_BUS_NAME: &str = "org.kde.KWin.ScreenShot2";
const OBJECT_PATH: &str = "/org/kde/KWin/ScreenShot2";
/// The interface version announced. Spectacle 6 checks it before using the
/// methods of the later versions (CaptureWorkspace among them).
const VERSION: u32 = 4;
/// `QImage::Format_ARGB32_Premultiplied`: 32-bit pixels 0xAARRGGBB in host
/// byte order, which on little-endian machines is [`Screenshot`]'s
/// B, G, R, A.
const QIMAGE_FORMAT_ARGB32_PREMULTIPLIED: u32 = 6;

/// What to take a screenshot of.
#[derive(Debug, Clone)]
pub enum Capture {
    /// A window by its handle (canoe's foreign-toplevel identifier).
    Window(String),
    ActiveWindow,
    /// A rectangle of the layout, in logical coordinates.
    Area(Rectangle<i32, Logical>),
    /// An output by its connector name.
    Screen(String),
    ActiveScreen,
    /// Whatever the user picks: a window (kind 0) or a screen (kind 1).
    Interactive(u32),
    /// All outputs.
    Workspace,
}

/// The options every method takes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub include_cursor: bool,
    pub include_decoration: bool,
    pub include_shadow: bool,
    /// At the scale of the outputs (or the largest of them) rather than at
    /// one pixel per logical pixel.
    pub native_resolution: bool,
}

impl Options {
    fn parse(options: &HashMap<String, OwnedValue>) -> Self {
        let flag = |name: &str, default: bool| {
            options
                .get(name)
                .and_then(|v| v.downcast_ref::<bool>().ok())
                .unwrap_or(default)
        };
        Self {
            include_cursor: flag("include-cursor", false),
            include_decoration: flag("include-decoration", false),
            // KWin includes the shadow unless told otherwise.
            include_shadow: flag("include-shadow", true),
            native_resolution: flag("native-resolution", false),
        }
    }
}

/// A screenshot taken, with what it shows.
pub struct Shot {
    pub image: Screenshot,
    /// The window's handle, for window screenshots.
    pub window: Option<String>,
    /// The output's name, for screen screenshots.
    pub screen: Option<String>,
}

/// The errors KWin answers with.
// The variant names are KWin's D-Bus error names.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.kde.KWin.ScreenShot2.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    /// The user didn't pick anything.
    Cancelled(String),
    InvalidWindow(String),
    InvalidScreen(String),
    InvalidArea(String),
    FileDescriptor(String),
    NoAuthorized(String),
    InternalError(String),
}

/// A request handed to the event loop.
pub struct Request {
    pub capture: Capture,
    pub options: Options,
    pub reply: oneshot::Sender<Result<Shot, Error>>,
}

/// The `org.kde.KWin.ScreenShot2` object.
struct ScreenShot2 {
    requests: Sender<Request>,
}

impl ScreenShot2 {
    async fn capture(
        &self,
        capture: Capture,
        options: HashMap<String, OwnedValue>,
        pipe: OwnedFd,
    ) -> Result<HashMap<String, Value<'static>>, Error> {
        let (reply, answer) = oneshot::channel();
        let request = Request {
            capture,
            options: Options::parse(&options),
            reply,
        };
        self.requests
            .send(request)
            .map_err(|_| Error::InternalError("canoe is shutting down".into()))?;
        let shot = answer
            .await
            .map_err(|_| Error::InternalError("the screenshot was dropped".into()))??;
        let image = shot.image;
        let mut results: HashMap<String, Value<'static>> = HashMap::from([
            ("type".to_string(), Value::from("raw")),
            ("width".to_string(), Value::from(image.width)),
            ("height".to_string(), Value::from(image.height)),
            ("stride".to_string(), Value::from(image.width * 4)),
            (
                "format".to_string(),
                Value::from(QIMAGE_FORMAT_ARGB32_PREMULTIPLIED),
            ),
            ("scale".to_string(), Value::from(image.scale)),
        ]);
        if let Some(window) = shot.window {
            results.insert("windowId".to_string(), Value::from(window));
        }
        if let Some(screen) = shot.screen {
            results.insert("screen".to_string(), Value::from(screen));
        }
        // The caller reads once it has the reply; a pipe holds far less than
        // a screenshot.
        let pipe: std::os::fd::OwnedFd = pipe.into();
        std::thread::Builder::new()
            .name("canoe-screenshot".into())
            .spawn(move || {
                let mut file = std::fs::File::from(pipe);
                if let Err(e) = file.write_all(&image.data) {
                    eprintln!("canoe: writing a screenshot failed: {e}");
                }
            })
            .map_err(|e| Error::FileDescriptor(e.to_string()))?;
        Ok(results)
    }
}

#[zbus::interface(name = "org.kde.KWin.ScreenShot2")]
impl ScreenShot2 {
    #[zbus(property)]
    fn version(&self) -> u32 {
        VERSION
    }

    async fn capture_window(
        &self,
        handle: String,
        options: HashMap<String, OwnedValue>,
        pipe: OwnedFd,
    ) -> Result<HashMap<String, Value<'static>>, Error> {
        self.capture(Capture::Window(handle), options, pipe).await
    }

    async fn capture_active_window(
        &self,
        options: HashMap<String, OwnedValue>,
        pipe: OwnedFd,
    ) -> Result<HashMap<String, Value<'static>>, Error> {
        self.capture(Capture::ActiveWindow, options, pipe).await
    }

    async fn capture_area(
        &self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        options: HashMap<String, OwnedValue>,
        pipe: OwnedFd,
    ) -> Result<HashMap<String, Value<'static>>, Error> {
        let width = i32::try_from(width).map_err(|_| Error::InvalidArea("too wide".into()))?;
        let height = i32::try_from(height).map_err(|_| Error::InvalidArea("too high".into()))?;
        let area = Rectangle::new((x, y).into(), (width, height).into());
        self.capture(Capture::Area(area), options, pipe).await
    }

    async fn capture_screen(
        &self,
        name: String,
        options: HashMap<String, OwnedValue>,
        pipe: OwnedFd,
    ) -> Result<HashMap<String, Value<'static>>, Error> {
        self.capture(Capture::Screen(name), options, pipe).await
    }

    async fn capture_active_screen(
        &self,
        options: HashMap<String, OwnedValue>,
        pipe: OwnedFd,
    ) -> Result<HashMap<String, Value<'static>>, Error> {
        self.capture(Capture::ActiveScreen, options, pipe).await
    }

    async fn capture_interactive(
        &self,
        kind: u32,
        options: HashMap<String, OwnedValue>,
        pipe: OwnedFd,
    ) -> Result<HashMap<String, Value<'static>>, Error> {
        self.capture(Capture::Interactive(kind), options, pipe)
            .await
    }

    async fn capture_workspace(
        &self,
        options: HashMap<String, OwnedValue>,
        pipe: OwnedFd,
    ) -> Result<HashMap<String, Value<'static>>, Error> {
        self.capture(Capture::Workspace, options, pipe).await
    }
}

/// The running emulation: the bus connection, and screenshots waiting for
/// the user to pick what to capture.
pub struct KwinScreenshot {
    /// Dropping it gives up the bus name.
    _connection: zbus::blocking::Connection,
    channel: RegistrationToken,
    picking: HashMap<ScreenshotRequestId, Request>,
    next_pick: ScreenshotRequestId,
}

impl KwinScreenshot {
    /// Take the name `org.kde.KWin` on the session bus and serve the
    /// screenshot interface, handing requests to the event loop.
    pub fn start(handle: &LoopHandle<'static, State>) -> Result<Self, String> {
        let (sender, receiver): (Sender<Request>, Channel<Request>) = channel::channel();
        let connection = zbus::blocking::connection::Builder::session()
            .and_then(|b| b.serve_at(OBJECT_PATH, ScreenShot2 { requests: sender }))
            .and_then(|b| b.name(BUS_NAME))
            .and_then(|b| b.name(SCREENSHOT_BUS_NAME))
            .and_then(|b| b.build())
            .map_err(|e| format!("can't serve {BUS_NAME} on the session bus: {e}"))?;
        let channel = handle
            .insert_source(receiver, |event, _, state| {
                if let Event::Msg(request) = event {
                    state.kwin_screenshot_request(request);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            _connection: connection,
            channel,
            picking: HashMap::new(),
            next_pick: 0,
        })
    }

    /// Stop serving, cancelling screenshots still being picked.
    pub fn stop(self, handle: &LoopHandle<'static, State>) {
        handle.remove(self.channel);
        for (_, request) in self.picking {
            let _ = request
                .reply
                .send(Err(Error::Cancelled("canoe stopped emulating KWin".into())));
        }
    }
}

impl State {
    /// Start or stop the KWin emulation as the configuration says.
    pub fn apply_kwin_emulation(&mut self) {
        let wanted = self.context.borrow().config.emulate_kwin;
        match (wanted, self.kwin_screenshot.is_some()) {
            (true, false) => match KwinScreenshot::start(&self.loop_handle) {
                Ok(kwin) => self.kwin_screenshot = Some(kwin),
                Err(e) => eprintln!("canoe: KWin emulation unavailable: {e}"),
            },
            (false, true) => {
                if let Some(kwin) = self.kwin_screenshot.take() {
                    kwin.stop(&self.loop_handle);
                }
            }
            _ => {}
        }
    }

    fn kwin_screenshot_request(&mut self, request: Request) {
        if self.lock_state != LockState::Unlocked {
            let _ = request
                .reply
                .send(Err(Error::NoAuthorized("the session is locked".into())));
            return;
        }
        if let Capture::Interactive(kind) = request.capture {
            let Some(kwin) = self.kwin_screenshot.as_mut() else {
                return;
            };
            let id = kwin.next_pick;
            kwin.next_pick += 1;
            kwin.picking.insert(id, request);
            if !super::wm_glue::start_screenshot_picker(self, id, kind == 0) {
                if let Some(request) = self
                    .kwin_screenshot
                    .as_mut()
                    .and_then(|kwin| kwin.picking.remove(&id))
                {
                    let _ = request.reply.send(Err(Error::Cancelled(
                        "there is nothing to pick, or something else is being picked".into(),
                    )));
                }
            }
            return;
        }
        let result = self.kwin_screenshot_take(&request.capture, request.options);
        let _ = request.reply.send(result);
    }

    /// The user picked `target` (or nothing) for screenshot `request`.
    pub fn screenshot_picked(&mut self, request: ScreenshotRequestId, target: Option<ShareTarget>) {
        let Some(request) = self
            .kwin_screenshot
            .as_mut()
            .and_then(|kwin| kwin.picking.remove(&request))
        else {
            return;
        };
        let capture = match target {
            Some(ShareTarget::Window(id)) => self
                .window_by_id(id)
                .and_then(|w| w.foreign.as_ref())
                .map(|foreign| Capture::Window(foreign.identifier())),
            Some(ShareTarget::Output(id)) => self
                .outputs
                .iter()
                .find(|o| o.id == id)
                .map(|o| Capture::Screen(o.output.name())),
            None => None,
        };
        let result = match capture {
            Some(capture) => self.kwin_screenshot_take(&capture, request.options),
            None => Err(Error::Cancelled("nothing was picked".into())),
        };
        let _ = request.reply.send(result);
    }

    fn kwin_screenshot_take(&mut self, capture: &Capture, options: Options) -> Result<Shot, Error> {
        let failed = || Error::InternalError("taking the screenshot failed".into());
        match capture {
            Capture::Window(handle) => {
                let key = self
                    .window_key_for_identifier(handle)
                    .ok_or_else(|| Error::InvalidWindow(format!("no window {handle}")))?;
                self.kwin_window_shot(key, options)
            }
            Capture::ActiveWindow => {
                let focused = self.context.borrow().focused_window;
                let key = focused
                    .and_then(|id| self.window_by_id(id))
                    .filter(|w| !w.hidden)
                    .map(|w| w.key)
                    .ok_or_else(|| Error::InvalidWindow("no window is active".into()))?;
                self.kwin_window_shot(key, options)
            }
            Capture::Area(area) => {
                let scale = (!options.native_resolution).then_some(1.0);
                let image = self
                    .screenshot_area(*area, scale, options.include_cursor)
                    .ok_or_else(|| Error::InvalidArea("the area shows no screen".into()))?;
                Ok(Shot {
                    image,
                    window: None,
                    screen: None,
                })
            }
            Capture::Screen(name) => {
                let output = self
                    .outputs
                    .iter()
                    .find(|o| o.displayed() && o.output.name() == *name)
                    .map(|o| o.output.clone())
                    .ok_or_else(|| Error::InvalidScreen(format!("no screen {name}")))?;
                self.kwin_screen_shot(&output, options)
            }
            Capture::ActiveScreen => {
                let output = self.kwin_active_output().ok_or_else(|| {
                    Error::InvalidScreen("no screen is active".into())
                })?;
                self.kwin_screen_shot(&output, options)
            }
            Capture::Workspace => {
                let area = self
                    .outputs
                    .iter()
                    .filter(|o| o.displayed())
                    .filter_map(|o| self.output_geometry(&o.output))
                    .reduce(|a, b| a.merge(b))
                    .ok_or_else(|| Error::InvalidScreen("there are no screens".into()))?;
                let scale = (!options.native_resolution).then_some(1.0);
                let image = self
                    .screenshot_area(area, scale, options.include_cursor)
                    .ok_or_else(failed)?;
                Ok(Shot {
                    image,
                    window: None,
                    screen: None,
                })
            }
            Capture::Interactive(_) => Err(Error::InternalError(
                "an interactive screenshot can't be taken directly".into(),
            )),
        }
    }

    fn kwin_window_shot(
        &mut self,
        key: crate::wm::WindowKey,
        options: Options,
    ) -> Result<Shot, Error> {
        let window = self
            .window_by_key(key)
            .and_then(|w| w.foreign.as_ref())
            .map(|f| f.identifier());
        let capture = WindowCapture {
            key,
            decoration: options.include_decoration,
            shadow: options.include_decoration && options.include_shadow,
            scale: (!options.native_resolution).then_some(1.0),
        };
        let image = self
            .screenshot_window(capture)
            .ok_or_else(|| Error::InternalError("taking the screenshot failed".into()))?;
        Ok(Shot {
            image,
            window,
            screen: None,
        })
    }

    fn kwin_screen_shot(
        &mut self,
        output: &smithay::output::Output,
        options: Options,
    ) -> Result<Shot, Error> {
        let image = if options.native_resolution {
            self.screenshot_output(output, options.include_cursor)
        } else {
            let area = self
                .output_geometry(output)
                .ok_or_else(|| Error::InvalidScreen("the screen is off".into()))?;
            self.screenshot_area(area, Some(1.0), options.include_cursor)
        }
        .ok_or_else(|| Error::InternalError("taking the screenshot failed".into()))?;
        Ok(Shot {
            image,
            window: None,
            screen: Some(output.name()),
        })
    }

    /// The output the user works on: the window manager's current output,
    /// or the one under the pointer.
    fn kwin_active_output(&self) -> Option<smithay::output::Output> {
        let current: Option<canoe::OutputId> = self.context.borrow().current_output;
        current
            .and_then(|id| self.outputs.iter().find(|o| o.id == id && o.displayed()))
            .map(|o| o.output.clone())
            .or_else(|| {
                let at = self.pointer_location.to_i32_floor();
                self.outputs
                    .iter()
                    .filter(|o| o.displayed())
                    .find(|o| {
                        self.output_geometry(&o.output)
                            .is_some_and(|g| g.contains(at))
                    })
                    .map(|o| o.output.clone())
            })
    }
}
