//! Server side of `xdg-toplevel-icon-v1` (version 1).
//!
//! A client builds an icon from an icon-theme name and square wl_shm
//! images, then sets it on one of its toplevels; it takes effect with the
//! toplevel's next commit, and the client can set a new one at any time.
//! canoe shows it for the window's desktop icon while it is minimized.
//!
//! Smithay has an implementation, but it asserts in debug builds (and goes
//! on changing the icon in release builds) when a client changes an icon
//! after setting it, instead of only raising the protocol error, and it
//! keeps the client's buffers to read later, which fails once the client
//! destroys them after destroying the icon, as it may. This one copies the
//! pixels when the icon is set:
//!
//! * `set_name` and `add_buffer` on an icon that was set are an `immutable`
//!   error; a buffer that isn't wl_shm or isn't square is `invalid_buffer`;
//!   a buffer destroyed before the icon is set is `no_buffer`. (A buffer
//!   destroyed afterwards isn't noticed: its pixels were copied already.)
//! * Images in formats other than (A|X)(RGB|BGR)8888 are skipped. Images
//!   larger than [`CLIENT_ICON_MAX_EDGE`] are scaled down to it, and only
//!   the largest few are kept, so a client can't make canoe hold much.
//! * An icon with neither a name nor a usable image, like a null one,
//!   resets the window to its default icon.
//!
//! The compositor calls [`commit`] for each commit of a toplevel; it
//! returns the toplevel's new icon if this commit changed it.
//!
//! ```ignore
//! delegate_toplevel_icon!(State);
//!
//! ToplevelIconManagerState::new::<State>(&display_handle);
//! ```

use std::sync::{Arc, Mutex};

use smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::server::{
    xdg_toplevel_icon_manager_v1::{self, XdgToplevelIconManagerV1},
    xdg_toplevel_icon_v1::{self, XdgToplevelIconV1},
};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::protocol::{
    wl_buffer::WlBuffer, wl_shm, wl_surface::WlSurface,
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgShellHandler;
use smithay::wayland::shm::{with_buffer_contents, BufferData};

use crate::canoe::{ClientIcon, CLIENT_ICON_MAX_EDGE};

const VERSION: u32 = 1;

/// The icon size canoe asks for, in surface-local pixels: that of its
/// desktop icons.
const PREFERRED_SIZE: i32 = crate::canoe::ICON_SIZE;

/// Images more than this many pixels across are read only every n-th pixel
/// (before being scaled down smoothly), so a huge icon doesn't have to be
/// copied whole.
const MAX_READ_EDGE: usize = 4 * CLIENT_ICON_MAX_EDGE as usize;

/// The compositor's side of xdg-toplevel-icon.
pub trait ToplevelIconHandler:
    XdgShellHandler
    + GlobalDispatch<XdgToplevelIconManagerV1, ()>
    + Dispatch<XdgToplevelIconManagerV1, ()>
    + Dispatch<XdgToplevelIconV1, IconData>
    + 'static
{
}

impl<D> ToplevelIconHandler for D where
    D: XdgShellHandler
        + GlobalDispatch<XdgToplevelIconManagerV1, ()>
        + Dispatch<XdgToplevelIconManagerV1, ()>
        + Dispatch<XdgToplevelIconV1, IconData>
        + 'static
{
}

/// Data of an `xdg_toplevel_icon_v1`.
#[derive(Default)]
pub struct IconData {
    inner: Mutex<Icon>,
}

#[derive(Default)]
struct Icon {
    name: Option<String>,
    /// Images by size in pixels and scale; a later one of the same size and
    /// scale replaces the earlier.
    buffers: Vec<(WlBuffer, i32, i32)>,
    /// What the icon became when it was first set on a toplevel, after
    /// which it can't change: `Some(None)` for an empty icon.
    frozen: Option<Option<Arc<ClientIcon>>>,
}

/// A toplevel's icon change, waiting for its next commit.
#[derive(Default)]
struct PendingIcon(Mutex<Option<Option<Arc<ClientIcon>>>>);

/// State of the xdg-toplevel-icon protocol.
pub struct ToplevelIconManagerState {
    global: GlobalId,
}

impl ToplevelIconManagerState {
    /// Create the `xdg_toplevel_icon_manager_v1` global.
    pub fn new<D: ToplevelIconHandler>(display: &DisplayHandle) -> Self {
        Self {
            global: display.create_global::<D, XdgToplevelIconManagerV1, _>(VERSION, ()),
        }
    }

    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

/// Apply the icon change `surface` (a toplevel) made before this commit.
/// Returns its new icon, `Some(None)` for its default one, or `None` if the
/// commit didn't change it.
pub fn commit(surface: &WlSurface) -> Option<Option<Arc<ClientIcon>>> {
    with_states(surface, |states| {
        states
            .data_map
            .get::<PendingIcon>()
            .and_then(|pending| pending.0.lock().unwrap().take())
    })
}

impl<D: ToplevelIconHandler> GlobalDispatch<XdgToplevelIconManagerV1, (), D>
    for ToplevelIconManagerState
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XdgToplevelIconManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        manager.icon_size(PREFERRED_SIZE);
        manager.done();
    }
}

