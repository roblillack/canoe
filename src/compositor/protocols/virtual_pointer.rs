//! Server side of `wlr-virtual-pointer-unstable-v1`
//! (`zwlr_virtual_pointer_manager_v1`, version 2).
//!
//! Lets clients such as `wlrctl`, `ydotool`-alikes, `wayvnc` or `lan-mouse`
//! emulate a pointer. Requests are translated into [`VirtualPointerEvent`]s
//! and handed to [`VirtualPointerHandler::virtual_pointer_event`], from where
//! the compositor feeds them into its normal pointer input path.
//!
//! # Event delivery
//!
//! * `motion`, `motion_absolute` and `button` are forwarded immediately, one
//!   event per request.
//! * `axis`, `axis_source`, `axis_stop` and `axis_discrete` are accumulated
//!   into one [`AxisFrame`] (values of the same axis are summed,
//!   `axis_discrete` steps are converted to v120 units). When the client sends
//!   `frame`, that axis frame is delivered as [`VirtualPointerEvent::Axis`]
//!   (only if there was any axis request), immediately followed by
//!   [`VirtualPointerEvent::Frame`]. That maps one-to-one onto
//!   `PointerHandle::axis(state, frame)` + `PointerHandle::frame(state)`.
//!   Axis requests not followed by `frame` are never delivered.
//! * `frame` is always delivered as [`VirtualPointerEvent::Frame`].
//!
//! ```ignore
//! impl VirtualPointerHandler for State {
//!     fn virtual_pointer_event(&mut self, pointer: &VirtualPointer<Self>, event: VirtualPointerEvent) {
//!         match event { /* feed into pointer handling */ }
//!     }
//! }
//! delegate_virtual_pointer!(State);
//!
//! let virtual_pointer_state = VirtualPointerManagerState::new::<State, _>(&display_handle, |_client| true);
//! ```

// A complete protocol API: not every accessor is used by canoe.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use smithay::backend::input::{Axis, AxisSource, ButtonState};
use smithay::input::pointer::AxisFrame;
use smithay::input::{Seat, SeatHandler};
use smithay::output::{Output, WeakOutput};
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::{
    zwlr_virtual_pointer_manager_v1::{self, ZwlrVirtualPointerManagerV1},
    zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
};
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    protocol::{wl_output::WlOutput, wl_pointer, wl_seat::WlSeat},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use smithay::utils::{Logical, Point, Rectangle};

const MANAGER_VERSION: u32 = 2;

/// An absolute motion request (`motion_absolute`).
#[derive(Debug, Clone)]
pub struct VirtualPointerAbsoluteMotion {
    /// Timestamp in milliseconds.
    pub time: u32,
    /// Position on the x axis, in `0..=x_extent`.
    pub x: u32,
    /// Position on the y axis, in `0..=y_extent`.
    pub y: u32,
    /// Extent of the x axis (guaranteed to be non-zero).
    pub x_extent: u32,
    /// Extent of the y axis (guaranteed to be non-zero).
    pub y_extent: u32,
    /// The output the pointer was bound to with
    /// `create_virtual_pointer_with_output`, if any (and still alive). If
    /// `None`, the coordinates span the whole output layout.
    pub output: Option<Output>,
}

impl VirtualPointerAbsoluteMotion {
    /// Maps the position into `area` (normally the geometry of
    /// [`Self::output`], or the bounding box of all outputs if there is none).
    /// The result is clamped to the area's pixels, i.e. `x == x_extent` lands
    /// on the last column (`area.loc.x + area.size.w - 1`), not beyond it.
    pub fn position_in(&self, area: Rectangle<i32, Logical>) -> Point<f64, Logical> {
        let map = |v: u32, extent: u32, origin: i32, len: i32| {
            let pos = origin as f64 + (v as f64 / extent as f64) * len as f64;
            let max = origin as f64 + (len as f64 - 1.0).max(0.0);
            pos.clamp(origin as f64, max)
        };
        Point::from((
            map(self.x, self.x_extent, area.loc.x, area.size.w),
            map(self.y, self.y_extent, area.loc.y, area.size.h),
        ))
    }
}

