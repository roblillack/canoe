//! `ext-image-capture-source-v1` (output and foreign-toplevel sources) and
//! `ext-image-copy-capture-v1`, shm buffers only.
//!
//! The compositor creates an [`ImageCopyCaptureState`] (which registers the
//! `ext_output_image_capture_source_manager_v1`,
//! `ext_foreign_toplevel_image_capture_source_manager_v1` and
//! `ext_image_copy_capture_manager_v1` globals), implements
//! [`ImageCopyCaptureHandler`] and calls
//! [`delegate_image_copy_capture!`].
//! Toplevel sources are created from smithay's
//! [`foreign_toplevel_list`](smithay::wayland::foreign_toplevel_list) handles,
//! so that protocol must be delegated as well.
//!
//! When a client captures a frame, the handler receives an owned
//! [`ImageCopyFrame`] describing what to render and into which buffer. The
//! compositor renders it (e.g. with
//! [`ShmCaptureRenderer::render_to_shm`](super::capture_render::ShmCaptureRenderer::render_to_shm))
//! and completes it with [`ImageCopyFrame::success`] or
//! [`ImageCopyFrame::fail`]. Dropping an uncompleted frame fails it.
//!
//! The first frame of a session is handed out right away; later frames only
//! once the source got damaged. So the compositor must:
//! - call [`ImageCopyCaptureState::damage_output`] after every rendered frame
//!   of an output, and [`ImageCopyCaptureState::damage_toplevel`] whenever a
//!   toplevel's content changed (e.g. on commits of its surfaces; passing the
//!   whole toplevel as damage is fine). These also pick up size changes and
//!   re-send the buffer constraints.
//! - call [`ImageCopyCaptureState::output_removed`] /
//!   [`ImageCopyCaptureState::toplevel_closed`] when a source goes away, which
//!   sends `stopped` to its sessions.
//! - implement [`ImageCopyCaptureHandler::capture_source_size`] for toplevels.
//!
//! Cursor sessions are supported only minimally: they advertise a 64x64
//! buffer, never report the cursor as entered, deliver one fully transparent
//! frame and then never deliver another one.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use smithay::{
    output::{Output, WeakOutput},
    reexports::{
        wayland_protocols::ext::{
            image_capture_source::v1::server::{
                ext_foreign_toplevel_image_capture_source_manager_v1::{
                    self, ExtForeignToplevelImageCaptureSourceManagerV1,
                },
                ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1},
                ext_output_image_capture_source_manager_v1::{
                    self, ExtOutputImageCaptureSourceManagerV1,
                },
            },
            image_copy_capture::v1::server::{
                ext_image_copy_capture_cursor_session_v1::{
                    self, ExtImageCopyCaptureCursorSessionV1,
                },
                ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
                ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
                ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
            },
        },
        wayland_server::{
            backend::{ClientId, GlobalId},
            protocol::{wl_buffer::WlBuffer, wl_shm},
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
        },
    },
    utils::{Buffer, Physical, Rectangle, Size, Transform},
    wayland::{
        foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelWeakHandle},
        shm,
    },
};

pub use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason;

/// More damage rectangles than this are collapsed into full damage.
const MAX_DAMAGE_RECTS: usize = 32;
/// Buffer size advertised for cursor sessions.
const CURSOR_SIZE: i32 = 64;
const SHM_FORMATS: [wl_shm::Format; 2] = [wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888];

/// Something that can be captured.
#[derive(Debug, Clone)]
pub enum ImageCaptureSource {
    /// A whole output.
    Output(Output),
    /// A toplevel window.
    Toplevel(ForeignToplevelHandle),
}

/// The buffer size (in buffer pixels) of a capture of the whole `output`:
/// its current mode size. `None` if it has no mode.
pub fn output_capture_size(output: &Output) -> Option<Size<i32, Buffer>> {
    output
        .current_mode()
        .map(|mode| (mode.size.w, mode.size.h).into())
}

