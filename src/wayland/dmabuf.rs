// SPDX-License-Identifier: GPL-3.0-or-later
//! Allocating GPU buffers for the compositor to render captures into.
//!
//! This is what makes a capture cost nothing on the CPU. With shared memory the compositor
//! draws offscreen, reads the result back across the bus, and this program copies it again into
//! PipeWire's buffer -- about 14 MB of traffic per frame at 1440p, most of it the readback.
//! With a dmabuf the compositor renders *into the buffer the application will read*, and no
//! pixel is ever touched by a CPU.
//!
//! ## Why this program is the one allocating
//!
//! Nobody else will. `ext-image-copy-capture` has the client provide the buffer, so not the
//! compositor. PipeWire's own buffer pool is memfd-backed; asking a stream for
//! `SPA_DATA_DmaBuf` means telling it the application allocates them. That leaves this program,
//! which therefore opens the render node the compositor named and allocates through gbm --
//! which is what every screencast portal ends up doing.
//!
//! ## The device is named by number, not by path
//!
//! The compositor sends a `dev_t`, because that is what identifies a DRM node unambiguously
//! across containers and symlinks. Turning it back into something that can be opened means
//! looking through `/dev/dri` for the node whose `rdev` matches. Guessing `renderD128` would be
//! wrong on this machine, which has two GPUs.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use gbm::{BufferObject, BufferObjectFlags};
use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_buffer::WlBuffer};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use crate::Portal;

/// A gbm device on the render node the compositor named.
pub struct Allocator {
    device: gbm::Device<Node>,
    /// The `dev_t` this was opened for, so a session naming a different GPU is not silently
    /// served from the wrong one.
    device_id: u64,
}

/// An opened DRM node. A newtype only because gbm wants something owning an fd.
pub struct Node(OwnedFd);

impl AsFd for Node {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// One plane of an allocated buffer, as PipeWire needs to describe it.
pub struct Plane {
    /// Kept alive for the life of the buffer: PipeWire hands this descriptor to the consuming
    /// process, and closing it here would pull the memory out from under a live stream.
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// A buffer the compositor can render into, and the `wl_buffer` naming it.
pub struct Buffer {
    pub buffer: WlBuffer,
    pub width: i32,
    pub height: i32,
    /// The layout the driver actually chose, which is what PipeWire has to be told.
    pub modifier: u64,
    /// One per plane. Almost always one for 32-bit RGB, but the format does not promise it.
    pub planes: Vec<Plane>,
    /// Held: dropping the buffer object frees the GPU memory the `wl_buffer` refers to.
    _bo: BufferObject<()>,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

impl Allocator {
    /// Open the render node with this `dev_t`.
    pub fn open(device_id: u64) -> Result<Self, String> {
        let path = node_path(device_id)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|err| format!("could not open {}: {err}", path.display()))?;
        let device = gbm::Device::new(Node(OwnedFd::from(file)))
            .map_err(|err| format!("{} is not usable through gbm: {err}", path.display()))?;
        tracing::debug!(path = %path.display(), device_id, "opened the render node");
        Ok(Self { device, device_id })
    }

    /// Whether this allocator is for the device a session named.
    pub fn serves(&self, device_id: u64) -> bool {
        self.device_id == device_id
    }

    /// Allocate a buffer and hand the compositor a `wl_buffer` for it.
    ///
    /// `modifiers` is what the compositor offered for this format. They are passed to gbm as a
    /// set so the driver picks the best layout it and the compositor both understand; a
    /// buffer allocated without modifiers is `INVALID`-layout and the compositor may refuse it.
    pub fn allocate(
        &self,
        dmabuf: &ZwpLinuxDmabufV1,
        qh: &QueueHandle<Portal>,
        width: u32,
        height: u32,
        fourcc: u32,
        modifiers: &[u64],
    ) -> Result<Buffer, String> {
        let format = gbm::Format::try_from(fourcc)
            .map_err(|_| format!("the compositor offered an unknown format {fourcc:#x}"))?;
        let modifiers: Vec<gbm::Modifier> = modifiers
            .iter()
            .copied()
            .map(gbm::Modifier::from)
            .filter(|modifier| *modifier != gbm::Modifier::Invalid)
            .collect();

        let bo = if modifiers.is_empty() {
            // No usable modifiers offered: fall back to a linear-ish driver choice. Rare, and
            // the compositor may still reject it, which is why this is not the normal path.
            self.device
                .create_buffer_object::<()>(
                    width,
                    height,
                    format,
                    BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR,
                )
                .map_err(|err| format!("could not allocate a {width}x{height} buffer: {err}"))?
        } else {
            self.device
                .create_buffer_object_with_modifiers2::<()>(
                    width,
                    height,
                    format,
                    modifiers.iter().copied(),
                    BufferObjectFlags::RENDERING,
                )
                .map_err(|err| format!("could not allocate a {width}x{height} buffer: {err}"))?
        };

        // Export every plane once. The descriptors are kept, not dropped after the protocol
        // call: `zwp_linux_buffer_params_v1.add` dups what it is given, but PipeWire is handed
        // the same fds later and needs them to still be open.
        let mut planes = Vec::new();
        let modifier: u64 = bo.modifier().into();
        for plane in 0..bo.plane_count() {
            planes.push(Plane {
                fd: bo
                    .fd_for_plane(plane as i32)
                    .map_err(|err| format!("could not export plane {plane}: {err}"))?,
                offset: bo.offset(plane as i32),
                stride: bo.stride_for_plane(plane as i32),
            });
        }

        // Describe every plane to the compositor, then ask for the buffer.
        let params = dmabuf.create_params(qh, ());
        for (index, plane) in planes.iter().enumerate() {
            params.add(
                plane.fd.as_fd(),
                index as u32,
                plane.offset,
                plane.stride,
                (modifier >> 32) as u32,
                (modifier & 0xffff_ffff) as u32,
            );
        }

        // `create_immed` rather than `create`: the reply form would need a round trip and an
        // event handler for a failure this program cannot do anything about anyway. A buffer
        // the compositor cannot use fails the frame instead, which is already handled.
        let buffer = params.create_immed(
            width as i32,
            height as i32,
            fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
            (),
        );
        params.destroy();

        Ok(Buffer {
            buffer,
            width: width as i32,
            height: height as i32,
            modifier,
            planes,
            _bo: bo,
        })
    }
}

/// Find the DRM node with this `dev_t`.
///
/// By `rdev` rather than by name: `/dev/dri` on this machine holds two cards and two render
/// nodes, and which number belongs to which GPU is not fixed across boots.
fn node_path(device_id: u64) -> Result<std::path::PathBuf, String> {
    let dir = std::path::Path::new("/dev/dri");
    let entries =
        std::fs::read_dir(dir).map_err(|err| format!("could not list {}: {err}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if std::os::unix::fs::MetadataExt::rdev(&metadata) == device_id {
            return Ok(path);
        }
    }
    Err(format!(
        "no DRM node with device id {device_id}; the compositor named a GPU this process \
         cannot see"
    ))
}

// The params object is used and destroyed within `allocate`; its events (`created`/`failed`)
// only arrive for the non-immediate form, which is not used.
impl Dispatch<ZwpLinuxBufferParamsV1, ()> for Portal {
    fn event(
        _portal: &mut Self,
        _params: &ZwpLinuxBufferParamsV1,
        _event: zwp_linux_buffer_params_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