/// An event produced by a virtual pointer.
#[derive(Debug, Clone)]
pub enum VirtualPointerEvent {
    /// Relative motion, in the global compositor space.
    Motion {
        /// Timestamp in milliseconds.
        time: u32,
        /// Displacement.
        delta: Point<f64, Logical>,
    },
    /// Absolute motion.
    MotionAbsolute(VirtualPointerAbsoluteMotion),
    /// A button was pressed or released.
    Button {
        /// Timestamp in milliseconds.
        time: u32,
        /// Button code (linux input event code, e.g. `BTN_LEFT` = 0x110).
        button: u32,
        /// New button state.
        state: ButtonState,
    },
    /// All axis requests since the previous `frame`, delivered right before
    /// [`VirtualPointerEvent::Frame`].
    Axis(AxisFrame),
    /// End of a group of events that logically belong together.
    Frame,
}

/// Handler trait for virtual pointers.
pub trait VirtualPointerHandler:
    SeatHandler
    + GlobalDispatch<ZwlrVirtualPointerManagerV1, VirtualPointerManagerGlobalData>
    + Dispatch<ZwlrVirtualPointerManagerV1, ()>
    + Dispatch<ZwlrVirtualPointerV1, VirtualPointer<Self>>
    + Sized
    + 'static
{
    /// An event from a virtual pointer.
    fn virtual_pointer_event(&mut self, pointer: &VirtualPointer<Self>, event: VirtualPointerEvent);

    /// A virtual pointer was created.
    fn virtual_pointer_created(&mut self, _pointer: &VirtualPointer<Self>) {}

    /// A virtual pointer was destroyed (by request, or because its client went
    /// away). A good moment to release buttons it left pressed.
    fn virtual_pointer_destroyed(&mut self, _pointer: &VirtualPointer<Self>) {}
}

/// Global data of the `zwlr_virtual_pointer_manager_v1` global.
pub struct VirtualPointerManagerGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

impl std::fmt::Debug for VirtualPointerManagerGlobalData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualPointerManagerGlobalData")
            .finish_non_exhaustive()
    }
}

/// State of the virtual pointer manager global.
#[derive(Debug)]
pub struct VirtualPointerManagerState {
    #[allow(dead_code)]
    global: GlobalId,
}

impl VirtualPointerManagerState {
    /// Creates the `zwlr_virtual_pointer_manager_v1` global. It is only
    /// advertised to clients for which `filter` returns `true`; since virtual
    /// pointers can drive the whole desktop, restricting it is advisable.
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: VirtualPointerHandler,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global = display.create_global::<D, ZwlrVirtualPointerManagerV1, _>(
            MANAGER_VERSION,
            VirtualPointerManagerGlobalData {
                filter: Box::new(filter),
            },
        );
        Self { global }
    }

    /// The id of the global.
    #[allow(dead_code)]
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

struct VirtualPointerInner<D: SeatHandler> {
    seat: Option<Seat<D>>,
    output: Option<WeakOutput>,
    pending_axis: Mutex<Option<AxisFrame>>,
}

/// A virtual pointer device. Also the user data of `zwlr_virtual_pointer_v1`.
///
/// Cheap to clone; clones compare equal.
pub struct VirtualPointer<D: SeatHandler> {
    inner: Arc<VirtualPointerInner<D>>,
}

impl<D: SeatHandler> Clone for VirtualPointer<D> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<D: SeatHandler> PartialEq for VirtualPointer<D> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl<D: SeatHandler> Eq for VirtualPointer<D> {}

impl<D: SeatHandler + 'static> std::fmt::Debug for VirtualPointer<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualPointer")
            .field(
                "seat",
                &self.inner.seat.as_ref().map(|s| s.name().to_owned()),
            )
            .field("output", &self.output().map(|o| o.name()))
            .finish_non_exhaustive()
    }
}

