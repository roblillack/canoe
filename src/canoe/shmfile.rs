//! Anonymous shared-memory file for wl_shm pools.
//!
//! Platform strategies:
//! * Linux: `memfd_create` (in-memory anonymous fd).
//! * OpenBSD: `shm_mkstemp` + immediate `shm_unlink` (POSIX shm object, kept
//!   alive by the open fd and not visible in any namespace).
//! * Other Unixes: unlinked tempfile via the `tempfile` crate.

use std::fs::File;
use std::io;

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