/// Handler trait for ext-image-copy-capture.
pub trait ImageCopyCaptureHandler:
    GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, ImageCopyCaptureGlobalData>
    + GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ImageCopyCaptureGlobalData>
    + GlobalDispatch<ExtImageCopyCaptureManagerV1, ImageCopyCaptureGlobalData>
    + Dispatch<ExtOutputImageCaptureSourceManagerV1, ()>
    + Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()>
    + Dispatch<ExtImageCaptureSourceV1, ImageCaptureSourceData>
    + Dispatch<ExtImageCopyCaptureManagerV1, ()>
    + Dispatch<ExtImageCopyCaptureSessionV1, ImageCopySessionData>
    + Dispatch<ExtImageCopyCaptureCursorSessionV1, ImageCopyCursorSessionData>
    + Dispatch<ExtImageCopyCaptureFrameV1, ImageCopyFrameData>
    + 'static
{
    /// [`ImageCopyCaptureState`] getter.
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState;

    /// The current size, in buffer pixels, of captures of `source`, or `None`
    /// if it cannot be captured (anymore), which stops the session.
    ///
    /// For toplevels this is the size the compositor will render the
    /// toplevel at (e.g. its window geometry times the output scale). The
    /// default implementation handles outputs only.
    fn capture_source_size(&mut self, source: &ImageCaptureSource) -> Option<Size<i32, Buffer>> {
        match source {
            ImageCaptureSource::Output(output) => output_capture_size(output),
            ImageCaptureSource::Toplevel(_) => None,
        }
    }

    /// A client wants `frame` to be rendered. The compositor should render it
    /// at the next opportunity (scheduling a repaint if needed), then call
    /// [`ImageCopyFrame::success`], or [`ImageCopyFrame::fail`]/drop it.
    fn image_copy_frame(&mut self, frame: ImageCopyFrame);
}

/// State of the image capture source and image copy capture globals.
#[derive(Debug)]
pub struct ImageCopyCaptureState {
    output_source_global: GlobalId,
    toplevel_source_global: GlobalId,
    capture_global: GlobalId,
    sessions: Vec<SessionEntry>,
    /// Captured frames waiting for damage.
    pending: Vec<ImageCopyFrame>,
}

#[derive(Debug)]
struct SessionEntry {
    resource: ExtImageCopyCaptureSessionV1,
    inner: Arc<Mutex<SessionInner>>,
}

impl ImageCopyCaptureState {
    /// Creates the globals, visible to all clients.
    pub fn new<D: ImageCopyCaptureHandler>(dh: &DisplayHandle) -> Self {
        Self::new_with_filter::<D, _>(dh, |_| true)
    }

    /// Creates the globals, visible to clients for which `filter` returns true.
    pub fn new_with_filter<D, F>(dh: &DisplayHandle, filter: F) -> Self
    where
        D: ImageCopyCaptureHandler,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let filter: Arc<dyn Fn(&Client) -> bool + Send + Sync> = Arc::new(filter);
        let data = || ImageCopyCaptureGlobalData {
            filter: filter.clone(),
        };
        Self {
            output_source_global: dh
                .create_global::<D, ExtOutputImageCaptureSourceManagerV1, _>(1, data()),
            toplevel_source_global: dh
                .create_global::<D, ExtForeignToplevelImageCaptureSourceManagerV1, _>(1, data()),
            capture_global: dh.create_global::<D, ExtImageCopyCaptureManagerV1, _>(1, data()),
            sessions: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// The globals: output source manager, toplevel source manager, copy capture manager.
    pub fn globals(&self) -> [GlobalId; 3] {
        [
            self.output_source_global.clone(),
            self.toplevel_source_global.clone(),
            self.capture_global.clone(),
        ]
    }

    /// Reports the damage of a frame the compositor just rendered for `output`.
    ///
    /// `damage` is in output-local physical coordinates in the orientation of
    /// the logical (untransformed) output, i.e. what
    /// [`OutputDamageTracker::render_output`](smithay::backend::renderer::damage::OutputDamageTracker::render_output)
    /// returns. Re-sends buffer constraints if the output size changed and
    /// hands frames that were waiting for damage to the handler.
    pub fn damage_output<D: ImageCopyCaptureHandler>(
        state: &mut D,
        output: &Output,
        damage: &[Rectangle<i32, Physical>],
    ) {
        let source = ImageCaptureSource::Output(output.clone());
        damage_source(state, &source, damage);
    }

    /// Reports that the content of `toplevel` changed.
    ///
    /// `damage` is in physical coordinates relative to the top-left corner of
    /// the captured toplevel image. Re-sends buffer constraints if
    /// [`ImageCopyCaptureHandler::capture_source_size`] changed and hands
    /// frames that were waiting for damage to the handler.
    pub fn damage_toplevel<D: ImageCopyCaptureHandler>(
        state: &mut D,
        toplevel: &ForeignToplevelHandle,
        damage: &[Rectangle<i32, Physical>],
    ) {
        let source = ImageCaptureSource::Toplevel(toplevel.clone());
        damage_source(state, &source, damage);
    }

    /// Stops all sessions capturing `output`.
    pub fn output_removed(&mut self, output: &Output) {
        self.stop_sessions(|source| source.is_output(output));
    }

    /// Stops all sessions capturing `toplevel`.
    pub fn toplevel_closed(&mut self, toplevel: &ForeignToplevelHandle) {
        self.stop_sessions(|source| source.is_toplevel(toplevel));
    }

    fn cleanup(&mut self) {
        // A destroyed session is kept as long as one of its frames is alive.
        self.sessions
            .retain(|entry| entry.resource.is_alive() || Arc::strong_count(&entry.inner) > 1);
    }

    fn stop_sessions(&mut self, matches: impl Fn(&WeakSource) -> bool) {
        self.cleanup();
        for entry in &self.sessions {
            let mut inner = entry.inner.lock().unwrap();
            if !inner.stopped && matches(&inner.source) {
                stop_session(&entry.resource, &mut inner);
            }
        }
        self.fail_stopped_frames();
    }

    /// Fails pending frames of stopped sessions.
    fn fail_stopped_frames(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].session.lock().unwrap().stopped {
                self.pending.remove(i).fail(FailureReason::Stopped);
            } else {
                i += 1;
            }
        }
    }
}

