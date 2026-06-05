//! Anonymous shared-memory file for wl_shm pools, plus a double-buffered
//! [`ShmPool`] that lets surfaces grow without reallocating shm storage on
//! every resize step.
//!
//! Platform strategies for the underlying anonymous fd:
//! * Linux: `memfd_create` (in-memory anonymous fd).
//! * OpenBSD: `shm_mkstemp` + immediate `shm_unlink` (POSIX shm object, kept
//!   alive by the open fd and not visible in any namespace).
//! * Other Unixes: unlinked tempfile via the `tempfile` crate.

use memmap2::MmapMut;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool};
use wayland_client::QueueHandle;

/// Tracks whether the compositor still owns a `wl_buffer`. Shared between the
/// slot in [`ShmPool`] and the `wl_buffer`'s user-data; the `wl_buffer.Release`
/// dispatch flips it back to released. The flag is `Send + Sync + 'static` so
/// it satisfies the wayland-client bound, even though canoe itself is
/// single-threaded.
#[derive(Clone, Debug)]
pub struct ReleaseFlag(Arc<AtomicBool>);

impl ReleaseFlag {
    fn new_released() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    pub fn is_released(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn set_in_flight(&self) {
        self.0.store(false, Ordering::Release);
    }

    pub fn set_released(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[cfg(target_os = "linux")]
pub fn create(name: &str, size: i64) -> io::Result<File> {
    let memfd = memfd::MemfdOptions::default()
        .close_on_exec(true)
        .create(name)
        .map_err(io::Error::other)?;
    memfd.as_file().set_len(size as u64)?;
    Ok(memfd.into_file())
}

#[cfg(target_os = "openbsd")]
pub fn create(_name: &str, size: i64) -> io::Result<File> {
    use std::ffi::{c_char, c_int};
    use std::os::fd::FromRawFd;

    // shm_mkstemp is an OpenBSD extension and not exposed by the libc crate.
    extern "C" {
        fn shm_mkstemp(template: *mut c_char) -> c_int;
        fn shm_unlink(name: *const c_char) -> c_int;
    }

    // The last six characters of the template must be "XXXXXX"; shm_mkstemp
    // rewrites them in place with the generated unique suffix.
    let mut template: Vec<u8> = b"canoe-XXXXXX\0".to_vec();
    let fd = unsafe { shm_mkstemp(template.as_mut_ptr() as *mut c_char) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Drop the name from the shm namespace; the object stays alive while our
    // fd is open, so the buffer becomes effectively anonymous.
    unsafe { shm_unlink(template.as_ptr() as *const c_char) };

    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(size as u64)?;
    Ok(file)
}

#[cfg(not(any(target_os = "linux", target_os = "openbsd")))]
pub fn create(_name: &str, size: i64) -> io::Result<File> {
    let file = tempfile::tempfile()?;
    file.set_len(size as u64)?;
    Ok(file)
}

/// Double-buffered wl_shm pool used by surfaces (titlebars, shadows) that
/// frequently resize. Two slots live side-by-side in one shm file so the next
/// frame writes to memory the compositor isn't currently sampling — `prepare`
/// always returns the slot that wasn't returned last time. The wl_buffer for
/// the chosen slot is (re)created when its dimensions change; the underlying
/// fd/pool/mmap is reused.
///
/// Growing the pool would overlay memory the compositor still reads from any
/// in-flight buffer, so on growth we drop the old pool entirely and allocate a
/// fresh fd — the compositor's mapping of the old fd survives until it
/// releases the last buffer carved from it. A 50% headroom on each grow keeps
/// allocations rare across a resize drag.
pub struct ShmPool {
    name: &'static str,
    memfile: Option<File>,
    mmap: Option<MmapMut>,
    pool: Option<wl_shm_pool::WlShmPool>,
    /// Bytes available per slot. Slot N lives at offset `slot_capacity * N`.
    slot_capacity: i32,
    slots: [Slot; 2],
    /// Index returned by the previous `prepare()` call; the next call rotates
    /// to `1 - last_prepared` so the slot the compositor still reads is left
    /// untouched.
    last_prepared: usize,
    /// Cumulative count of `allocate_fresh()` calls; surfaces compare before
    /// and after `prepare()` to see whether they hit the slow path.
    allocate_fresh_count: u32,
}

struct Slot {
    buffer: Option<wl_buffer::WlBuffer>,
    width: i32,
    height: i32,
    stride: i32,
    /// Shared with the `wl_buffer`'s user-data. `is_released() == true` means
    /// the compositor has released the buffer and the slot's bytes are safe
    /// to rewrite. A fresh `Slot` starts released so the first `prepare()`
    /// after construction can use it without waiting.
    release_flag: ReleaseFlag,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            buffer: None,
            width: 0,
            height: 0,
            stride: 0,
            release_flag: ReleaseFlag::new_released(),
        }
    }
}

impl ShmPool {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            memfile: None,
            mmap: None,
            pool: None,
            slot_capacity: 0,
            slots: [Slot::default(), Slot::default()],
            last_prepared: 0,
            allocate_fresh_count: 0,
        }
    }

