//! Window management

#![allow(dead_code)]

use super::titlebar::TitlebarButton;
use super::WindowShadow;
use crate::config::WindowDecoration;
use crate::wm::{Capabilities, Edges, NodeHandle, OutputHandle, WindowHandle};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::Arc;

/// Window identifier
pub type WindowId = usize;

/// Fullscreen state
#[derive(Debug, Clone)]
pub enum FullscreenState {
    None,
    /// Fullscreen within window bounds
    Window,
    /// Fullscreen on a specific output
    Output(Weak<RefCell<super::Output>>),
}

/// Window snap state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapState {
    Left,
    Right,
    Maximized,
}

impl SnapState {
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Maximized => Self::Maximized,
        }
    }
}

/// Window clip state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipState {
    #[default]
    Unknown,
    Normal,
    Clipped,
}

/// Saved geometry for restoring after maximize
#[derive(Debug, Clone, Copy)]
pub struct SavedGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Operator state for move/resize operations
#[derive(Debug, Clone, Default)]
pub enum Operator {
    #[default]
    None,
    Move {
        start_x: i32,
        start_y: i32,
        seat: Option<Weak<RefCell<super::Seat>>>,
    },
    Resize {
        start_x: i32,
        start_y: i32,
        start_width: i32,
        start_height: i32,
        edges: u32,
        seat: Option<Weak<RefCell<super::Seat>>>,
    },
}

/// Window event types
#[derive(Debug, Clone)]
pub enum WindowEvent {
    Init,
    Close,
    Fullscreen(Option<Weak<RefCell<super::Output>>>),
    Unfullscreen,
    Maximize,
    Unmaximize,
    Minimize,
    Move(Weak<RefCell<super::Seat>>),
    Resize(Weak<RefCell<super::Seat>>, u32), // seat, edges
}

/// A managed window
pub struct Window {
    /// Unique window ID
    pub id: WindowId,
    /// The compositor's window
    pub rwm_window: Option<WindowHandle>,
    /// The window's place in the compositor's layout and rendering order
    pub rwm_node: Option<NodeHandle>,

    /// Associated output
    pub output: Option<Weak<RefCell<super::Output>>>,
    /// Process ID
    pub pid: i32,
    /// Application ID
    pub app_id: Option<String>,
    /// Window title
    pub title: Option<String>,
    /// The icon the client set for this window (xdg-toplevel-icon), if any
    pub client_icon: Option<Arc<super::ClientIcon>>,
    /// `client_icon` drawn at a size in pixels; cleared when it changes
    pub client_icon_pixmap: Option<(i32, Option<Rc<resvg::tiny_skia::Pixmap>>)>,
    /// The client asked for the window to be left out of the window menu,
    /// Super+Tab and focus cycling (org_kde_plasma_surface.set_skip_switcher)
    pub skip_switcher: bool,
    /// Compositor-assigned identifier, shared with ext-foreign-toplevel-list-v1
    /// (river_window_v1 version 4 and later)
    pub identifier: Option<String>,
    /// Parent window (if any)
    pub parent: Option<WindowId>,
    /// Swallow top pixels from window content
    pub swallow_top: i32,

    /// Position X
    pub x: i32,
    /// Position Y
    pub y: i32,
    /// Width
    pub width: i32,
    /// Height
    pub height: i32,
    /// Minimum width
    pub min_width: i32,
    /// Minimum height
    pub min_height: i32,
    /// Maximum width hint (0 = no preference)
    pub max_width: i32,
    /// Maximum height hint (0 = no preference)
    pub max_height: i32,

    /// Fullscreen state
    pub fullscreen: FullscreenState,
    /// Maximized state
    pub maximized: bool,
    /// Geometry to restore when exiting fullscreen
    pub pre_fullscreen: Option<SavedGeometry>,
    /// Geometry to restore when unsnapping
    pub pre_snap: Option<SavedGeometry>,
    /// Current snap state
    pub snap_state: Option<SnapState>,
    /// Restore geometry on the next manage sequence after exiting fullscreen
    pub pending_unfullscreen_restore: bool,
    /// Floating state
    pub floating: bool,
    /// Hidden state
    pub hidden: bool,
    /// Sequence number assigned when minimized (for ordering icons)
    pub minimize_seq: u64,
    /// Clip state
    pub clip_state: ClipState,