fn stop_session(resource: &ExtImageCopyCaptureSessionV1, inner: &mut SessionInner) {
    inner.stopped = true;
    if resource.is_alive() {
        resource.stopped();
    }
}

fn send_constraints(resource: &ExtImageCopyCaptureSessionV1, size: Size<i32, Buffer>) {
    for format in SHM_FORMATS {
        resource.shm_format(format);
    }
    resource.buffer_size(size.w as u32, size.h as u32);
    resource.done();
}

fn damage_source<D: ImageCopyCaptureHandler>(
    state: &mut D,
    source: &ImageCaptureSource,
    damage: &[Rectangle<i32, Physical>],
) {
    let this = state.image_copy_capture_state();
    this.cleanup();
    let captured = this
        .sessions
        .iter()
        .any(|entry| entry.inner.lock().unwrap().source.matches(source));
    if !captured {
        return;
    }
    let size = state.capture_source_size(source);
    let this = state.image_copy_capture_state();

    for entry in &this.sessions {
        let mut inner = entry.inner.lock().unwrap();
        if inner.stopped || inner.cursor || !inner.source.matches(source) {
            continue;
        }
        match size {
            None => stop_session(&entry.resource, &mut inner),
            Some(size) => {
                if inner.size != Some(size) {
                    inner.size = Some(size);
                    inner.damage = Damage::Full;
                    if entry.resource.is_alive() {
                        send_constraints(&entry.resource, size);
                    }
                }
                inner.damage.add(damage);
            }
        }
    }
    this.fail_stopped_frames();

    let mut ready = Vec::new();
    let mut i = 0;
    while i < this.pending.len() {
        let frame = &mut this.pending[i];
        let (matches, size) = {
            let inner = frame.session.lock().unwrap();
            (!inner.cursor && inner.source.matches(source), inner.size)
        };
        if !matches {
            i += 1;
        } else if size != Some(frame.buffer_size) {
            this.pending
                .remove(i)
                .fail(FailureReason::BufferConstraints);
        } else if frame.take_session_damage() {
            ready.push(this.pending.remove(i));
        } else {
            i += 1;
        }
    }
    for frame in ready {
        state.image_copy_frame(frame);
    }
}

/// Global data of the image capture globals.
#[derive(Clone)]
pub struct ImageCopyCaptureGlobalData {
    filter: Arc<dyn Fn(&Client) -> bool + Send + Sync>,
}

impl std::fmt::Debug for ImageCopyCaptureGlobalData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageCopyCaptureGlobalData")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
enum WeakSource {
    Output(WeakOutput),
    Toplevel(ForeignToplevelWeakHandle),
    /// Created from an inert output or toplevel handle.
    Invalid,
}

impl WeakSource {
    fn upgrade(&self) -> Option<ImageCaptureSource> {
        match self {
            WeakSource::Output(output) => output.upgrade().map(ImageCaptureSource::Output),
            WeakSource::Toplevel(toplevel) => toplevel
                .upgrade()
                .filter(|toplevel| !toplevel.is_closed())
                .map(ImageCaptureSource::Toplevel),
            WeakSource::Invalid => None,
        }
    }

    fn is_output(&self, output: &Output) -> bool {
        matches!(self, WeakSource::Output(weak) if weak == output)
    }

    fn is_toplevel(&self, toplevel: &ForeignToplevelHandle) -> bool {
        match self {
            WeakSource::Toplevel(weak) => weak
                .upgrade()
                .is_some_and(|handle| handle.identifier() == toplevel.identifier()),
            _ => false,
        }
    }

