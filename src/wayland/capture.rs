// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading pixels out of the compositor: `ext-image-copy-capture-v1`.
//!
//! A *session* is opened against a source and lives as long as the thing being captured does.
//! The compositor announces what size and format of buffer it wants, and re-announces whenever
//! that changes -- a window resized, a monitor's mode changed, a window moved to a differently
//! scaled output. Against that session, a *frame* is requested one at a time: attach a buffer,
//! ask, and wait for `ready`.
//!
//! Two things follow from that shape, and both are easy to get wrong:
//!
//! - **Constraints can change under a live cast.** The session is not torn down; the buffers
//!   are re-made. A cast that ignored this would keep handing the compositor buffers of the old
//!   size, and get `failed` for every frame from then on.
//! - **`stopped` is final.** It means the source is gone -- window closed, monitor unplugged --
//!   and the session will never produce another frame. The stream on the other side has to be
//!   ended rather than left waiting, or the application sits watching a frozen picture.
//!
//! Frame *pacing* needs no work here. The compositor fills pending capture frames when its
//! backend next draws, so asking for the next frame as soon as the last one lands follows the
//! compositor's own repaint rate.

use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_shm::Format};
use wayland_protocols::ext::{
    image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
        ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

use super::inventory::SourceId;
use crate::Portal;

/// What the compositor says a buffer for this source must look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Constraints {
    pub width: u32,
    pub height: u32,
    pub format: Format,
}

/// How the last requested frame ended.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The buffer holds a frame.
    Ready,
    /// This frame did not happen. Not necessarily fatal -- the reason says whether to retry.
    Failed(String),
}

/// Why a capture exists, which decides what happens to each finished frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Feeding a PipeWire stream: every frame goes to the application.
    Cast,
    /// Feeding a picker tile: frames are occasional and downscaled.
    Preview,
}

/// One live capture session.
pub struct Capture {
    pub session: ExtImageCopyCaptureSessionV1,
    source: ExtImageCaptureSourceV1,
    /// Which inventory entry this captures, so a finished frame can be routed back.
    pub source_id: SourceId,
    pub purpose: Purpose,
    /// Constraints accumulating between `done` events.
    incoming: Incoming,
    /// The last complete set. `None` until the first `done`.
    pub constraints: Option<Constraints>,
    /// Set when constraints change, so the loop knows to re-make its buffers. Cleared by
    /// [`Capture::take_reconfigure`].
    reconfigured: bool,
    /// The source is gone. Terminal.
    pub stopped: bool,
    /// The frame currently outstanding, if any. One at a time: the protocol allows no more, and
    /// there would be nothing to gain -- the compositor produces them at its own repaint rate.
    frame: Option<ExtImageCopyCaptureFrameV1>,
    /// How that frame ended, waiting to be collected by the loop.
    outcome: Option<Outcome>,
    /// Frames asked for, against which the outcomes can be counted.
    requested: u64,
    /// What the compositor will let this source be rendered into directly, if anything.
    pub dmabuf: Option<DmabufOffer>,
}

/// What the compositor will let a capture be rendered straight into.
///
/// Settled at `done` alongside [`Constraints`], and kept separate from them because it is
/// optional: a compositor that offers no dmabuf still offers shm, and the capture works either
/// way. Only the *choice* of path depends on this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmabufOffer {
    /// The render node a buffer must be allocated on, as a `dev_t`.
    pub device: u64,
    /// `(fourcc, modifiers)` in the compositor's order of preference.
    pub formats: Vec<(u32, Vec<u64>)>,
}

/// Constraint fields as they arrive, before the `done` that makes them a set.
#[derive(Default, Clone)]
struct Incoming {
    width: u32,
    height: u32,
    format: Option<Format>,
    /// The render node a dmabuf must be allocated on, as a `dev_t`.
    dmabuf_device: Option<u64>,
    /// Offered `(fourcc, modifiers)`, in the compositor's order of preference.
    dmabuf: Vec<(u32, Vec<u64>)>,
}

impl Capture {
    /// Open a session against a source.
    ///
    /// Takes ownership of `source`: the session is only meaningful while the source exists, so
    /// tying their lifetimes together removes a way to get it wrong.
    pub fn new(
        copy: &ExtImageCopyCaptureManagerV1,
        qh: &QueueHandle<Portal>,
        source: ExtImageCaptureSourceV1,
        source_id: SourceId,
        purpose: Purpose,
        with_cursor: bool,
    ) -> Self {
        let options = if with_cursor {
            ext_image_copy_capture_manager_v1::Options::PaintCursors
        } else {
            ext_image_copy_capture_manager_v1::Options::empty()
        };
        let session = copy.create_session(&source, options, qh, ());
        Self {
            session,
            source,
            source_id,
            purpose,
            incoming: Incoming::default(),
            constraints: None,
            reconfigured: false,
            stopped: false,
            frame: None,
            outcome: None,
            requested: 0,
            dmabuf: None,
        }
    }

    /// Whether a frame is outstanding.
    pub fn busy(&self) -> bool {
        self.frame.is_some()
    }