    /// Decoration mode
    pub decoration: Option<WindowDecoration>,
    /// Decoration hint from client (None = not received yet)
    pub decoration_hint: Option<u32>,

    /// Current operator (move/resize)
    pub operator: Operator,
    /// Pending events queue
    pub unhandled_events: VecDeque<WindowEvent>,
    /// Position is undefined (newly created)
    pub position_undefined: bool,
    /// Placed while larger than its output's usable area; propose a size
    /// that fits in the next manage sequence
    pub pending_initial_fit: bool,
    /// Titlebar for server-side decoration
    pub titlebar: Option<super::Titlebar>,
    /// Shadow decoration surface
    pub shadow: Option<WindowShadow>,
    /// Hovered titlebar button (if any)
    pub titlebar_hovered: Option<TitlebarButton>,
    /// Pressed titlebar button (if any)
    pub titlebar_pressed: Option<TitlebarButton>,
    /// Whether left mouse is currently held on the titlebar surface
    pub titlebar_left_down: bool,

    /// Window needs to be configured (propose_dimensions)
    pub needs_configure: bool,

    /// Proposed dimensions (for sizing)
    proposed_width: i32,
    proposed_height: i32,
}

impl Window {
    /// Create a new window
    pub fn new(id: WindowId) -> Self {
        Self {
            id,
            rwm_window: None,
            rwm_node: None,
            output: None,
            pid: 0,
            app_id: None,
            title: None,
            client_icon: None,
            client_icon_pixmap: None,
            skip_switcher: false,
            identifier: None,
            parent: None,
            swallow_top: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            min_width: 0,
            min_height: 0,
            max_width: 0,
            max_height: 0,
            fullscreen: FullscreenState::None,
            maximized: false,
            pre_fullscreen: None,
            pre_snap: None,
            snap_state: None,
            pending_unfullscreen_restore: false,
            floating: false,
            hidden: false,
            minimize_seq: 0,
            clip_state: ClipState::Unknown,
            decoration: None,
            decoration_hint: None,
            operator: Operator::None,
            unhandled_events: VecDeque::new(),
            position_undefined: true,
            pending_initial_fit: false,
            titlebar: None,
            shadow: None,
            titlebar_hovered: None,
            titlebar_pressed: None,
            titlebar_left_down: false,
            needs_configure: true,
            proposed_width: 0,
            proposed_height: 0,
        }
    }

    /// True when the window is a dialog-like child of another window. We use
    /// the parent relationship as the only available signal, since the River
    /// window-management protocol does not surface xdg-dialog-v1's modal flag.
    pub fn is_dialog(&self) -> bool {
        self.parent.is_some()
    }

    /// True when the client pinned the window to a fixed size: min == max on
    /// both axes, both nonzero. This is the canonical Wayland signal for a
    /// non-resizable toplevel.
    pub fn is_fixed_size(&self) -> bool {
        self.min_width > 0
            && self.max_width == self.min_width
            && self.min_height > 0
            && self.max_height == self.min_height
    }

    /// Whether the user may resize this window by dragging its borders.
    pub fn is_resizable(&self) -> bool {
        !self.is_fixed_size()
    }

    /// Whether the SSD titlebar should expose a minimize button.
    pub fn has_minimize_button(&self) -> bool {
        !self.is_dialog()
    }

    /// Whether the SSD titlebar should expose a maximize button.
    pub fn has_maximize_button(&self) -> bool {
        !self.is_dialog() && !self.is_fixed_size()
    }

    /// The frame style to render for this window's SSD decoration.
    pub fn frame_style(&self) -> super::titlebar::FrameStyle {
        if self.is_dialog() {
            super::titlebar::FrameStyle::Dialog
        } else if self.is_fixed_size() {
            super::titlebar::FrameStyle::FixedSize
        } else {
            super::titlebar::FrameStyle::Normal
        }
    }

    /// How this window's frame takes up fractional-scale rounding: tiled
    /// windows meet the screen edges and each other exactly; floating ones
    /// keep their border and titlebar a constant size as they move.
    pub fn frame_fit(&self) -> super::titlebar::FrameFit {
        if self.maximized || self.snap_state.is_some() {
            super::titlebar::FrameFit::Tiled
        } else {
            super::titlebar::FrameFit::Floating
        }
    }