    /// How many times this pool has hit the `allocate_fresh()` slow path.
    /// Callers can sample before/after `prepare()` to detect whether the
    /// frame got a brand-new shm fd (and the full `set_len` / `mmap` cost).
    pub fn allocate_fresh_count(&self) -> u32 {
        self.allocate_fresh_count
    }

    /// Prepare a slot for writing a buffer of the given dimensions. Returns
    /// the slot index. Caller writes pixels via [`Self::slot_bytes_mut`],
    /// attaches via [`Self::current_buffer`], then calls
    /// [`Self::mark_attached`] after the surface commit so the slot stays
    /// off-limits until its `wl_buffer.Release` event arrives.
    pub fn prepare<D>(
        &mut self,
        width: i32,
        height: i32,
        stride: i32,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
    ) -> Option<usize>
    where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ReleaseFlag>,
    {
        if width <= 0 || height <= 0 || stride <= 0 {
            return None;
        }
        let buf_size = (stride as i64).checked_mul(height as i64)?;
        if buf_size <= 0 || buf_size > i32::MAX as i64 {
            return None;
        }
        let buf_size = buf_size as i32;

        if self.pool.is_none() || buf_size > self.slot_capacity {
            self.allocate_fresh(buf_size, shm, qh)?;
        }

        // Prefer the rotation target, but if its buffer is still in flight
        // (compositor hasn't released it yet) try the other slot before
        // burning a fresh allocation. The wayland spec forbids writing to a
        // buffer the compositor is still sampling; ignoring this used to
        // produce visible tearing during fast resizes.
        let preferred = 1 - self.last_prepared;
        let slot_idx = if self.slots[preferred].release_flag.is_released() {
            preferred
        } else if self.slots[1 - preferred].release_flag.is_released() {
            1 - preferred
        } else {
            // Both slots in flight. Falling back to a fresh pool gives us new
            // memory the compositor isn't yet reading. With 50% headroom on
            // grow and double-buffered rotation, hitting this path during a
            // steady resize means the compositor is at least two frames
            // behind us.
            self.allocate_fresh(buf_size, shm, qh)?;
            0
        };

        let slot = &mut self.slots[slot_idx];
        if slot.buffer.is_none()
            || slot.width != width
            || slot.height != height
            || slot.stride != stride
        {
            if let Some(b) = slot.buffer.take() {
                b.destroy();
            }
            // The old wl_buffer (if any) is gone, so the old flag has no
            // meaning either. Start the new buffer with a fresh, "released"
            // flag and pass a clone as its user-data.
            slot.release_flag = ReleaseFlag::new_released();
            let offset = (slot_idx as i32).checked_mul(self.slot_capacity)?;
            let pool = self.pool.as_ref()?;
            let buffer = pool.create_buffer(
                offset,
                width,
                height,
                stride,
                wl_shm::Format::Argb8888,
                qh,
                slot.release_flag.clone(),
            );
            slot.buffer = Some(buffer);
            slot.width = width;
            slot.height = height;
            slot.stride = stride;
        }

        self.last_prepared = slot_idx;
        Some(slot_idx)
    }

