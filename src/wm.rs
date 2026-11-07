//! The window-management interface between canoe's window manager and its
//! compositor.
//!
//! Canoe started out as a client of River's `river-window-management-v1`
//! protocol, and its window manager (`crate::canoe`) still thinks in that
//! protocol's terms: the compositor reports state changes, then runs a
//! *manage sequence* in which the window manager decides on window
//! management state (focus, proposed sizes, fullscreen, ...), followed by a
//! *render sequence* in which it places windows and draws decorations.
//!
//! Now that canoe is the compositor, the objects the window manager used to
//! hold are in-process handles. Their methods do what the protocol requests
//! did: they queue a [`Request`] that the compositor applies once the
//! sequence is finished. Keeping the queue (rather than letting the window
//! manager reach into compositor state directly) keeps both sides
//! independently borrowable and preserves the transactional semantics the
//! window manager was written against.

#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

/// Compositor-side key of a toplevel window.
pub type WindowKey = u64;
/// Compositor-side key of an output.
pub type OutputKey = u64;
/// Compositor-side key of a seat.
pub type SeatKey = u64;

bitflags::bitflags! {
    /// Window edges, numbered like `xdg_toplevel.resize_edge`'s bits.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Edges: u32 {
        const Top = 1;
        const Bottom = 2;
        const Left = 4;
        const Right = 8;
    }
}

bitflags::bitflags! {
    /// What the user may ask a window to do (`xdg_toplevel.wm_capabilities`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Capabilities: u32 {
        const WindowMenu = 1;
        const Maximize = 2;
        const Fullscreen = 4;
        const Minimize = 8;
    }
}

/// Cursor images the window manager asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorShape {
    Default,
    Pointer,
    Crosshair,
    NResize,
    SResize,
    EResize,
    WResize,
    NeResize,
    NwResize,
    SeResize,
    SwResize,
    EwResize,
    NsResize,
    AllResize,
}

impl CursorShape {
    /// The XCursor name of the shape.
    pub fn xcursor_name(self) -> &'static str {
        match self {
            CursorShape::Default => "default",
            CursorShape::Pointer => "pointer",
            CursorShape::Crosshair => "crosshair",
            CursorShape::NResize => "n-resize",
            CursorShape::SResize => "s-resize",
            CursorShape::EResize => "e-resize",
            CursorShape::WResize => "w-resize",
            CursorShape::NeResize => "ne-resize",
            CursorShape::NwResize => "nw-resize",
            CursorShape::SeResize => "se-resize",
            CursorShape::SwResize => "sw-resize",
            CursorShape::EwResize => "ew-resize",
            CursorShape::NsResize => "ns-resize",
            CursorShape::AllResize => "all-scroll",
        }
    }
}

/// A box relative to a window's top left corner. Zero width or height
/// disables clipping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipBox {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl ClipBox {
    pub fn is_enabled(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// Something the window manager asked the compositor to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Start another manage sequence once this one is finished.
    ManageDirty,

    ProposeDimensions(WindowKey, i32, i32),
    SetDimensionBounds(WindowKey, i32, i32),
    Hide(WindowKey),
    Show(WindowKey),
    Close(WindowKey),
    SetTiled(WindowKey, Edges),
    Fullscreen(WindowKey, OutputKey),
    ExitFullscreen(WindowKey),
    InformFullscreen(WindowKey, bool),
    InformMaximized(WindowKey, bool),
    InformResizing(WindowKey, bool),
    UseSsd(WindowKey, bool),
    SetCapabilities(WindowKey, Capabilities),
    SetClipBox(WindowKey, ClipBox),
    SetContentClipBox(WindowKey, ClipBox),

    SetPosition(WindowKey, i32, i32),
    PlaceTop(WindowKey),
    PlaceBottom(WindowKey),
    PlaceAbove(WindowKey, WindowKey),
    PlaceBelow(WindowKey, WindowKey),

    FocusWindow(SeatKey, WindowKey),
    ClearFocus(SeatKey),
    OpStartPointer(SeatKey),
    OpEnd(SeatKey),
    SetXcursorTheme(SeatKey, String, u32),
    SetCursorShape(SeatKey, Option<CursorShape>),

    /// Layer surfaces created without an output open on this one.
    SetDefaultLayerOutput(OutputKey),
}