impl<D: ToplevelIconHandler> Dispatch<XdgToplevelIconManagerV1, (), D>
    for ToplevelIconManagerState
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &XdgToplevelIconManagerV1,
        request: xdg_toplevel_icon_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            xdg_toplevel_icon_manager_v1::Request::CreateIcon { id } => {
                data_init.init(id, IconData::default());
            }
            xdg_toplevel_icon_manager_v1::Request::SetIcon { toplevel, icon } => {
                let icon = match icon {
                    Some(icon) => match freeze(&icon) {
                        Some(frozen) => frozen,
                        // A protocol error was raised.
                        None => return,
                    },
                    None => None,
                };
                // A toplevel whose role object is gone has nothing to show
                // the icon on.
                let Some(toplevel) = state.xdg_shell_state().get_toplevel(&toplevel) else {
                    return;
                };
                with_states(toplevel.wl_surface(), |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(PendingIcon::default);
                    *states
                        .data_map
                        .get::<PendingIcon>()
                        .unwrap()
                        .0
                        .lock()
                        .unwrap() = Some(icon);
                });
            }
            xdg_toplevel_icon_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

/// The icon `icon` stands for from now on; it can't change any more.
/// `None` after raising a protocol error.
fn freeze(icon: &XdgToplevelIconV1) -> Option<Option<Arc<ClientIcon>>> {
    let data = icon.data::<IconData>().unwrap();
    let mut inner = data.inner.lock().unwrap();
    if let Some(frozen) = &inner.frozen {
        return Some(frozen.clone());
    }
    if inner
        .buffers
        .iter()
        .any(|(buffer, _, _)| !buffer.is_alive())
    {
        icon.post_error(
            xdg_toplevel_icon_v1::Error::NoBuffer,
            "a buffer of the icon was destroyed before the icon",
        );
        return None;
    }
    let images = inner
        .buffers
        .iter()
        .filter_map(|(buffer, _, _)| read_image(buffer))
        .collect();
    let frozen = ClientIcon::new(inner.name.take(), images).map(Arc::new);
    // The buffers aren't needed any more: their pixels were copied.
    inner.buffers.clear();
    inner.frozen = Some(frozen.clone());
    Some(frozen)
}

