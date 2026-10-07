//! One memfd segment per captured frame.
//!
//! WHY (owner 2026-10-07: "pass the raw frame byte buffers to the writer process
//! via shared memory, not a pipe", then "Allocate one memfd segment per frame on
//! demand and free it after the write"). CEF's paint buffer is copied ONCE, into
//! this segment; OCR reads it in place and the PNG writer process maps the same
//! pages through the passed fd. memfd is anonymous memory, so /dev/shm's size
//! (Docker's 64 MB default) never bounds it.

use anyhow::{bail, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// A mapped memfd. Dropping it unmaps and closes this process's handle; the
/// pages live until every holder (lane, writer) has closed its fd.
pub struct Segment {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

// The mapping is plain memory; ownership rules are the Vec-like ones below.
unsafe impl Send for Segment {}
unsafe impl Sync for Segment {}

impl Segment {
    /// A fresh zeroed segment of `len` bytes.
    pub fn create(len: usize) -> Result<Segment> {
        if len == 0 {
            bail!("memfd segment of 0 bytes");
        }
        let name = std::ffi::CString::new("snapbot-frame").unwrap();
        // SAFETY: plain syscalls; every return value is checked.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            bail!("memfd_create: {}", std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            bail!("ftruncate memfd to {len}: {}", std::io::Error::last_os_error());
        }
        Self::map(fd, len)
    }

    /// Map a segment received from another process (the writer's side).
    pub fn from_fd(fd: OwnedFd, len: usize) -> Result<Segment> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 || (st.st_size as usize) < len {
            bail!("received memfd is smaller than the announced {len} bytes");
        }
        Self::map(fd, len)
    }

    fn map(fd: OwnedFd, len: usize) -> Result<Segment> {
        // SAFETY: a shared mapping of a file we hold open, length checked above.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            bail!("mmap memfd of {len} bytes: {}", std::io::Error::last_os_error());
        }
        Ok(Segment { fd, ptr: ptr.cast(), len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is `len` bytes for the segment's lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Writable view. Only the capture side writes, before the segment is shared.
    #[allow(clippy::mut_from_ref)]
    pub fn as_mut_slice(&self) -> &mut [u8] {
        // SAFETY: as above; the single writer is the paint callback filling a
        // segment nothing else reads yet (capture completes before any reader).
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what map() mapped.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// One captured frame: BGRA rows, `stride` bytes apart, in its own segment.
pub struct Frame {
    pub seg: Segment,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}
