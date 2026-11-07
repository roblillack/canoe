//! `wlr-screencopy-unstable-v1` (version 3), shm buffers only.
//!
//! The compositor creates a [`ScreencopyManagerState`], implements
//! [`ScreencopyHandler`] and calls [`delegate_screencopy!`].
//! Whenever a client asks for a frame to be copied, the handler receives an
//! owned [`ScreencopyFrame`] describing what to render and into which buffer.
//! The compositor renders it (e.g. with
//! [`ShmCaptureRenderer::render_to_shm`](super::capture_render::ShmCaptureRenderer::render_to_shm))
//! and completes it with [`ScreencopyFrame::success`] or
//! [`ScreencopyFrame::fail`]. Dropping an uncompleted frame fails it.
//!
//! The compositor must additionally:
//! - call [`ScreencopyManagerState::damage_output`] after every rendered frame
//!   of an output, with the damage of that frame. `copy_with_damage` requests
//!   are only handed to the handler once their output got damaged since the
//!   client's previous frame (like wlroots does).
//! - call [`ScreencopyManagerState::output_removed`] when an output goes away.
//!
//! ```ignore
//! impl ScreencopyHandler for State {
//!     fn screencopy_state(&mut self) -> &mut ScreencopyManagerState { &mut self.screencopy }
//!     fn screencopy_frame(&mut self, frame: ScreencopyFrame) {
//!         // Keep it until the next render of frame.output(), and make sure one happens.
//!         self.schedule_render(frame.output());
//!         self.pending_screencopies.push(frame);
//!     }
//! }
//! delegate_screencopy!(State);
//! ```

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use smithay::{
    output::{Output, WeakOutput},
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            backend::{ClientId, GlobalId},
            protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
        },
    },
    utils::{Buffer, Logical, Physical, Rectangle, Size, Transform},
    wayland::shm,
};

const MANAGER_VERSION: u32 = 3;
/// More damage rectangles than this are collapsed into full damage.
const MAX_DAMAGE_RECTS: usize = 32;
/// The format announced in the `buffer` event. ARGB8888 buffers are accepted as well.
const ADVERTISED_FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

/// Handler trait for wlr-screencopy.
pub trait ScreencopyHandler:
    GlobalDispatch<ZwlrScreencopyManagerV1, ScreencopyGlobalData>
    + Dispatch<ZwlrScreencopyManagerV1, ()>
    + Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameData>
    + 'static
{
    /// [`ScreencopyManagerState`] getter.
    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState;

    /// A client wants `frame` to be rendered. The compositor should render it
    /// with its next repaint of [`ScreencopyFrame::output`] (and schedule one
    /// if none is pending), then call [`ScreencopyFrame::success`], or
    /// [`ScreencopyFrame::fail`]/drop it if that is not possible.
    fn screencopy_frame(&mut self, frame: ScreencopyFrame);
}

/// State of the `zwlr_screencopy_manager_v1` global.
#[derive(Debug)]
pub struct ScreencopyManagerState {
    global: GlobalId,
    /// Accumulated damage per (manager instance, output), for `copy_with_damage`.
    trackers: Vec<DamageTracker>,
    /// `copy_with_damage` frames waiting for damage.
    pending: Vec<ScreencopyFrame>,
}

#[derive(Debug)]
struct DamageTracker {
    manager: Weak<ZwlrScreencopyManagerV1>,
    output: WeakOutput,
    damage: Arc<Mutex<Damage>>,
}

impl ScreencopyManagerState {
    /// Creates the `zwlr_screencopy_manager_v1` global, visible to all clients.
    pub fn new<D: ScreencopyHandler>(dh: &DisplayHandle) -> Self {
        Self::new_with_filter::<D, _>(dh, |_| true)
    }