impl<D: ToplevelIconHandler> Dispatch<XdgToplevelIconV1, IconData, D> for ToplevelIconManagerState {
    fn request(
        _state: &mut D,
        _client: &Client,
        resource: &XdgToplevelIconV1,
        request: xdg_toplevel_icon_v1::Request,
        data: &IconData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let mut inner = data.inner.lock().unwrap();
        let immutable = || {
            resource.post_error(
                xdg_toplevel_icon_v1::Error::Immutable,
                "the icon was set on a toplevel already",
            )
        };
        match request {
            xdg_toplevel_icon_v1::Request::SetName { icon_name } => {
                if inner.frozen.is_some() {
                    immutable();
                    return;
                }
                inner.name = Some(icon_name);
            }
            xdg_toplevel_icon_v1::Request::AddBuffer { buffer, scale } => {
                if inner.frozen.is_some() {
                    immutable();
                    return;
                }
                let Ok(info) = with_buffer_contents(&buffer, |_, _, info| info) else {
                    resource.post_error(
                        xdg_toplevel_icon_v1::Error::InvalidBuffer,
                        "the buffer is not a wl_shm buffer",
                    );
                    return;
                };
                if info.width != info.height {
                    resource.post_error(
                        xdg_toplevel_icon_v1::Error::InvalidBuffer,
                        format!("the buffer is not square ({}x{})", info.width, info.height),
                    );
                    return;
                }
                let size = info.width;
                inner.buffers.retain(|(_, other_size, other_scale)| {
                    (*other_size, *other_scale) != (size, scale)
                });
                inner.buffers.push((buffer, size, scale));
            }
            xdg_toplevel_icon_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

/// Copy the pixels of a wl_shm buffer. `None` for formats other than
/// (A|X)(RGB|BGR)8888 and for buffers that don't fit their pool.
fn read_image(buffer: &WlBuffer) -> Option<resvg::tiny_skia::Pixmap> {
    with_buffer_contents(buffer, |ptr, len, info| {
        // SAFETY: `ptr` is valid for `len` bytes while this runs; the
        // client may change them meanwhile, which can only garble the
        // copied pixels.
        unsafe { copy_image(ptr, len, info) }
    })
    .ok()
    .flatten()
}

/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes.
unsafe fn copy_image(
    ptr: *const u8,
    len: usize,
    info: BufferData,
) -> Option<resvg::tiny_skia::Pixmap> {
    // Byte order in memory (little endian): [blue, green, red, alpha] for
    // ARGB and XRGB, [red, green, blue, alpha] for ABGR and XBGR.
    let (bgr, opaque) = match info.format {
        wl_shm::Format::Argb8888 => (true, false),
        wl_shm::Format::Xrgb8888 => (true, true),
        wl_shm::Format::Abgr8888 => (false, false),
        wl_shm::Format::Xbgr8888 => (false, true),
        _ => return None,
    };
    let width = usize::try_from(info.width).ok().filter(|&w| w > 0)?;
    let height = usize::try_from(info.height).ok().filter(|&h| h > 0)?;
    let stride = usize::try_from(info.stride).ok()?;
    let offset = usize::try_from(info.offset).ok()?;
    let row_len = width.checked_mul(4)?;
    let end = stride
        .checked_mul(height - 1)?
        .checked_add(row_len)?
        .checked_add(offset)?;
    if stride < row_len || end > len {
        return None;
    }

    let step = width.div_ceil(MAX_READ_EDGE);
    let out_width = width.div_ceil(step);
    let out_height = height.div_ceil(step);
    let mut pixels = Vec::with_capacity(out_width * out_height * 4);
    let mut row = vec![0u8; row_len];
    for y in (0..height).step_by(step) {
        // SAFETY: the row lies within `offset..end`, checked above.
        unsafe {
            std::ptr::copy_nonoverlapping(ptr.add(offset + y * stride), row.as_mut_ptr(), row_len);
        }
        for &[c0, c1, c2, c3] in row.as_chunks::<4>().0.iter().step_by(step) {
            let (r, g, b) = if bgr { (c2, c1, c0) } else { (c0, c1, c2) };
            let a = if opaque { 255 } else { c3 };
            // wl_shm pixels are premultiplied, as tiny-skia's are; clamp
            // what isn't valid premultiplied color.
            pixels.extend_from_slice(&[r.min(a), g.min(a), b.min(a), a]);
        }
    }
    let size = resvg::tiny_skia::IntSize::from_wh(out_width as u32, out_height as u32)?;
    resvg::tiny_skia::Pixmap::from_vec(pixels, size)
}

macro_rules! delegate_toplevel_icon {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::server::xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1: ()
        ] => $crate::compositor::protocols::toplevel_icon::ToplevelIconManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::server::xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1: ()
        ] => $crate::compositor::protocols::toplevel_icon::ToplevelIconManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::server::xdg_toplevel_icon_v1::XdgToplevelIconV1:
                $crate::compositor::protocols::toplevel_icon::IconData
        ] => $crate::compositor::protocols::toplevel_icon::ToplevelIconManagerState);
    };
}
pub(crate) use delegate_toplevel_icon;

