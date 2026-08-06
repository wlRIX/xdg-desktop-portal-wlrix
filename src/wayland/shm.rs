// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared-memory buffers for the compositor to write captures into.
//!
//! The important thing here is that a pool can be built over **any** file descriptor, not only
//! one this module allocated. That is what makes the PipeWire path free of a copy: a
//! `pw_buffer`'s `spa_data` for `SPA_DATA_MemFd` exposes an fd, and wrapping *that* in a
//! `wl_shm_pool` has the compositor write its capture straight into the buffer PipeWire is about
//! to hand the application. The alternative -- capture into our own memory, then memcpy into
//! theirs -- is an extra 14 MB per frame at 1440p, for nothing.
//!
//! The previews use the same machinery with a memfd of their own, which is why
//! [`Memory`] exists alongside [`Buffer`].
//!
//! (A dmabuf would avoid the readback as well, not just the copy. The compositor advertises no
//! dmabuf constraints yet -- `constraints_for` in its `image_capture.rs` sets `dma: None` -- so
//! shared memory is the only path available.)

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::{
    fs::{MemfdFlags, ftruncate, memfd_create},
    mm::{MapFlags, ProtFlags, mmap, munmap},
};
use wayland_client::{
    QueueHandle,
    protocol::{wl_buffer::WlBuffer, wl_shm::WlShm, wl_shm_pool::WlShmPool},
};

use super::capture::Constraints;
use crate::Portal;

/// Bytes per pixel for every format this backend deals in.
///
/// All four candidates -- `Xrgb8888`, `Argb8888` and their PipeWire spellings -- are 32-bit.
/// Named so the arithmetic below reads as something other than a stray 4.
pub const BYTES_PER_PIXEL: usize = 4;

/// An anonymous file, mapped, for buffers this backend allocates itself.
///
/// Used for previews. A capture destined for PipeWire uses PipeWire's own memory instead, so
/// this does not appear on that path at all.
pub struct Memory {
    fd: OwnedFd,
    map: *mut u8,
    len: usize,
}

// SAFETY: the mapping is owned by this struct and only reached through `&self`/`&mut self`; the
// raw pointer is what makes the compiler doubt it, not anything shared across threads.
unsafe impl Send for Memory {}

impl Memory {
    pub fn new(len: usize) -> Result<Self, String> {
        let fd = memfd_create(c"wlrix-portal", MemfdFlags::CLOEXEC)
            .map_err(|err| format!("memfd_create: {err}"))?;
        ftruncate(&fd, len as u64).map_err(|err| format!("ftruncate to {len}: {err}"))?;

        // SAFETY: a fresh memfd of exactly `len` bytes, mapped shared so the compositor's writes
        // through its own mapping of the same file are visible here.
        let map = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )
        }
        .map_err(|err| format!("mmap {len} bytes: {err}"))?;

        Ok(Self {
            fd,
            map: map.cast(),
            len,
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// What the compositor wrote.
    ///
    /// Only sound to read after a frame's `ready`: before that the compositor may be writing,
    /// and a torn preview is the best case.
    pub fn pixels(&self) -> &[u8] {
        // SAFETY: `map` covers `len` readable bytes for as long as `self` lives.
        unsafe { std::slice::from_raw_parts(self.map, self.len) }
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the region mapped in `new`, once.
        let _ = unsafe { munmap(self.map.cast(), self.len) };
    }
}

/// A `wl_buffer` and the pool behind it, destroyed together.
///
/// Kept as a pair because destroying them in the wrong order, or forgetting the pool, leaks a
/// file descriptor per frame -- which on a stream at 60fps is a matter of seconds before the
/// process runs out.
pub struct Buffer {
    pub buffer: WlBuffer,
    pool: WlShmPool,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
}

/// Where in a file an image sits.
///
/// Grouped rather than passed as three loose integers because the PipeWire case is the one that
/// gets this wrong: one memfd carries several buffers, so `pool_size` is the whole file while
/// `offset` is this buffer's place in it, and swapping them silently yields a pool too small for
/// the image it is asked to hold.
#[derive(Clone, Copy)]
pub struct Region<'a> {
    pub fd: BorrowedFd<'a>,
    /// The size of the whole file backing the pool.
    pub pool_size: usize,
    /// Where this image starts within it. Zero when the file holds one image.
    pub offset: i32,
}

impl<'a> Region<'a> {
    /// A file holding exactly one image, which is what the previews and the probe use.
    pub fn whole(fd: BorrowedFd<'a>, pool_size: usize) -> Self {
        Self {
            fd,
            pool_size,
            offset: 0,
        }
    }
}

impl Buffer {
    /// Wrap an existing fd -- ours or PipeWire's -- as a buffer the compositor can write to.
    ///
    /// `offset` is where the image starts within the file, which matters for PipeWire: it hands
    /// out one memfd carrying several buffers, each at its own `mapoffset`.
    pub fn new(
        shm: &WlShm,
        qh: &QueueHandle<Portal>,
        memory: Region<'_>,
        constraints: Constraints,
    ) -> Self {
        let (width, height) = (constraints.width as i32, constraints.height as i32);
        let stride = width * BYTES_PER_PIXEL as i32;
        let pool = shm.create_pool(memory.fd, memory.pool_size as i32, qh, ());
        let buffer = pool.create_buffer(
            memory.offset,
            width,
            height,
            stride,
            constraints.format,
            qh,
            (),
        );
        Self {
            buffer,
            pool,
            width,
            height,
            stride,
        }
    }

    /// How many bytes an image of this size needs.
    pub fn size_for(width: i32, height: i32) -> usize {
        width as usize * height as usize * BYTES_PER_PIXEL
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}