    /// Creates the `zwlr_screencopy_manager_v1` global, visible to clients for which `filter` returns true.
    pub fn new_with_filter<D, F>(dh: &DisplayHandle, filter: F) -> Self
    where
        D: ScreencopyHandler,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global = dh.create_global::<D, ZwlrScreencopyManagerV1, _>(
            MANAGER_VERSION,
            ScreencopyGlobalData {
                filter: Box::new(filter),
            },
        );
        Self {
            global,
            trackers: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// The [`GlobalId`] of the global.
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// Reports the damage of a frame the compositor just rendered for `output`.
    ///
    /// `damage` is in output-local physical coordinates in the orientation of
    /// the logical (untransformed) output, i.e. what
    /// [`OutputDamageTracker::render_output`](smithay::backend::renderer::damage::OutputDamageTracker::render_output)
    /// returns. Hands all `copy_with_damage` frames that were waiting for this
    /// damage to [`ScreencopyHandler::screencopy_frame`].
    pub fn damage_output<D: ScreencopyHandler>(
        state: &mut D,
        output: &Output,
        damage: &[Rectangle<i32, Physical>],
    ) {
        if damage.is_empty() {
            return;
        }
        let this = state.screencopy_state();
        this.trackers.retain(|tracker| {
            tracker.output.upgrade().is_some()
                && (tracker.manager.upgrade().is_ok() || Arc::strong_count(&tracker.damage) > 1)
        });
        for tracker in this
            .trackers
            .iter()
            .filter(|tracker| tracker.output == *output)
        {
            tracker.damage.lock().unwrap().add(damage);
        }

        let mut ready = Vec::new();
        let mut i = 0;
        while i < this.pending.len() {
            let frame = &mut this.pending[i];
            if frame.output == *output && frame.take_tracked_damage() {
                ready.push(this.pending.remove(i));
            } else {
                i += 1;
            }
        }
        for frame in ready {
            state.screencopy_frame(frame);
        }
    }

    /// Fails all frames still waiting for damage on `output` and forgets its damage state.
    ///
    /// Frames already handed to the compositor are not affected; the
    /// compositor should fail those itself.
    pub fn output_removed(&mut self, output: &Output) {
        self.trackers.retain(|tracker| tracker.output != *output);
        // Dropping the frames sends `failed`.
        self.pending.retain(|frame| frame.output != *output);
    }
}

/// Global data of the `zwlr_screencopy_manager_v1` global.
pub struct ScreencopyGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

impl std::fmt::Debug for ScreencopyGlobalData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreencopyGlobalData")
            .finish_non_exhaustive()
    }
}

/// User data of `zwlr_screencopy_frame_v1` objects.
#[derive(Debug)]
pub struct ScreencopyFrameData {
    inner: Mutex<FrameInfo>,
}

#[derive(Debug)]
struct FrameInfo {
    /// `None` if the frame failed right away.
    target: Option<CaptureTarget>,
    manager: Weak<ZwlrScreencopyManagerV1>,
    used: bool,
}

#[derive(Debug, Clone)]
struct CaptureTarget {
    output: WeakOutput,
    region: Rectangle<i32, Physical>,
    transform: Transform,
    buffer_size: Size<i32, Buffer>,
    overlay_cursor: bool,
}

/// A frame to be rendered for a screencopy client.
///
/// Render the [`region`](Self::region) of [`output`](Self::output) into
/// [`buffer`](Self::buffer), applying [`transform`](Self::transform), then call
/// [`success`](Self::success). Dropping the frame without completing it sends
/// `failed` to the client.
#[derive(Debug)]
pub struct ScreencopyFrame {
    frame: ZwlrScreencopyFrameV1,
    output: Output,
    buffer: WlBuffer,
    format: wl_shm::Format,
    region: Rectangle<i32, Physical>,
    transform: Transform,
    buffer_size: Size<i32, Buffer>,
    overlay_cursor: bool,
    with_damage: bool,
    damage: Vec<Rectangle<i32, Buffer>>,
    tracker: Option<Arc<Mutex<Damage>>>,
    y_invert: bool,
    completed: bool,
}