    fn matches(&self, source: &ImageCaptureSource) -> bool {
        match source {
            ImageCaptureSource::Output(output) => self.is_output(output),
            ImageCaptureSource::Toplevel(toplevel) => self.is_toplevel(toplevel),
        }
    }
}

/// User data of `ext_image_capture_source_v1` objects.
#[derive(Debug)]
pub struct ImageCaptureSourceData {
    source: WeakSource,
}

impl ImageCaptureSourceData {
    /// The source, if it still exists.
    pub fn source(&self) -> Option<ImageCaptureSource> {
        self.source.upgrade()
    }
}

/// User data of `ext_image_copy_capture_session_v1` objects.
#[derive(Debug)]
pub struct ImageCopySessionData {
    inner: Arc<Mutex<SessionInner>>,
}

#[derive(Debug)]
struct SessionInner {
    source: WeakSource,
    paint_cursors: bool,
    /// Created through a cursor session.
    cursor: bool,
    /// Last advertised buffer size.
    size: Option<Size<i32, Buffer>>,
    stopped: bool,
    /// A frame object exists.
    has_frame: bool,
    /// Damage since the last delivered frame, in physical capture-space coordinates.
    damage: Damage,
}

/// User data of `ext_image_copy_capture_cursor_session_v1` objects.
#[derive(Debug)]
pub struct ImageCopyCursorSessionData {
    source: WeakSource,
    has_session: AtomicBool,
}

/// User data of `ext_image_copy_capture_frame_v1` objects.
#[derive(Debug)]
pub struct ImageCopyFrameData {
    inner: Mutex<FrameInner>,
}

#[derive(Debug)]
struct FrameInner {
    session: Arc<Mutex<SessionInner>>,
    session_resource: ExtImageCopyCaptureSessionV1,
    buffer: Option<WlBuffer>,
    buffer_damage: Vec<Rectangle<i32, Buffer>>,
    captured: bool,
}

/// A frame to be rendered for an image copy capture client.
///
/// Render [`source`](Self::source) into [`buffer`](Self::buffer), applying
/// [`transform`](Self::transform), then call [`success`](Self::success).
/// Dropping the frame without completing it sends `failed`.
#[derive(Debug)]
pub struct ImageCopyFrame {
    frame: ExtImageCopyCaptureFrameV1,
    session: Arc<Mutex<SessionInner>>,
    source: ImageCaptureSource,
    buffer: WlBuffer,
    format: wl_shm::Format,
    buffer_size: Size<i32, Buffer>,
    transform: Transform,
    paint_cursors: bool,
    damage: Vec<Rectangle<i32, Buffer>>,
    buffer_damage: Vec<Rectangle<i32, Buffer>>,
    completed: bool,
}

impl ImageCopyFrame {
    /// What to capture.
    pub fn source(&self) -> &ImageCaptureSource {
        &self.source
    }

    /// The client's buffer to render into (a `wl_shm` buffer of
    /// [`buffer_size`](Self::buffer_size) in [`format`](Self::format)).
    pub fn buffer(&self) -> &WlBuffer {
        &self.buffer
    }

    /// The shm format of [`buffer`](Self::buffer) (ARGB8888 or XRGB8888).
    pub fn format(&self) -> wl_shm::Format {
        self.format
    }

    /// Size of [`buffer`](Self::buffer).
    pub fn buffer_size(&self) -> Size<i32, Buffer> {
        self.buffer_size
    }

    /// The transform to apply when rendering: the output transform for
    /// outputs, `Normal` for toplevels. Reported to the client.
    pub fn transform(&self) -> Transform {
        self.transform
    }

    /// The captured area in physical capture-space coordinates (the
    /// untransformed source, origin at its top-left corner): position render
    /// elements relative to it.
    pub fn region(&self) -> Rectangle<i32, Physical> {
        let size = self.transform.transform_size(self.buffer_size);
        Rectangle::from_size((size.w, size.h).into())
    }

    /// Whether cursors should be painted onto the frame.
    pub fn paint_cursors(&self) -> bool {
        self.paint_cursors
    }

    /// Damage that will be reported by [`success`](Self::success), in buffer coordinates.
    pub fn damage(&self) -> &[Rectangle<i32, Buffer>] {
        &self.damage
    }

    /// Regions of the buffer the client says changed since it was last
    /// captured into. The compositor must at least redraw these plus
    /// [`damage`](Self::damage) (redrawing everything is always fine).
    pub fn buffer_damage(&self) -> &[Rectangle<i32, Buffer>] {
        &self.buffer_damage
    }

    /// Whether the client still has the frame object.
    pub fn is_alive(&self) -> bool {
        self.frame.is_alive()
    }