    /// Ask for a frame into `buffer`.
    ///
    /// Does nothing if one is already outstanding or the session has stopped, so a caller
    /// driving this from a timer does not have to track either.
    /// Takes the `wl_buffer` and its size rather than a buffer type, because the two kinds --
    /// shared memory and dmabuf -- are allocated by different machinery and the protocol does
    /// not care which one this is.
    pub fn request(
        &mut self,
        qh: &QueueHandle<Portal>,
        buffer: &wayland_client::protocol::wl_buffer::WlBuffer,
        width: i32,
        height: i32,
    ) {
        if self.busy() || self.stopped {
            return;
        }
        let frame = self.session.create_frame(qh, ());
        frame.attach_buffer(buffer);
        // The whole buffer: this backend keeps no previous frame to diff against, so there is no
        // damage to report that would be narrower than everything.
        frame.damage_buffer(0, 0, width, height);
        frame.capture();
        self.outcome = None;
        self.frame = Some(frame);
        self.requested += 1;
        tracing::trace!(
            source = %self.source_id,
            requested = self.requested,
            "asked the compositor for a frame",
        );
    }

    /// Collect a finished frame, if one finished.
    ///
    /// Destroys the frame object as it goes: a frame is single-use, and holding it would leak a
    /// protocol object per captured frame.
    pub fn take_outcome(&mut self) -> Option<Outcome> {
        let outcome = self.outcome.take()?;
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        Some(outcome)
    }

    /// Whether the buffers need re-making, clearing the flag.
    pub fn take_reconfigure(&mut self) -> bool {
        std::mem::take(&mut self.reconfigured)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.session.destroy();
        self.source.destroy();
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for Portal {
    fn event(
        portal: &mut Self,
        session: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(capture) = portal
            .wayland
            .captures
            .iter_mut()
            .find(|capture| &capture.session == session)
        else {
            return;
        };
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                capture.incoming.width = width;
                capture.incoming.height = height;
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                // The first offered format wins: the compositor lists them in its own order of
                // preference, and both of the two it offers are 32-bit and equally convertible.
                if capture.incoming.format.is_none()
                    && let Ok(format) = format.into_result()
                {
                    capture.incoming.format = Some(format);
                }
            }
            // What the compositor will let a capture be drawn straight into. Recorded
            // alongside the shm formats rather than instead of them: a client that cannot
            // allocate on this device still has the shm path, and picking between them is the
            // caller's decision, not this module's.
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                // A `dev_t`, sent as native-endian bytes.
                match <[u8; 8]>::try_from(device.as_slice()) {
                    Ok(bytes) => capture.incoming.dmabuf_device = Some(u64::from_ne_bytes(bytes)),
                    Err(_) => tracing::warn!("the compositor sent a malformed dmabuf device id"),
                }
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                // Modifiers arrive as a flat array of native-endian `u64`s.
                let modifiers: Vec<u64> = modifiers
                    .chunks_exact(8)
                    .filter_map(|chunk| <[u8; 8]>::try_from(chunk).ok())
                    .map(u64::from_ne_bytes)
                    .collect();
                capture.incoming.dmabuf.push((format, modifiers));
            }
            // Everything since the last `done` is one consistent set.
            ext_image_copy_capture_session_v1::Event::Done => {
                let Some(format) = capture.incoming.format else {
                    tracing::warn!(
                        source = %capture.source_id,
                        "the compositor offered no shm format; cannot capture this source",
                    );
                    return;
                };
                let settled = Constraints {
                    width: capture.incoming.width,
                    height: capture.incoming.height,
                    format,
                };
                // Only a *change* is a reconfigure. The compositor re-sends the whole set on any
                // update, and re-allocating identical buffers mid-cast would drop frames for no
                // reason.
                if capture.constraints != Some(settled) {
                    tracing::debug!(source = %capture.source_id, ?settled, "buffer constraints");
                    capture.constraints = Some(settled);
                    capture.reconfigured = true;
                }
                capture.incoming.format = None;

                // The dmabuf offer is re-sent in full with every update, so it is rebuilt here
                // and the accumulator cleared -- otherwise a resize would append a second copy
                // of every format to the first.
                let offer = capture
                    .incoming
                    .dmabuf_device
                    .filter(|_| !capture.incoming.dmabuf.is_empty())
                    .map(|device| DmabufOffer {
                        device,
                        formats: std::mem::take(&mut capture.incoming.dmabuf),
                    });
                capture.incoming.dmabuf.clear();
                capture.incoming.dmabuf_device = None;
                if capture.dmabuf != offer {
                    if let Some(offer) = &offer {
                        tracing::debug!(
                            source = %capture.source_id,
                            device = offer.device,
                            formats = offer.formats.len(),
                            "the compositor offers dmabuf capture",
                        );
                    }
                    capture.dmabuf = offer;
                }
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                tracing::info!(source = %capture.source_id, "the compositor stopped the capture");
                capture.stopped = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for Portal {
    fn event(
        portal: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(capture) = portal
            .wayland
            .captures
            .iter_mut()
            .find(|capture| capture.frame.as_ref() == Some(frame))
        else {
            return;
        };
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => capture.outcome = Some(Outcome::Ready),
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                capture.outcome = Some(Outcome::Failed(format!("{reason:?}")));
            }
            // `Transform` is not acted on. wlRIX's compositor captures offscreen with
            // `Transform::Normal` on both paths -- it had a bug where it passed the output's own
            // transform and `grim` came back upside down under winit, fixed by always using
            // Normal. Honoring the event would reintroduce exactly that.
            _ => {}
        }
    }
}