impl ScreencopyFrame {
    /// The output to capture.
    pub fn output(&self) -> &Output {
        &self.output
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

    /// The captured region in output-local physical coordinates, in the
    /// orientation of the logical (untransformed) output. Render elements
    /// should be offset by `-region.loc`.
    pub fn region(&self) -> Rectangle<i32, Physical> {
        self.region
    }

    /// The output transform to apply when rendering (as for the output itself).
    pub fn transform(&self) -> Transform {
        self.transform
    }

    /// Size of [`buffer`](Self::buffer); `transform` applied to the region size.
    pub fn buffer_size(&self) -> Size<i32, Buffer> {
        self.buffer_size
    }

    /// Whether the client asked for the cursor to be composited onto the frame.
    pub fn overlay_cursor(&self) -> bool {
        self.overlay_cursor
    }

    /// Whether this frame came from `copy_with_damage`.
    pub fn with_damage(&self) -> bool {
        self.with_damage
    }

    /// Damage that will be reported by [`success`](Self::success), in buffer coordinates.
    pub fn damage(&self) -> &[Rectangle<i32, Buffer>] {
        &self.damage
    }

    /// Sets the `y_invert` flag sent with [`success`](Self::success). Not
    /// needed with [`ShmCaptureRenderer`](super::capture_render::ShmCaptureRenderer).
    pub fn set_y_invert(&mut self, y_invert: bool) {
        self.y_invert = y_invert;
    }

    /// Whether the client still has the frame object.
    pub fn is_alive(&self) -> bool {
        self.frame.is_alive()
    }

    /// Completes the frame: the buffer has been filled. Sends `flags`, the
    /// tracked damage (for `copy_with_damage`) and `ready`.
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

    /// Fails the frame (same as dropping it).
    pub fn fail(self) {}

    fn complete(&mut self, damage: &[Rectangle<i32, Buffer>], presentation_time: Duration) {
        self.completed = true;
        if !self.frame.is_alive() {
            return;
        }
        self.frame.flags(if self.y_invert {
            zwlr_screencopy_frame_v1::Flags::YInvert
        } else {
            zwlr_screencopy_frame_v1::Flags::empty()
        });
        if self.with_damage && self.frame.version() >= 2 {
            for rect in damage {
                self.frame.damage(
                    rect.loc.x.max(0) as u32,
                    rect.loc.y.max(0) as u32,
                    rect.size.w.max(0) as u32,
                    rect.size.h.max(0) as u32,
                );
            }
        }
        let secs = presentation_time.as_secs();
        self.frame.ready(
            (secs >> 32) as u32,
            secs as u32,
            presentation_time.subsec_nanos(),
        );
    }

    /// Takes the damage accumulated for this frame's client/output, if it
    /// touches the captured region. Returns whether there was any.
    fn take_tracked_damage(&mut self) -> bool {
        let Some(tracker) = &self.tracker else {
            return true;
        };
        let mut tracked = tracker.lock().unwrap();
        let damage = tracked.to_buffer(self.region, self.transform);
        if damage.is_empty() {
            return false;
        }
        tracked.clear();
        self.damage = damage;
        true
    }
}

impl Drop for ScreencopyFrame {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        if let Some(tracker) = &self.tracker {
            // The client did not get this damage, so the next frame has to cover everything.
            *tracker.lock().unwrap() = Damage::Full;
        }
        if self.frame.is_alive() {
            self.frame.failed();
        }
    }
}

/// Damage accumulated since the last delivered frame, in output-local physical coordinates.
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

/// Computes what to capture of `output`: the region in output-local physical
/// coordinates, the transform and the resulting buffer size.
fn capture_target(
    output: &Output,
    logical_region: Option<Rectangle<i32, Logical>>,
    overlay_cursor: bool,
) -> Option<CaptureTarget> {
    let mode = output.current_mode()?;
    let transform = output.current_transform();
    let scale = output.current_scale().fractional_scale();
    let size = transform.transform_size(mode.size);
    let region = match logical_region {
        None => Rectangle::from_size(size),
        Some(r) => {
            let x1 = ((r.loc.x as f64 * scale).floor() as i32).clamp(0, size.w);
            let y1 = ((r.loc.y as f64 * scale).floor() as i32).clamp(0, size.h);
            let x2 = (((r.loc.x as f64 + r.size.w as f64) * scale).ceil() as i32).clamp(0, size.w);
            let y2 = (((r.loc.y as f64 + r.size.h as f64) * scale).ceil() as i32).clamp(0, size.h);
            Rectangle::new((x1, y1).into(), (x2 - x1, y2 - y1).into())
        }
    };
    if region.size.w <= 0 || region.size.h <= 0 {
        return None;
    }
    let buffer_size = transform.transform_size(region.size);
    Some(CaptureTarget {
        output: output.downgrade(),
        region,
        transform,
        buffer_size: (buffer_size.w, buffer_size.h).into(),
        overlay_cursor,
    })
}

/// Validates a client buffer against the frame's constraints, returning its format.
fn check_buffer(
    buffer: &WlBuffer,
    size: Size<i32, Buffer>,
) -> Result<wl_shm::Format, &'static str> {
    let data =
        shm::with_buffer_contents(buffer, |_, _, data| data).map_err(|_| "not a wl_shm buffer")?;
    if !matches!(
        data.format,
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
    ) {
        return Err("unsupported buffer format");
    }
    if data.width != size.w || data.height != size.h {
        return Err("buffer has the wrong size");
    }
    if data.stride < data.width * 4 {
        return Err("buffer stride is too small");
    }
    Ok(data.format)
}

impl<D: ScreencopyHandler> GlobalDispatch<ZwlrScreencopyManagerV1, ScreencopyGlobalData, D>
    for ScreencopyManagerState
{
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &ScreencopyGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, global_data: &ScreencopyGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D: ScreencopyHandler> Dispatch<ZwlrScreencopyManagerV1, (), D> for ScreencopyManagerState {
    fn request(
        _state: &mut D,
        _client: &Client,
        manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let (frame, overlay_cursor, output, region) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => (frame, overlay_cursor, output, None),
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => (
                frame,
                overlay_cursor,
                output,
                Some(Rectangle::new((x, y).into(), (width, height).into())),
            ),
            zwlr_screencopy_manager_v1::Request::Destroy => return,
            _ => unreachable!(),
        };
        init_frame(
            manager,
            frame,
            overlay_cursor != 0,
            &output,
            region,
            data_init,
        );
    }
}