    /// Completes the frame: the buffer has been filled. Sends `transform`,
    /// the tracked damage, `presentation_time` and `ready`.
    ///
    /// `presentation_time` is a `CLOCK_MONOTONIC` timestamp.
    pub fn success(mut self, presentation_time: Duration) {
        let damage = std::mem::take(&mut self.damage);
        self.complete(&damage, presentation_time);
    }

    /// Like [`success`](Self::success), but reports the given damage (buffer
    /// coordinates) instead of the tracked one.
    pub fn success_with_damage(
        mut self,
        damage: &[Rectangle<i32, Buffer>],
        presentation_time: Duration,
    ) {
        self.complete(damage, presentation_time);
    }

    /// Fails the frame with `reason`. Dropping the frame fails it with `Unknown`.
    pub fn fail(mut self, reason: FailureReason) {
        self.fail_internal(reason);
    }

    fn complete(&mut self, damage: &[Rectangle<i32, Buffer>], presentation_time: Duration) {
        self.completed = true;
        if !self.frame.is_alive() {
            return;
        }
        self.frame.transform(self.transform.into());
        for rect in damage {
            self.frame
                .damage(rect.loc.x, rect.loc.y, rect.size.w, rect.size.h);
        }
        send_presentation_time(&self.frame, presentation_time);
        self.frame.ready();
    }

    fn fail_internal(&mut self, reason: FailureReason) {
        if self.completed {
            return;
        }
        self.completed = true;
        // The client did not get this damage, so the next frame has to cover everything.
        self.session.lock().unwrap().damage = Damage::Full;
        if self.frame.is_alive() {
            self.frame.failed(reason);
        }
    }

    /// Takes the session's accumulated damage, if any. Returns whether there was any.
    fn take_session_damage(&mut self) -> bool {
        let mut session = self.session.lock().unwrap();
        let damage = session.damage.to_buffer(self.region(), self.transform);
        if damage.is_empty() {
            return false;
        }
        session.damage.clear();
        self.damage = damage;
        true
    }
}

impl Drop for ImageCopyFrame {
    fn drop(&mut self) {
        self.fail_internal(FailureReason::Unknown);
    }
}

fn send_presentation_time(frame: &ExtImageCopyCaptureFrameV1, time: Duration) {
    let secs = time.as_secs();
    frame.presentation_time((secs >> 32) as u32, secs as u32, time.subsec_nanos());
}

fn monotonic_now() -> Duration {
    smithay::utils::Clock::<smithay::utils::Monotonic>::new()
        .now()
        .into()
}

/// Damage accumulated since the last delivered frame, in physical capture-space coordinates.
#[derive(Debug, Clone)]
enum Damage {
    Full,
    Rects(Vec<Rectangle<i32, Physical>>),
}

impl Damage {
    fn add(&mut self, rects: &[Rectangle<i32, Physical>]) {
        if let Damage::Rects(current) = self {
            current.extend(rects.iter().filter(|r| !r.is_empty()));
            if current.len() > MAX_DAMAGE_RECTS {
                *self = Damage::Full;
            }
        }
    }

    fn clear(&mut self) {
        *self = Damage::Rects(Vec::new());
    }

    /// The damage within `region`, relative to it, in buffer coordinates.
    fn to_buffer(
        &self,
        region: Rectangle<i32, Physical>,
        transform: Transform,
    ) -> Vec<Rectangle<i32, Buffer>> {
        let rects = match self {
            Damage::Full => vec![Rectangle::from_size(region.size)],
            Damage::Rects(rects) => rects
                .iter()
                .filter_map(|rect| rect.intersection(region))
                .map(|mut rect| {
                    rect.loc -= region.loc;
                    rect
                })
                .collect(),
        };
        rects
            .into_iter()
            .map(|rect| {
                let rect = transform.invert().transform_rect_in(rect, &region.size);
                Rectangle::new(
                    (rect.loc.x, rect.loc.y).into(),
                    (rect.size.w, rect.size.h).into(),
                )
            })
            .collect()
    }
}

/// Validates a client buffer against the session's constraints, returning its format.
fn check_buffer(buffer: &WlBuffer, size: Size<i32, Buffer>) -> Option<wl_shm::Format> {
    let data = shm::with_buffer_contents(buffer, |_, _, data| data).ok()?;
    (SHM_FORMATS.contains(&data.format)
        && data.width == size.w
        && data.height == size.h
        && data.stride >= data.width * 4)
        .then_some(data.format)
}