/// The queue the handles below write their requests to.
#[derive(Clone, Default)]
pub struct Requests(Rc<RefCell<Vec<Request>>>);

impl Requests {
    pub fn push(&self, request: Request) {
        self.0.borrow_mut().push(request);
    }

    /// Take everything queued so far, in order.
    pub fn drain(&self) -> Vec<Request> {
        std::mem::take(&mut *self.0.borrow_mut())
    }

    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }
}

/// The window manager's handle on the compositor as a whole.
#[derive(Clone)]
pub struct WindowManager {
    requests: Requests,
}

impl WindowManager {
    pub fn new(requests: Requests) -> Self {
        Self { requests }
    }

    /// Ensure another manage sequence runs.
    pub fn manage_dirty(&self) {
        self.requests.push(Request::ManageDirty);
    }

    /// The window manager is done with the current manage sequence. Requests
    /// are applied by the compositor when the sequence returns, so this is
    /// only kept to mark the spot in the window manager's code.
    pub fn manage_finish(&self) {}

    /// See [`WindowManager::manage_finish`].
    pub fn render_finish(&self) {}
}

/// A toplevel window, as seen by the window manager.
#[derive(Clone)]
pub struct WindowHandle {
    key: WindowKey,
    requests: Requests,
}

impl PartialEq for WindowHandle {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl std::fmt::Debug for WindowHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WindowHandle({})", self.key)
    }
}

impl WindowHandle {
    pub fn new(key: WindowKey, requests: Requests) -> Self {
        Self { key, requests }
    }

    pub fn key(&self) -> WindowKey {
        self.key
    }

    /// Kept for code written against versioned protocol objects: every
    /// request is available in-process.
    pub fn version(&self) -> u32 {
        u32::MAX
    }

    fn push(&self, request: Request) {
        self.requests.push(request);
    }

    /// Propose a size; zero lets the window pick its own.
    pub fn propose_dimensions(&self, width: i32, height: i32) {
        self.push(Request::ProposeDimensions(self.key, width, height));
    }

    pub fn set_dimension_bounds(&self, max_width: i32, max_height: i32) {
        self.push(Request::SetDimensionBounds(self.key, max_width, max_height));
    }

    pub fn hide(&self) {
        self.push(Request::Hide(self.key));
    }

    pub fn show(&self) {
        self.push(Request::Show(self.key));
    }

    pub fn close(&self) {
        self.push(Request::Close(self.key));
    }

    /// Compositor-drawn borders. Canoe draws its own frame, so the only
    /// thing ever asked for is "none", which is what the compositor does.
    pub fn set_borders(&self, _edges: Edges, _width: i32, _r: u32, _g: u32, _b: u32, _a: u32) {}

    pub fn set_tiled(&self, edges: Edges) {
        self.push(Request::SetTiled(self.key, edges));
    }

    pub fn fullscreen(&self, output: &OutputHandle) {
        self.push(Request::Fullscreen(self.key, output.key));
    }

    pub fn exit_fullscreen(&self) {
        self.push(Request::ExitFullscreen(self.key));
    }

    pub fn inform_fullscreen(&self) {
        self.push(Request::InformFullscreen(self.key, true));
    }

    pub fn inform_not_fullscreen(&self) {
        self.push(Request::InformFullscreen(self.key, false));
    }

    pub fn inform_maximized(&self) {
        self.push(Request::InformMaximized(self.key, true));
    }

    pub fn inform_unmaximized(&self) {
        self.push(Request::InformMaximized(self.key, false));
    }

    pub fn inform_resize_start(&self) {
        self.push(Request::InformResizing(self.key, true));
    }

    pub fn inform_resize_end(&self) {
        self.push(Request::InformResizing(self.key, false));
    }

    pub fn use_csd(&self) {
        self.push(Request::UseSsd(self.key, false));
    }

    pub fn use_ssd(&self) {
        self.push(Request::UseSsd(self.key, true));
    }