fn init_frame<D: ScreencopyHandler>(
    manager: &ZwlrScreencopyManagerV1,
    frame: New<ZwlrScreencopyFrameV1>,
    overlay_cursor: bool,
    output: &WlOutput,
    region: Option<Rectangle<i32, Logical>>,
    data_init: &mut DataInit<'_, D>,
) {
    let target = Output::from_resource(output)
        .and_then(|output| capture_target(&output, region, overlay_cursor));
    let frame = data_init.init(
        frame,
        ScreencopyFrameData {
            inner: Mutex::new(FrameInfo {
                target: target.clone(),
                manager: manager.downgrade(),
                used: false,
            }),
        },
    );
    match target {
        Some(target) => {
            let size = target.buffer_size;
            frame.buffer(
                ADVERTISED_FORMAT,
                size.w as u32,
                size.h as u32,
                size.w as u32 * 4,
            );
            if frame.version() >= 3 {
                frame.buffer_done();
            }
        }
        None => frame.failed(),
    }
}

impl<D: ScreencopyHandler> Dispatch<ZwlrScreencopyFrameV1, ScreencopyFrameData, D>
    for ScreencopyManagerState
{
    fn request(
        state: &mut D,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &ScreencopyFrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => return,
            _ => unreachable!(),
        };

        let mut info = data.inner.lock().unwrap();
        if info.used {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "frame was already used to copy a buffer",
            );
            return;
        }
        info.used = true;
        let Some(target) = info.target.clone() else {
            frame.failed();
            return;
        };
        let manager = info.manager.clone();
        drop(info);

        let format = match check_buffer(&buffer, target.buffer_size) {
            Ok(format) => format,
            Err(msg) => {
                frame.post_error(zwlr_screencopy_frame_v1::Error::InvalidBuffer, msg);
                return;
            }
        };
        // The output may have gone away or changed its mode since the frame was created.
        let Some(output) = target.output.upgrade() else {
            frame.failed();
            return;
        };
        let still_fits = capture_target(&output, None, false).is_some_and(|full| {
            full.region.contains_rect(target.region) && full.transform == target.transform
        });
        if !still_fits {
            frame.failed();
            return;
        }

        let mut capture = ScreencopyFrame {
            frame: frame.clone(),
            output,
            buffer,
            format,
            region: target.region,
            transform: target.transform,
            buffer_size: target.buffer_size,
            overlay_cursor: target.overlay_cursor,
            with_damage,
            damage: Vec::new(),
            tracker: None,
            y_invert: false,
            completed: false,
        };
        if !with_damage {
            state.screencopy_frame(capture);
            return;
        }

        let this = state.screencopy_state();
        let tracker = match this
            .trackers
            .iter()
            .find(|tracker| tracker.manager == manager && tracker.output == capture.output)
        {
            Some(tracker) => tracker.damage.clone(),
            None => {
                // First frame of this client/output: everything is damaged.
                let damage = Arc::new(Mutex::new(Damage::Full));
                this.trackers.push(DamageTracker {
                    manager,
                    output: capture.output.downgrade(),
                    damage: damage.clone(),
                });
                damage
            }
        };
        capture.tracker = Some(tracker);
        if capture.take_tracked_damage() {
            state.screencopy_frame(capture);
        } else {
            this.pending.push(capture);
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        frame: &ZwlrScreencopyFrameV1,
        _data: &ScreencopyFrameData,
    ) {
        state
            .screencopy_state()
            .pending
            .retain(|pending| pending.frame != *frame);
    }
}

/// Delegates `zwlr_screencopy_manager_v1` to [`ScreencopyManagerState`].
///
/// The type must implement [`ScreencopyHandler`].
macro_rules! delegate_screencopy {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        ::smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: $crate::compositor::protocols::screencopy::ScreencopyGlobalData
        ] => $crate::compositor::protocols::screencopy::ScreencopyManagerState);
        ::smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: ()
        ] => $crate::compositor::protocols::screencopy::ScreencopyManagerState);
        ::smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            ::smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1: $crate::compositor::protocols::screencopy::ScreencopyFrameData
        ] => $crate::compositor::protocols::screencopy::ScreencopyManagerState);
    };
}
pub(crate) use delegate_screencopy;