/// Fills a cursor frame's buffer with transparent pixels.
fn clear_buffer(buffer: &WlBuffer) -> bool {
    shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
        let end = data.offset as usize + data.stride as usize * data.height as usize;
        if data.offset < 0 || data.stride <= 0 || end > len {
            return false;
        }
        // SAFETY: the range was checked to lie within the pool, which stays
        // mapped while this closure runs.
        unsafe {
            std::ptr::write_bytes(ptr.add(data.offset as usize), 0, end - data.offset as usize)
        };
        true
    })
    .unwrap_or(false)
}

macro_rules! global_dispatch {
    ($interface:ty) => {
        impl<D: ImageCopyCaptureHandler> GlobalDispatch<$interface, ImageCopyCaptureGlobalData, D>
            for ImageCopyCaptureState
        {
            fn bind(
                _state: &mut D,
                _dh: &DisplayHandle,
                _client: &Client,
                resource: New<$interface>,
                _global_data: &ImageCopyCaptureGlobalData,
                data_init: &mut DataInit<'_, D>,
            ) {
                data_init.init(resource, ());
            }

            fn can_view(client: Client, global_data: &ImageCopyCaptureGlobalData) -> bool {
                (global_data.filter)(&client)
            }
        }
    };
}

global_dispatch!(ExtOutputImageCaptureSourceManagerV1);
global_dispatch!(ExtForeignToplevelImageCaptureSourceManagerV1);
global_dispatch!(ExtImageCopyCaptureManagerV1);

