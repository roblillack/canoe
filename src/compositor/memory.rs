//! Giving memory back to the system.
//!
//! glibc's allocator keeps memory that is freed for reuse rather than
//! handing it back. Two of its habits let a compositor that has been running
//! for a while hold on to far more than it uses:
//!
//! - A large block gets a mapping of its own, returned when it is freed, but
//!   once one is freed, glibc raises the size from which it does so to that
//!   block's. Buffers of that size then come from the heap, and stay with it
//!   after they are freed.
//! - Freed memory amid the heap goes back only when the heap is trimmed.
//!
//! So the size is fixed at startup, and the heap is trimmed after windows
//! close, when much is freed at once.

use std::time::Duration;

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use super::State;

/// Blocks from this size up get mappings of their own: glibc's default,
/// kept from growing.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const MMAP_THRESHOLD: nix::libc::c_int = 128 * 1024;

/// How long after a window closes the heap is trimmed. Windows closing
/// together are trimmed for once, after the last of them.
const TRIM_DELAY: Duration = Duration::from_secs(1);

/// Have large blocks returned to the system as soon as they are freed.
pub fn init() {
    // SAFETY: only changes a setting of the allocator, before any thread
    // could allocate concurrently.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        nix::libc::mallopt(nix::libc::M_MMAP_THRESHOLD, MMAP_THRESHOLD);
    }
}

impl State {
    /// Trim the heap shortly, once whatever a closed window held is freed.
    pub fn schedule_trim(&mut self) {
        if let Some(token) = self.trim_timer.take() {
            self.loop_handle.remove(token);
        }
        let timer = Timer::from_duration(TRIM_DELAY);
        self.trim_timer = self
            .loop_handle
            .insert_source(timer, |_, _, state| {
                state.trim_timer = None;
                trim();
                TimeoutAction::Drop
            })
            .ok();
    }
}

/// Hand the heap's free memory back to the system.
fn trim() {
    // SAFETY: malloc_trim only releases memory the allocator holds unused.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        nix::libc::malloc_trim(0);
    }
}