    pub fn set_capabilities(&self, caps: Capabilities) {
        self.push(Request::SetCapabilities(self.key, caps));
    }

    pub fn set_clip_box(&self, x: i32, y: i32, width: i32, height: i32) {
        self.push(Request::SetClipBox(
            self.key,
            ClipBox {
                x,
                y,
                width,
                height,
            },
        ));
    }

    pub fn set_content_clip_box(&self, x: i32, y: i32, width: i32, height: i32) {
        self.push(Request::SetContentClipBox(
            self.key,
            ClipBox {
                x,
                y,
                width,
                height,
            },
        ));
    }

    /// The window's place in the rendering order and on screen.
    pub fn node(&self) -> NodeHandle {
        NodeHandle {
            key: self.key,
            requests: self.requests.clone(),
        }
    }
}

/// A window's place in the rendering order and in the layout.
#[derive(Clone)]
pub struct NodeHandle {
    key: WindowKey,
    requests: Requests,
}

impl NodeHandle {
    pub fn set_position(&self, x: i32, y: i32) {
        self.requests.push(Request::SetPosition(self.key, x, y));
    }

    pub fn place_top(&self) {
        self.requests.push(Request::PlaceTop(self.key));
    }

    pub fn place_bottom(&self) {
        self.requests.push(Request::PlaceBottom(self.key));
    }

    pub fn place_above(&self, other: &NodeHandle) {
        self.requests.push(Request::PlaceAbove(self.key, other.key));
    }

    pub fn place_below(&self, other: &NodeHandle) {
        self.requests.push(Request::PlaceBelow(self.key, other.key));
    }
}

/// An output, as seen by the window manager.
#[derive(Clone)]
pub struct OutputHandle {
    key: OutputKey,
    requests: Requests,
}

impl PartialEq for OutputHandle {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl OutputHandle {
    pub fn new(key: OutputKey, requests: Requests) -> Self {
        Self { key, requests }
    }

    pub fn key(&self) -> OutputKey {
        self.key
    }

    /// Open layer surfaces that don't name an output on this one.
    pub fn set_default_layer_output(&self) {
        self.requests.push(Request::SetDefaultLayerOutput(self.key));
    }
}

/// A seat, as seen by the window manager.
#[derive(Clone)]
pub struct SeatHandle {
    key: SeatKey,
    requests: Requests,
}

impl PartialEq for SeatHandle {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl SeatHandle {
    pub fn new(key: SeatKey, requests: Requests) -> Self {
        Self { key, requests }
    }

    pub fn key(&self) -> SeatKey {
        self.key
    }

    pub fn version(&self) -> u32 {
        u32::MAX
    }

    pub fn focus_window(&self, window: &WindowHandle) {
        self.requests
            .push(Request::FocusWindow(self.key, window.key()));
    }

    pub fn clear_focus(&self) {
        self.requests.push(Request::ClearFocus(self.key));
    }

    /// Start an interactive pointer operation: until it ends, the pointer's
    /// motion is reported as deltas instead of reaching clients.
    pub fn op_start_pointer(&self) {
        self.requests.push(Request::OpStartPointer(self.key));
    }

    pub fn op_end(&self) {
        self.requests.push(Request::OpEnd(self.key));
    }

    pub fn set_xcursor_theme(&self, name: String, size: u32) {
        self.requests
            .push(Request::SetXcursorTheme(self.key, name, size));
    }

    /// Show `shape` while the pointer is over a surface of the window
    /// manager's, or during a pointer operation. `None` hides the cursor.
    pub fn set_cursor_shape(&self, shape: Option<CursorShape>) {
        self.requests.push(Request::SetCursorShape(self.key, shape));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_keep_their_order() {
        let requests = Requests::default();
        let window = WindowHandle::new(7, requests.clone());
        window.propose_dimensions(0, 0);
        window.node().set_position(10, 20);
        window.use_ssd();
        assert_eq!(
            requests.drain(),
            vec![
                Request::ProposeDimensions(7, 0, 0),
                Request::SetPosition(7, 10, 20),
                Request::UseSsd(7, true),
            ]
        );
        assert!(requests.is_empty());
    }
}
