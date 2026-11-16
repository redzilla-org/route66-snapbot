//! memfd segments for frames.
//!
//! WHY: a screenshot's CDP surface capture is decoded once into a memfd
//! (cefhost.rs; route66 GH #4486), and its readers use it in place: OCR,
//! inspect and the PNG encode, which runs in-process in the lane (writer.rs),
//! all read the same mapping. The CDP document path decodes its PNG tiles
//! into a segment of its own. memfd is anonymous memory, so /dev/shm's size
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
        // SAFETY: as above; screenshot decoding fills the segment before either
        // OCR or the background writer reads it.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what map() mapped.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// One captured frame: BGRA rows, `stride` bytes apart, in a segment that is
/// never written again once a frame holds it (a decoded surface capture; see
/// cefhost::decode_shot).
pub struct Frame {
    pub seg: std::sync::Arc<Segment>,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}