impl<D: SeatHandler> VirtualPointer<D> {
    /// The seat the client asked for, if any. If `None`, use the default seat.
    pub fn seat(&self) -> Option<&Seat<D>> {
        self.inner.seat.as_ref()
    }

    /// The output the client asked to map this pointer to, if any (and still
    /// alive).
    pub fn output(&self) -> Option<Output> {
        self.inner.output.as_ref().and_then(|o| o.upgrade())
    }

    /// Returns the pointer of a `zwlr_virtual_pointer_v1` resource.
    #[allow(dead_code)]
    pub fn from_resource(resource: &ZwlrVirtualPointerV1) -> Option<Self>
    where
        D: 'static,
    {
        resource.data::<Self>().cloned()
    }
}

impl<D: VirtualPointerHandler>
    GlobalDispatch<ZwlrVirtualPointerManagerV1, VirtualPointerManagerGlobalData, D>
    for VirtualPointerManagerState
{
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrVirtualPointerManagerV1>,
        _global_data: &VirtualPointerManagerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }

    fn can_view(client: Client, global_data: &VirtualPointerManagerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

fn create_pointer<D: VirtualPointerHandler>(
    state: &mut D,
    data_init: &mut DataInit<'_, D>,
    id: New<ZwlrVirtualPointerV1>,
    seat: Option<WlSeat>,
    output: Option<WlOutput>,
) {
    let pointer = VirtualPointer {
        inner: Arc::new(VirtualPointerInner {
            seat: seat.as_ref().and_then(Seat::<D>::from_resource),
            output: output
                .as_ref()
                .and_then(Output::from_resource)
                .map(|o| o.downgrade()),
            pending_axis: Mutex::new(None),
        }),
    };
    data_init.init(id, pointer.clone());
    state.virtual_pointer_created(&pointer);
}

impl<D: VirtualPointerHandler> Dispatch<ZwlrVirtualPointerManagerV1, (), D>
    for VirtualPointerManagerState
{
    fn request(
        state: &mut D,
        _client: &Client,
        _manager: &ZwlrVirtualPointerManagerV1,
        request: zwlr_virtual_pointer_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        use zwlr_virtual_pointer_manager_v1::Request;
        match request {
            Request::CreateVirtualPointer { seat, id } => {
                create_pointer(state, data_init, id, seat, None)
            }
            Request::CreateVirtualPointerWithOutput { seat, output, id } => {
                create_pointer(state, data_init, id, seat, output)
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

fn convert_axis(axis: WEnum<wl_pointer::Axis>) -> Option<Axis> {
    match axis {
        WEnum::Value(wl_pointer::Axis::VerticalScroll) => Some(Axis::Vertical),
        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => Some(Axis::Horizontal),
        _ => None,
    }
}

fn convert_axis_source(source: WEnum<wl_pointer::AxisSource>) -> Option<AxisSource> {
    match source {
        WEnum::Value(wl_pointer::AxisSource::Wheel) => Some(AxisSource::Wheel),
        WEnum::Value(wl_pointer::AxisSource::Finger) => Some(AxisSource::Finger),
        WEnum::Value(wl_pointer::AxisSource::Continuous) => Some(AxisSource::Continuous),
        WEnum::Value(wl_pointer::AxisSource::WheelTilt) => Some(AxisSource::WheelTilt),
        _ => None,
    }
}

impl<D: VirtualPointerHandler> Dispatch<ZwlrVirtualPointerV1, VirtualPointer<D>, D>
    for VirtualPointerManagerState
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrVirtualPointerV1,
        request: zwlr_virtual_pointer_v1::Request,
        pointer: &VirtualPointer<D>,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        use zwlr_virtual_pointer_v1::{Error, Request};

        // Runs `f` on the pending axis frame (creating it if needed).
        let with_axis = |time: Option<u32>, f: &mut dyn FnMut(&mut AxisFrame)| {
            let mut pending = pointer.inner.pending_axis.lock().unwrap();
            let frame = pending.get_or_insert_with(|| AxisFrame::new(time.unwrap_or(0)));
            if let Some(time) = time {
                frame.time = time;
            }
            f(frame);
        };
        let invalid_axis = || resource.post_error(Error::InvalidAxis, "invalid axis");

        match request {
            Request::Motion { time, dx, dy } => {
                let event = VirtualPointerEvent::Motion {
                    time,
                    delta: (dx, dy).into(),
                };
                state.virtual_pointer_event(pointer, event);
            }
            Request::MotionAbsolute {
                time,
                x,
                y,
                x_extent,
                y_extent,
            } => {
                if x_extent == 0 || y_extent == 0 {
                    return;
                }
                let event = VirtualPointerEvent::MotionAbsolute(VirtualPointerAbsoluteMotion {
                    time,
                    x,
                    y,
                    x_extent,
                    y_extent,
                    output: pointer.output(),
                });
                state.virtual_pointer_event(pointer, event);
            }
            Request::Button {
                time,
                button,
                state: button_state,
            } => {
                let button_state = match button_state {
                    WEnum::Value(wl_pointer::ButtonState::Pressed) => ButtonState::Pressed,
                    WEnum::Value(wl_pointer::ButtonState::Released) => ButtonState::Released,
                    _ => return,
                };
                let event = VirtualPointerEvent::Button {
                    time,
                    button,
                    state: button_state,
                };
                state.virtual_pointer_event(pointer, event);
            }
            Request::Axis { time, axis, value } => {
                let Some(axis) = convert_axis(axis) else {
                    return invalid_axis();
                };
                with_axis(Some(time), &mut |frame| match axis {
                    Axis::Horizontal => frame.axis.0 += value,
                    Axis::Vertical => frame.axis.1 += value,
                });
            }
            Request::AxisSource { axis_source } => {
                let Some(source) = convert_axis_source(axis_source) else {
                    resource.post_error(Error::InvalidAxisSource, "invalid axis source");
                    return;
                };
                with_axis(None, &mut |frame| frame.source = Some(source));
            }
            Request::AxisStop { time, axis } => {
                let Some(axis) = convert_axis(axis) else {
                    return invalid_axis();
                };
                with_axis(Some(time), &mut |frame| match axis {
                    Axis::Horizontal => frame.stop.0 = true,
                    Axis::Vertical => frame.stop.1 = true,
                });
            }
            Request::AxisDiscrete {
                time,
                axis,
                value,
                discrete,
            } => {
                let Some(axis) = convert_axis(axis) else {
                    return invalid_axis();
                };
                with_axis(Some(time), &mut |frame| {
                    let v120 = frame.v120.get_or_insert((0, 0));
                    match axis {
                        Axis::Horizontal => {
                            frame.axis.0 += value;
                            v120.0 += discrete.saturating_mul(120);
                        }
                        Axis::Vertical => {
                            frame.axis.1 += value;
                            v120.1 += discrete.saturating_mul(120);
                        }
                    }
                });
            }
            Request::Frame => {
                let axis = pointer.inner.pending_axis.lock().unwrap().take();
                if let Some(frame) = axis {
                    state.virtual_pointer_event(pointer, VirtualPointerEvent::Axis(frame));
                }
                state.virtual_pointer_event(pointer, VirtualPointerEvent::Frame);
            }
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        _resource: &ZwlrVirtualPointerV1,
        pointer: &VirtualPointer<D>,
    ) {
        state.virtual_pointer_destroyed(pointer);
    }
}

/// Implements the wayland dispatch traits for the virtual pointer protocol on
/// `$ty` (which must implement [`VirtualPointerHandler`]).
macro_rules! delegate_virtual_pointer {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1:
                $crate::compositor::protocols::virtual_pointer::VirtualPointerManagerGlobalData
        ] => $crate::compositor::protocols::virtual_pointer::VirtualPointerManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1: ()
        ] => $crate::compositor::protocols::virtual_pointer::VirtualPointerManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1:
                $crate::compositor::protocols::virtual_pointer::VirtualPointer<Self>
        ] => $crate::compositor::protocols::virtual_pointer::VirtualPointerManagerState);
    };
}
pub(crate) use delegate_virtual_pointer;