    /// The rectangle the window covers on screen, decorations included, in
    /// global logical coordinates: `(x, y, width, height)`.
    pub fn frame_rect(&self, ui: &crate::config::UiConfig) -> (i32, i32, i32, i32) {
        let fullscreen = !matches!(self.fullscreen, FullscreenState::None);
        if fullscreen || self.decoration == Some(WindowDecoration::Csd) {
            return (self.x, self.y, self.width, self.height);
        }
        let border = super::titlebar::border_width(ui, self.frame_style());
        let titlebar = super::titlebar::titlebar_height(ui);
        let swallow_top = self.swallow_top.clamp(0, self.height);
        (
            self.x - border,
            self.y - border - titlebar + swallow_top,
            self.width + border * 2,
            self.height - swallow_top + titlebar + border * 2,
        )
    }

    /// Check if window is visible on the given output
    pub fn is_visible_on(&self, output: &super::Output) -> bool {
        if self.hidden {
            return false;
        }

        // Check if window has the same output
        if let Some(ref win_output) = self.output {
            if let Some(win_output) = win_output.upgrade() {
                if win_output.borrow().id != output.id {
                    return false;
                }
            }
        }

        true
    }

    /// Propose dimensions for the window
    pub fn propose_dimensions(&mut self, width: i32, height: i32) {
        self.proposed_width = width.max(self.min_width);
        self.proposed_height = height.max(self.min_height);

        // Send to compositor via protocol
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.propose_dimensions(self.proposed_width, self.proposed_height);
        }
    }

    /// Let the window pick its own preferred size.
    ///
    /// Per the river-window-management protocol, proposing (0, 0) means "let
    /// the window decide its own dimensions". River then asks the client (via
    /// an xdg_toplevel configure of 0x0) to choose, and reports the size the
    /// window picks back to us in a `dimensions` event. This is how a client's
    /// preferred/default geometry reaches us — there is no other event that
    /// surfaces it. We must propose *something* for the window to be displayed,
    /// so this is the request to use for the initial configure when we want to
    /// honor the app's own size rather than imposing one.
    ///
    /// Note we deliberately bypass the `propose_dimensions` min-size clamp: the
    /// zeros must reach River verbatim, otherwise a client that advertises a
    /// minimum (e.g. a terminal's 1-cell minimum) would be pinned to that
    /// minimum instead of its preferred size.
    pub fn propose_preferred_dimensions(&mut self) {
        self.proposed_width = 0;
        self.proposed_height = 0;
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.propose_dimensions(0, 0);
        }
    }

    /// The dimensions last proposed to the compositor
    #[cfg(test)]
    pub fn proposed_dimensions(&self) -> (i32, i32) {
        (self.proposed_width, self.proposed_height)
    }

    /// Recommend that the window keep within the given size when it picks
    /// its own dimensions. Only a hint; ignored by River before version 4.
    pub fn set_dimension_bounds(&self, max_width: i32, max_height: i32) {
        if let Some(ref rwm_window) = self.rwm_window {
            if rwm_window.version() >= 4 {
                rwm_window.set_dimension_bounds(max_width.max(0), max_height.max(0));
            }
        }
    }

    /// Set window position
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
        self.position_undefined = false;
        if matches!(self.fullscreen, FullscreenState::None) && !self.pending_unfullscreen_restore {
            let saved = self.pre_fullscreen.get_or_insert(SavedGeometry {
                x,
                y,
                width: self.width,
                height: self.height,
            });
            saved.x = x;
            saved.y = y;
        }

        if let Some(ref rwm_node) = self.rwm_node {
            rwm_node.set_position(x, y);
        }
    }

    /// Update the window's position without moving it (called when the
    /// compositor kept an edge in place because the client took another
    /// size than proposed)
    pub fn update_position(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
        if matches!(self.fullscreen, FullscreenState::None) && !self.pending_unfullscreen_restore {
            if let Some(ref mut saved) = self.pre_fullscreen {
                saved.x = x;
                saved.y = y;
            }
        }
    }

    /// Update window dimensions (called when compositor confirms dimensions)
    pub fn update_dimensions(&mut self, width: i32, height: i32) {
        self.width = width;
        self.height = height;
        if matches!(self.fullscreen, FullscreenState::None) && !self.pending_unfullscreen_restore {
            let saved = self.pre_fullscreen.get_or_insert(SavedGeometry {
                x: self.x,
                y: self.y,
                width,
                height,
            });
            saved.x = self.x;
            saved.y = self.y;
            saved.width = width;
            saved.height = height;
        }
    }

    /// Set the number of pixels to swallow from the top of the window
    pub fn set_swallow_top(&mut self, swallow_top: i32) {
        self.swallow_top = swallow_top.max(0);
    }

    /// Hide the window
    pub fn hide(&mut self) {
        if !self.hidden {
            self.hidden = true;
            if let Some(ref rwm_window) = self.rwm_window {
                rwm_window.hide();
            }
        }
    }

    /// Show the window
    pub fn show(&mut self) {
        if self.hidden {
            self.hidden = false;
            if let Some(ref rwm_window) = self.rwm_window {
                rwm_window.show();
            }
            // Force the titlebar to re-render + commit on the next render
            // sequence. Without a fresh commit the decoration surface stays
            // unmapped after rwm_window.show() and no wl_pointer.Enter
            // event fires for it, so titlebar clicks never reach us.
            if let Some(ref mut titlebar) = self.titlebar {
                titlebar.dirty = true;
            }
        }
    }

    /// Request window close
    pub fn close(&self) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.close();
        }
    }

    /// Set window borders
    pub fn set_borders(&self, edges: Edges, width: i32, r: u32, g: u32, b: u32, a: u32) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.set_borders(edges, width, r, g, b, a);
        }
    }

    /// Set window tiled edges
    pub fn set_tiled(&self, edges: Edges) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.set_tiled(edges);
        }
    }

    /// Request fullscreen on output
    pub fn fullscreen_on(&mut self, output: &OutputHandle) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.fullscreen(output);
            rwm_window.inform_fullscreen();
        }
    }

    /// Exit fullscreen
    pub fn exit_fullscreen(&mut self) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.exit_fullscreen();
            rwm_window.inform_not_fullscreen();
        }
        self.fullscreen = FullscreenState::None;
    }

    /// Whether the window is fullscreen on the output with the given id
    pub fn is_fullscreen_on(&self, output_id: super::OutputId) -> bool {
        match &self.fullscreen {
            FullscreenState::Output(output) => output
                .upgrade()
                .is_some_and(|output| output.borrow().id == output_id),
            _ => false,
        }
    }

    /// River implicitly exits fullscreen when the fullscreen output is
    /// removed (see river_window_v1.fullscreen). Sync our state and tell the
    /// client. Returns whether the window was fullscreen on a removed output;
    /// the caller must then propose a position and dimensions.
    pub fn clear_stale_fullscreen(&mut self) -> bool {
        let FullscreenState::Output(ref output) = self.fullscreen else {
            return false;
        };
        if output.upgrade().is_some() {
            return false;
        }
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.inform_not_fullscreen();
        }
        self.fullscreen = FullscreenState::None;
        self.pending_unfullscreen_restore = false;
        true
    }

    /// Inform window it's maximized
    pub fn inform_maximized(&self) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.inform_maximized();
        }
    }

    /// Inform window it's unmaximized
    pub fn inform_unmaximized(&self) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.inform_unmaximized();
        }
    }

    /// Animate the change of position and size that follows (see
    /// [`crate::wm::WindowHandle::animate_geometry`]).
    pub fn animate_geometry(&self, duration: std::time::Duration) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.animate_geometry(duration);
        }
    }

    /// Animate minimizing the window into its desktop icon, or bringing it
    /// back from there, as it is hidden or shown next (see
    /// [`crate::wm::WindowHandle::animate_iconify`]).
    pub fn animate_iconify(&self, duration: std::time::Duration) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.animate_iconify(duration);
        }
    }

    pub fn clear_maximized_without_restore(&mut self) {
        if self.maximized {
            self.maximized = false;
            self.inform_unmaximized();
        }
    }

    pub fn unmaximize_restore_size_only(&mut self) {
        if self.maximized {
            self.maximized = false;
            self.snap_state = None;
            if let Some(saved) = self.pre_snap.take() {
                self.propose_dimensions(saved.width, saved.height);
            }
            self.inform_unmaximized();
        }
    }

    /// Set decoration mode
    pub fn set_decoration(&self, decoration: WindowDecoration) {
        if let Some(ref rwm_window) = self.rwm_window {
            match decoration {
                WindowDecoration::Csd => rwm_window.use_csd(),
                WindowDecoration::Ssd => rwm_window.use_ssd(),
            }
        }
    }

    /// Inform the client which window-management capabilities are supported.
    /// Only callable inside a manage sequence.
    pub fn set_capabilities(&self, caps: Capabilities) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.set_capabilities(caps);
        }
    }

    /// Set clip box for rendering
    pub fn set_clip_box(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.set_clip_box(x, y, width, height);
        }
        self.clip_state = if width > 0 && height > 0 {
            ClipState::Clipped
        } else {
            ClipState::Normal
        };
    }

    /// Set clip box for content rendering (excludes decorations)
    pub fn set_content_clip_box(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.set_content_clip_box(x, y, width, height);
        }
    }

    /// Clear clip box
    pub fn clear_clip_box(&mut self) {
        self.set_clip_box(0, 0, 0, 0);
    }

    /// Place this window's node above another
    pub fn place_above(&self, other: &Window) {
        if let (Some(ref node), Some(ref other_node)) = (&self.rwm_node, &other.rwm_node) {
            node.place_above(other_node);
        }
    }

    /// Place this window's node below another
    pub fn place_below(&self, other: &Window) {
        if let (Some(ref node), Some(ref other_node)) = (&self.rwm_node, &other.rwm_node) {
            node.place_below(other_node);
        }
    }

    /// Place this window at top of render list
    pub fn place_top(&self) {
        if let Some(ref node) = self.rwm_node {
            node.place_top();
        }
    }

    /// Place this window at bottom of render list
    pub fn place_bottom(&self) {
        if let Some(ref node) = self.rwm_node {
            node.place_bottom();
        }
    }

    /// Queue an event for processing during manage phase
    pub fn queue_event(&mut self, event: WindowEvent) {
        self.unhandled_events.push_back(event);
    }

    /// Process queued events
    pub fn handle_events(&mut self) -> Vec<WindowEvent> {
        self.unhandled_events.drain(..).collect()
    }

    /// Start a move operation
    pub fn start_move(&mut self, seat: Weak<RefCell<super::Seat>>) {
        self.operator = Operator::Move {
            start_x: self.x,
            start_y: self.y,
            seat: Some(seat),
        };
    }

    /// Start a resize operation
    pub fn start_resize(&mut self, seat: Weak<RefCell<super::Seat>>, edges: u32) {
        self.operator = Operator::Resize {
            start_x: self.x,
            start_y: self.y,
            start_width: self.width,
            start_height: self.height,
            edges,
            seat: Some(seat),
        };

        if let Some(ref rwm_window) = self.rwm_window {
            rwm_window.inform_resize_start();
        }
    }

    /// End current operation
    pub fn end_operation(&mut self) {
        if matches!(self.operator, Operator::Resize { .. }) {
            if let Some(ref rwm_window) = self.rwm_window {
                rwm_window.inform_resize_end();
            }
        }
        self.operator = Operator::None;
    }

    /// Apply operation delta
    pub fn apply_op_delta(&mut self, dx: i32, dy: i32) {
        match &self.operator {
            Operator::Move {
                start_x, start_y, ..
            } => {
                self.set_position(*start_x + dx, *start_y + dy);
            }
            Operator::Resize {
                start_x,
                start_y,
                start_width,
                start_height,
                edges,
                ..
            } => {
                let edges = *edges;
                let mut new_width = *start_width;
                let mut new_height = *start_height;
                let mut new_x = *start_x;
                let mut new_y = *start_y;

                // Apply resize based on edges
                if edges & 4 != 0 {
                    // Left edge
                    new_width = (*start_width - dx).max(self.min_width);
                    new_x = *start_x + (*start_width - new_width);
                }
                if edges & 8 != 0 {
                    // Right edge
                    new_width = (*start_width + dx).max(self.min_width);
                }
                if edges & 1 != 0 {
                    // Top edge
                    new_height = (*start_height - dy).max(self.min_height);
                    new_y = *start_y + (*start_height - new_height);
                }
                if edges & 2 != 0 {
                    // Bottom edge
                    new_height = (*start_height + dy).max(self.min_height);
                }

                if new_x != self.x || new_y != self.y {
                    self.set_position(new_x, new_y);
                }
                self.propose_dimensions(new_width, new_height);
            }
            Operator::None => {}
        }
    }
}

impl std::fmt::Debug for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Window")
            .field("id", &self.id)
            .field("app_id", &self.app_id)
            .field("title", &self.title)
            .field("x", &self.x)
            .field("y", &self.y)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("floating", &self.floating)
            .field("hidden", &self.hidden)
            .finish()
    }
}