#[cfg(test)]
mod tests {
    use super::*;

    fn info(width: i32, height: i32, stride: i32, format: wl_shm::Format) -> BufferData {
        BufferData {
            offset: 0,
            width,
            height,
            stride,
            format,
        }
    }

    fn rgba(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
        let p = pixmap.pixel(x, y).unwrap();
        [p.red(), p.green(), p.blue(), p.alpha()]
    }

    #[test]
    fn copies_argb_and_abgr_pixels() {
        // 2x2 with padding at the end of each row.
        let mut data = Vec::new();
        for px in [[0x10, 0x20, 0x30, 0x40], [0, 0, 0xff, 0xff]] {
            data.extend_from_slice(&px);
        }
        data.extend_from_slice(&[9; 4]);
        for px in [[0xff, 0, 0, 0xff], [0x80, 0x80, 0x80, 0x00]] {
            data.extend_from_slice(&px);
        }
        data.extend_from_slice(&[9; 4]);

        let argb = unsafe {
            copy_image(
                data.as_ptr(),
                data.len(),
                info(2, 2, 12, wl_shm::Format::Argb8888),
            )
        }
        .unwrap();
        assert_eq!(rgba(&argb, 0, 0), [0x30, 0x20, 0x10, 0x40]);
        assert_eq!(rgba(&argb, 1, 0), [0xff, 0, 0, 0xff]);
        assert_eq!(rgba(&argb, 0, 1), [0, 0, 0xff, 0xff]);
        // Not premultiplied: clamped to the alpha.
        assert_eq!(rgba(&argb, 1, 1), [0, 0, 0, 0]);

        let xbgr = unsafe {
            copy_image(
                data.as_ptr(),
                data.len(),
                info(2, 2, 12, wl_shm::Format::Xbgr8888),
            )
        }
        .unwrap();
        assert_eq!(rgba(&xbgr, 0, 0), [0x10, 0x20, 0x30, 0xff]);
        assert_eq!(rgba(&xbgr, 1, 1), [0x80, 0x80, 0x80, 0xff]);
    }

    #[test]
    fn rejects_buffers_outside_their_pool_and_other_formats() {
        let data = [0u8; 16];
        let read = |info| unsafe { copy_image(data.as_ptr(), data.len(), info) };
        assert!(read(info(2, 2, 8, wl_shm::Format::Argb8888)).is_some());
        assert!(read(info(2, 2, 12, wl_shm::Format::Argb8888)).is_none());
        assert!(read(info(2, 2, 4, wl_shm::Format::Argb8888)).is_none());
        assert!(read(BufferData {
            offset: 4,
            ..info(2, 2, 8, wl_shm::Format::Argb8888)
        })
        .is_none());
        assert!(read(info(2, 2, 8, wl_shm::Format::Rgb565)).is_none());
    }

    #[test]
    fn reads_huge_images_sparsely() {
        let edge = MAX_READ_EDGE * 2 + 1;
        let data = vec![0xffu8; edge * edge * 4];
        let pixmap = unsafe {
            copy_image(
                data.as_ptr(),
                data.len(),
                info(
                    edge as i32,
                    edge as i32,
                    edge as i32 * 4,
                    wl_shm::Format::Argb8888,
                ),
            )
        }
        .unwrap();
        assert_eq!(pixmap.width() as usize, edge.div_ceil(3));
        assert_eq!(pixmap.height(), pixmap.width());
    }
}