impl<D: ImageCopyCaptureHandler> Dispatch<ExtOutputImageCaptureSourceManagerV1, (), D>
    for ImageCopyCaptureState
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _manager: &ExtOutputImageCaptureSourceManagerV1,
        request: ext_output_image_capture_source_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_output_image_capture_source_manager_v1::Request::CreateSource {
                source,
                output,
            } => {
                let source_ref = Output::from_resource(&output)
                    .map(|output| WeakSource::Output(output.downgrade()))
                    .unwrap_or(WeakSource::Invalid);
                data_init.init(source, ImageCaptureSourceData { source: source_ref });
            }
            ext_output_image_capture_source_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D: ImageCopyCaptureHandler> Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, (), D>
    for ImageCopyCaptureState
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _manager: &ExtForeignToplevelImageCaptureSourceManagerV1,
        request: ext_foreign_toplevel_image_capture_source_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_foreign_toplevel_image_capture_source_manager_v1::Request::CreateSource {
                source,
                toplevel_handle,
            } => {
                let source_ref = ForeignToplevelHandle::from_resource(&toplevel_handle)
                    .filter(|handle| !handle.is_closed())
                    .map(|handle| WeakSource::Toplevel(handle.downgrade()))
                    .unwrap_or(WeakSource::Invalid);
                data_init.init(source, ImageCaptureSourceData { source: source_ref });
            }
            ext_foreign_toplevel_image_capture_source_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D: ImageCopyCaptureHandler> Dispatch<ExtImageCaptureSourceV1, ImageCaptureSourceData, D>
    for ImageCopyCaptureState
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _source: &ExtImageCaptureSourceV1,
        request: ext_image_capture_source_v1::Request,
        _data: &ImageCaptureSourceData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_image_capture_source_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D: ImageCopyCaptureHandler> Dispatch<ExtImageCopyCaptureManagerV1, (), D>
    for ImageCopyCaptureState
{
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &ExtImageCopyCaptureManagerV1,
        request: ext_image_copy_capture_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_image_copy_capture_manager_v1::Request::CreateSession {
                session,
                source,
                options,
            } => {
                let options = match options {
                    WEnum::Value(options) => options,
                    WEnum::Unknown(_) => {
                        manager.post_error(
                            ext_image_copy_capture_manager_v1::Error::InvalidOption,
                            "invalid options",
                        );
                        return;
                    }
                };
                let source = source
                    .data::<ImageCaptureSourceData>()
                    .map(|data| data.source.clone())
                    .unwrap_or(WeakSource::Invalid);
                let paint_cursors =
                    options.contains(ext_image_copy_capture_manager_v1::Options::PaintCursors);
                create_session(state, session, source, paint_cursors, false, data_init);
            }
            ext_image_copy_capture_manager_v1::Request::CreatePointerCursorSession {
                session,
                source,
                pointer: _,
            } => {
                let source = source
                    .data::<ImageCaptureSourceData>()
                    .map(|data| data.source.clone())
                    .unwrap_or(WeakSource::Invalid);
                data_init.init(
                    session,
                    ImageCopyCursorSessionData {
                        source,
                        has_session: AtomicBool::new(false),
                    },
                );
            }
            ext_image_copy_capture_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

fn create_session<D: ImageCopyCaptureHandler>(
    state: &mut D,
    session: New<ExtImageCopyCaptureSessionV1>,
    source: WeakSource,
    paint_cursors: bool,
    cursor: bool,
    data_init: &mut DataInit<'_, D>,
) {
    let size = if cursor {
        source
            .upgrade()
            .map(|_| Size::from((CURSOR_SIZE, CURSOR_SIZE)))
    } else {
        source
            .upgrade()
            .and_then(|source| state.capture_source_size(&source))
    };
    let inner = Arc::new(Mutex::new(SessionInner {
        source,
        paint_cursors,
        cursor,
        size,
        stopped: size.is_none(),
        has_frame: false,
        damage: Damage::Full,
    }));
    let resource = data_init.init(
        session,
        ImageCopySessionData {
            inner: inner.clone(),
        },
    );
    match size {
        Some(size) => send_constraints(&resource, size),
        None => resource.stopped(),
    }
    let this = state.image_copy_capture_state();
    this.cleanup();
    this.sessions.push(SessionEntry { resource, inner });
}

impl<D: ImageCopyCaptureHandler>
    Dispatch<ExtImageCopyCaptureCursorSessionV1, ImageCopyCursorSessionData, D>
    for ImageCopyCaptureState
{
    fn request(
        state: &mut D,
        _client: &Client,
        cursor_session: &ExtImageCopyCaptureCursorSessionV1,
        request: ext_image_copy_capture_cursor_session_v1::Request,
        data: &ImageCopyCursorSessionData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_image_copy_capture_cursor_session_v1::Request::GetCaptureSession { session } => {
                if data.has_session.swap(true, Ordering::Relaxed) {
                    cursor_session.post_error(
                        ext_image_copy_capture_cursor_session_v1::Error::DuplicateSession,
                        "get_capture_session sent twice",
                    );
                    return;
                }
                create_session(state, session, data.source.clone(), false, true, data_init);
            }
            ext_image_copy_capture_cursor_session_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D: ImageCopyCaptureHandler> Dispatch<ExtImageCopyCaptureSessionV1, ImageCopySessionData, D>
    for ImageCopyCaptureState
{
    fn request(
        _state: &mut D,
        _client: &Client,
        session: &ExtImageCopyCaptureSessionV1,
        request: ext_image_copy_capture_session_v1::Request,
        data: &ImageCopySessionData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_image_copy_capture_session_v1::Request::CreateFrame { frame } => {
                {
                    let mut inner = data.inner.lock().unwrap();
                    if inner.has_frame {
                        session.post_error(
                            ext_image_copy_capture_session_v1::Error::DuplicateFrame,
                            "create_frame sent before destroying the previous frame",
                        );
                        return;
                    }
                    inner.has_frame = true;
                }
                data_init.init(
                    frame,
                    ImageCopyFrameData {
                        inner: Mutex::new(FrameInner {
                            session: data.inner.clone(),
                            session_resource: session.clone(),
                            buffer: None,
                            buffer_damage: Vec::new(),
                            captured: false,
                        }),
                    },
                );
            }
            ext_image_copy_capture_session_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D: ImageCopyCaptureHandler> Dispatch<ExtImageCopyCaptureFrameV1, ImageCopyFrameData, D>
    for ImageCopyCaptureState
{
    fn request(
        state: &mut D,
        _client: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        data: &ImageCopyFrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let mut inner = data.inner.lock().unwrap();
        match request {
            ext_image_copy_capture_frame_v1::Request::AttachBuffer { buffer } => {
                if inner.captured {
                    frame.post_error(
                        ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
                        "attach_buffer sent after capture",
                    );
                    return;
                }
                inner.buffer = Some(buffer);
            }
            ext_image_copy_capture_frame_v1::Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if inner.captured {
                    frame.post_error(
                        ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
                        "damage_buffer sent after capture",
                    );
                    return;
                }
                if x < 0 || y < 0 || width <= 0 || height <= 0 {
                    frame.post_error(
                        ext_image_copy_capture_frame_v1::Error::InvalidBufferDamage,
                        "invalid buffer damage",
                    );
                    return;
                }
                inner
                    .buffer_damage
                    .push(Rectangle::new((x, y).into(), (width, height).into()));
            }
            ext_image_copy_capture_frame_v1::Request::Capture => {
                if inner.captured {
                    frame.post_error(
                        ext_image_copy_capture_frame_v1::Error::AlreadyCaptured,
                        "capture sent twice",
                    );
                    return;
                }
                let Some(buffer) = inner.buffer.clone() else {
                    frame.post_error(
                        ext_image_copy_capture_frame_v1::Error::NoBuffer,
                        "capture sent without attach_buffer",
                    );
                    return;
                };
                inner.captured = true;
                let session = inner.session.clone();
                let session_resource = inner.session_resource.clone();
                let buffer_damage = std::mem::take(&mut inner.buffer_damage);
                drop(inner);
                capture(
                    state,
                    frame,
                    session,
                    &session_resource,
                    buffer,
                    buffer_damage,
                );
            }
            ext_image_copy_capture_frame_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        frame: &ExtImageCopyCaptureFrameV1,
        data: &ImageCopyFrameData,
    ) {
        data.inner.lock().unwrap().session.lock().unwrap().has_frame = false;
        state
            .image_copy_capture_state()
            .pending
            .retain(|pending| pending.frame != *frame);
    }
}

fn capture<D: ImageCopyCaptureHandler>(
    state: &mut D,
    frame: &ExtImageCopyCaptureFrameV1,
    session: Arc<Mutex<SessionInner>>,
    session_resource: &ExtImageCopyCaptureSessionV1,
    buffer: WlBuffer,
    buffer_damage: Vec<Rectangle<i32, Buffer>>,
) {
    let (source, cursor, paint_cursors, advertised) = {
        let mut inner = session.lock().unwrap();
        if inner.stopped {
            frame.failed(FailureReason::Stopped);
            return;
        }
        let Some(source) = inner.source.upgrade() else {
            stop_session(session_resource, &mut inner);
            frame.failed(FailureReason::Stopped);
            return;
        };
        (source, inner.cursor, inner.paint_cursors, inner.size)
    };

    let size = if cursor {
        advertised
    } else {
        let size = state.capture_source_size(&source);
        let mut inner = session.lock().unwrap();
        match size {
            None => {
                stop_session(session_resource, &mut inner);
                frame.failed(FailureReason::Stopped);
                return;
            }
            Some(size) if inner.size != Some(size) => {
                inner.size = Some(size);
                inner.damage = Damage::Full;
                send_constraints(session_resource, size);
            }
            Some(_) => {}
        }
        size
    };
    let Some(size) = size else {
        frame.failed(FailureReason::Stopped);
        return;
    };
    let Some(format) = check_buffer(&buffer, size) else {
        frame.failed(FailureReason::BufferConstraints);
        return;
    };

    let transform = match &source {
        ImageCaptureSource::Output(output) if !cursor => output.current_transform(),
        _ => Transform::Normal,
    };
    let mut capture = ImageCopyFrame {
        frame: frame.clone(),
        session,
        source,
        buffer,
        format,
        buffer_size: size,
        transform,
        paint_cursors,
        damage: Vec::new(),
        buffer_damage,
        completed: false,
    };

    if cursor {
        // Minimal cursor capture: the cursor is never reported as entered, the
        // first frame is transparent and later ones never complete.
        if capture.take_session_damage() {
            if clear_buffer(&capture.buffer) {
                capture.success(monotonic_now());
            } else {
                capture.fail(FailureReason::BufferConstraints);
            }
        } else {
            state.image_copy_capture_state().pending.push(capture);
        }
        return;
    }

    if capture.take_session_damage() {
        state.image_copy_frame(capture);
    } else {
        state.image_copy_capture_state().pending.push(capture);
    }
}

/// Delegates the image capture source and image copy capture protocols to
/// [`ImageCopyCaptureState`].
///
/// The type must implement [`ImageCopyCaptureHandler`].
macro_rules! delegate_image_copy_capture {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        ::smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1: $crate::compositor::protocols::image_capture::ImageCopyCaptureGlobalData
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
        ::smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1: $crate::compositor::protocols::image_capture::ImageCopyCaptureGlobalData
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
        ::smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1: $crate::compositor::protocols::image_capture::ImageCopyCaptureGlobalData
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
        ::smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1: ()
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
        ::smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1: ()
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
        ::smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_image_capture_source_v1::ExtImageCaptureSourceV1: $crate::compositor::protocols::image_capture::ImageCaptureSourceData
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
        ::smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1: ()
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
        ::smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1: $crate::compositor::protocols::image_capture::ImageCopySessionData
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
        ::smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_cursor_session_v1::ExtImageCopyCaptureCursorSessionV1: $crate::compositor::protocols::image_capture::ImageCopyCursorSessionData
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
        ::smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1: $crate::compositor::protocols::image_capture::ImageCopyFrameData
        ] => $crate::compositor::protocols::image_capture::ImageCopyCaptureState);
    };
}
pub(crate) use delegate_image_copy_capture;