    /// Mark the most recently prepared slot as in-flight. Surfaces should
    /// call this immediately after `surface.commit()` so the next `prepare()`
    /// knows it can't reuse this slot until the compositor releases it.
    pub fn mark_attached(&self) {
        if let Some(slot) = self.slots.get(self.last_prepared) {
            slot.release_flag.set_in_flight();
        }
    }

    /// Mutable byte slice for the prepared slot's pixel data.
    pub fn slot_bytes_mut(&mut self, slot_idx: usize) -> Option<&mut [u8]> {
        let slot = self.slots.get(slot_idx)?;
        let bytes = (slot.stride as i64).checked_mul(slot.height as i64)?;
        if bytes <= 0 {
            return None;
        }
        let bytes = bytes as usize;
        let offset = (slot_idx as i64).checked_mul(self.slot_capacity as i64)? as usize;
        let mmap = self.mmap.as_mut()?;
        mmap.get_mut(offset..offset + bytes)
    }

    /// wl_buffer for the most recently prepared slot.
    pub fn current_buffer(&self) -> Option<&wl_buffer::WlBuffer> {
        self.slots.get(self.last_prepared)?.buffer.as_ref()
    }

    /// True once a wl_buffer is available for the current slot.
    pub fn is_ready(&self) -> bool {
        self.current_buffer().is_some()
    }

    /// Drop all client-side resources. Any in-flight buffer in the compositor
    /// stays valid until it sends a release event for it.
    pub fn destroy(&mut self) {
        for slot in &mut self.slots {
            if let Some(b) = slot.buffer.take() {
                b.destroy();
            }
            *slot = Slot::default();
        }
        if let Some(p) = self.pool.take() {
            p.destroy();
        }
        self.mmap.take();
        self.memfile.take();
        self.slot_capacity = 0;
        self.last_prepared = 0;
    }
}

impl Drop for ShmPool {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl ShmPool {
    fn allocate_fresh<D>(
        &mut self,
        slot_size: i32,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
    ) -> Option<()>
    where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ReleaseFlag>,
    {
        // 50% headroom keeps small-step resizes from hitting this path each
        // frame.
        let headroom = slot_size / 2;
        let new_slot_cap = slot_size.checked_add(headroom)?;
        let new_capacity = new_slot_cap.checked_mul(2)?;

        // Release client-side handles. The compositor's mapping of the old fd
        // persists until the last buffer carved from the old pool is released.
        for slot in &mut self.slots {
            if let Some(b) = slot.buffer.take() {
                b.destroy();
            }
            *slot = Slot::default();
        }
        if let Some(p) = self.pool.take() {
            p.destroy();
        }
        self.mmap.take();
        self.memfile.take();

        let memfile = create(self.name, new_capacity as i64).ok()?;
        let mut mmap = unsafe { MmapMut::map_mut(&memfile) }.ok()?;
        // Pre-fault every page so the first per-frame `fill(0)` doesn't pay
        // the page-fault tax. On OpenBSD shm, pages aren't physically backed
        // by `set_len`; they fault in on first write, which for a ~30 MB
        // titlebar adds ~40 ms to whichever frame happens to touch them
        // first. Doing it here folds that cost into the (rare) grow path
        // instead of a user-visible resize frame.
        mmap.fill(0);
        let pool = shm.create_pool(memfile.as_fd(), new_capacity, qh, ());
        self.memfile = Some(memfile);
        self.mmap = Some(mmap);
        self.pool = Some(pool);
        self.slot_capacity = new_slot_cap;
        // Neither slot is in flight after a fresh allocation; the next
        // `prepare()` will return slot 0.
        self.last_prepared = 1;
        self.allocate_fresh_count = self.allocate_fresh_count.wrapping_add(1);
        Some(())
    }
}
