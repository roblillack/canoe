//! Anonymous shared-memory file for wl_shm pools.
//!
//! On Linux we use `memfd_create` to get an in-memory anonymous file. On other
//! Unix platforms (BSDs) `memfd_create` is not available, so we fall back to
//! an unlinked tempfile.

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

#[cfg(not(target_os = "linux"))]
pub fn create(_name: &str, size: i64) -> io::Result<File> {
    let file = tempfile::tempfile()?;
    file.set_len(size as u64)?;
    Ok(file)
}
