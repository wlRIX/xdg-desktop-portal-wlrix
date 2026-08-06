// SPDX-License-Identifier: GPL-3.0-or-later
//! One PipeWire stream, fed by one Wayland capture.
//!
//! ## The capture has a buffer of its own, and frames are copied into PipeWire's
//!
//! The tempting design is zero-copy: ask PipeWire for `SPA_DATA_MemFd` buffers, wrap each one's
//! fd in a `wl_shm_pool`, and have the compositor write its capture straight into the memory the
//! application will read. It saves 14 MB of `memcpy` per frame at 1440p.
//!
//! **It was built, and it deadlocks.** The two sides run on independent clocks: the compositor
//! produces a frame when it repaints, and PipeWire frees a buffer when the graph cycles. Making
//! the capture target a PipeWire buffer couples them -- a capture cannot start without a free
//! PipeWire buffer, and the graph will not advance without a queued frame. The observed symptom
//! is exactly one frame delivered and then permanent silence, with the stream still reporting
//! `Streaming`.
//!
//! So the capture owns one buffer of its own and always writes there, at whatever rate the
//! compositor repaints. Separately, whenever PipeWire offers a buffer, the newest captured frame
//! is copied into it. Neither side can stall the other, and a frame captured with no consumer
//! ready is simply overwritten by the next one.
//!
//! The copy is the price of that independence. The way to get rid of it is not to re-couple the
//! two, but dmabuf: the compositor would render into a buffer *we* own and hand PipeWire the
//! same dmabuf, with no clock shared between them. The compositor advertises no dmabuf capture
//! constraints yet, so that is future work.
//!
//! ## `process` is how a buffer becomes available, and it cannot be skipped
//!
//! For a producer, `process` means "the graph is ready for a frame". It is tempting to ignore
//! it, since frames appear when the *compositor* repaints rather than when the graph asks, and
//! to just call `pw_stream_dequeue_buffer` from the main loop whenever a capture is wanted.
//!
//! That does not work, and fails in a way that looks like something else entirely: the first
//! dequeue succeeds, one frame is delivered, and every dequeue after it returns null forever
//! while the stream still reports `Streaming`. Buffers are put back on the queue as part of the
//! graph cycle, and `process` is the notification that one is there. Without hooking it, nothing
//! ever asks again.
//!
//! So `process` dequeues, and the main loop takes the buffer from there. The stream is *not*
//! `RT_PROCESS`, so the callback runs on the loop thread rather than a realtime one, which is
//! what makes it safe to touch [`Shared`] from inside it.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use pipewire::{
    core::CoreRc,
    spa::{
        self,
        param::video::VideoFormat,
        pod::Pod,
        utils::{Direction, Fraction, Rectangle},
    },
    stream::{StreamListener, StreamRc, StreamState},
};
use wayland_client::protocol::wl_shm;

use crate::wayland::{capture::Constraints, inventory::SourceId, shm};

/// The framerate announced to the graph.
///
/// An upper bound, not a promise: frames arrive when the compositor repaints, which may be
/// slower. Consumers use it to size their own buffering, so naming a plausible ceiling matters
/// more than naming the truth, which varies by the second.
const MAX_FRAMERATE: u32 = 60;

/// How many buffers to ask PipeWire for.
///
/// Enough that one can be filling while the application reads another, few enough that a 1440p
/// stream is not tens of megabytes of standing allocation. This is the range every screencast
/// portal settles on.
const MIN_BUFFERS: i32 = 2;
const MAX_BUFFERS: i32 = 4;

/// What the PipeWire callbacks record for the loop to act on.
///
/// A callback fires inside a calloop dispatch and so cannot reach `Portal`; everything it learns
/// lands here and is read afterwards. See the [`super`] module docs.
#[derive(Default)]
struct Shared {
    state: Option<StreamState>,
    /// A PipeWire buffer `process` dequeued, waiting for a frame to be copied into it.
    ///
    /// The only place buffers come from. `pw_stream_dequeue_buffer` cannot simply be called
    /// from the main loop whenever one is wanted: it succeeds once and then returns null
    /// forever. Buffers return to the queue as part of a graph cycle, and `process` is the
    /// notification that one has.
    available: Option<usize>,
}

/// What is being shared, as the inventory described it when the share began.
///
/// Carried as a unit because it travels as one: it is settled at `Start`, outlives the
/// inventory entry it came from (a window can close mid-cast), and every field of it ends up in
/// the stream properties or the node name.
pub struct Source {
    pub id: SourceId,
    /// What to call the PipeWire node, so `pw-cli` and `wpctl` show something recognizable.
    pub label: String,
    pub is_monitor: bool,
    /// Position in the compositor's coordinate space; only meaningful for a monitor.
    pub position: (i32, i32),
}

/// One screen share: a PipeWire node, and the capture feeding it.
pub struct Cast {
    /// What is being shared. Its `id` matches a [`crate::wayland::capture::Capture`].
    pub source: Source,
    /// The size agreed with the graph. Frames of any other size cannot be sent.
    pub constraints: Constraints,

    stream: StreamRc,
    /// Registered callbacks. Dropping it unregisters them, so it is held even though unused.
    _listener: StreamListener<()>,
    shared: Rc<RefCell<Shared>>,
    /// The memory the compositor captures into, and the `wl_buffer` naming it.
    ///
    /// One, reused for every frame, and owned by this program rather than PipeWire -- which is
    /// the whole point: see the module docs.
    memory: shm::Memory,
    capture_buffer: shm::Buffer,
    /// Whether `memory` holds a frame that has not been passed on yet.
    ///
    /// A frame captured while no PipeWire buffer is free is not queued anywhere; it just sits
    /// here until the next capture overwrites it. A screen share should show what is on screen
    /// now, not work through a backlog.
    fresh: bool,
    /// Frames handed to the application, for the periodic liveness log.
    frames: u64,
}

impl Cast {
    /// Create the stream and announce the format.
    ///
    /// The format is exact rather than a range: the compositor has already said what size it
    /// will produce, and offering the graph a choice it cannot honour only invites a
    /// renegotiation that would have to be rejected.
    pub fn new(
        core: &CoreRc,
        shm_global: &wl_shm::WlShm,
        qh: &wayland_client::QueueHandle<crate::Portal>,
        source: Source,
        constraints: Constraints,
    ) -> Result<Self, String> {
        let length = shm::Buffer::size_for(constraints.width as i32, constraints.height as i32);
        let memory = shm::Memory::new(length)?;
        let capture_buffer = shm::Buffer::new(
            shm_global,
            qh,
            shm::Region::whole(memory.as_fd(), length),
            constraints,
        );

        let stream = StreamRc::new(
            core.clone(),
            &source.label,
            pipewire::properties::properties! {
                *pipewire::keys::MEDIA_TYPE => "Video",
                *pipewire::keys::MEDIA_CATEGORY => "Capture",
                *pipewire::keys::MEDIA_ROLE => "Screen",
                *pipewire::keys::NODE_NAME => "wlrix-screencast",
            },
        )
        .map_err(|err| format!("could not create the PipeWire stream: {err}"))?;

        let shared = Rc::new(RefCell::new(Shared::default()));

        let listener = {
            let state_shared = Rc::clone(&shared);
            let remove_shared = Rc::clone(&shared);
            let process_shared = Rc::clone(&shared);
            stream
                .add_local_listener_with_user_data(())
                // Only record the state here. Calling back into the stream from this callback --
                // `is_driving()` was tried -- crashes the process: it fires while `connect` is
                // still setting the stream up, before there is anything to ask.
                .state_changed(move |_, _, old, new| {
                    tracing::debug!(?old, ?new, "PipeWire stream state");
                    state_shared.borrow_mut().state = Some(new);
                })
                .remove_buffer(move |_, _, buffer| {
                    // PipeWire frees the buffer as soon as this returns, and it takes all of
                    // them back whenever the last consumer disconnects. A pointer held here
                    // would dangle, and queueing it later would write through freed memory.
                    let mut shared = remove_shared.borrow_mut();
                    if shared.available == Some(buffer as usize) {
                        shared.available = None;
                    }
                })
                .process(move |stream, _| {
                    let mut shared = process_shared.borrow_mut();
                    // One at a time: a second would be a buffer the consumer cannot have and
                    // nothing here is going to fill any faster.
                    if shared.available.is_some() {
                        return;
                    }
                    // SAFETY: called from `process` on the loop thread, which is exactly where
                    // dequeueing is defined to happen. Null means nothing is ready.
                    let raw =
                        unsafe { pipewire::sys::pw_stream_dequeue_buffer(stream.as_raw_ptr()) };
                    if !raw.is_null() {
                        shared.available = Some(raw as usize);
                    }
                })
                .register()
                .map_err(|err| format!("could not register the stream listener: {err}"))?
        };

        let mut params = [format_param(constraints)?, buffers_param(constraints)?];
        let mut params: Vec<&Pod> = params
            .iter_mut()
            .map(|bytes| Pod::from_bytes(bytes).expect("a pod this program just serialized"))
            .collect();

        stream
            .connect(
                Direction::Output,
                None,
                // `MAP_BUFFERS`, because the frame is copied in from the capture's own memory --
                // without it `spa_data.data` is null and there is nowhere to write.
                //
                // Deliberately **not** `DRIVER`, which was tried at length and does not work
                // here. A driver runs the graph cycle itself, and with it set `process` was
                // never called at all -- captures ran at full speed into a stream that accepted
                // nothing, and `pw_stream_trigger_process` succeeded while changing nothing.
                // Letting PipeWire drive the graph normally is what makes `process` fire, and
                // `process` is the only way a buffer is ever offered.
                //
                // Nor `ALLOC_BUFFERS`: PipeWire allocating them is what makes them shareable
                // with the consumer's process.
                pipewire::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .map_err(|err| format!("could not connect the stream: {err}"))?;

        tracing::debug!(source = %source.id, ?constraints, "PipeWire stream connected");

        Ok(Self {
            source,
            constraints,
            stream,
            _listener: listener,
            shared,
            memory,
            capture_buffer,
            fresh: false,
            frames: 0,
        })
    }

    /// Whether the graph has the stream running.
    pub fn streaming(&self) -> bool {
        matches!(self.shared.borrow().state, Some(StreamState::Streaming))
    }

    /// The node id, once PipeWire has assigned one.
    ///
    /// Not known at construction: `connect` only asks for the node, and the id arrives with the
    /// state change to `Paused`. This is what `Start` waits for -- reporting `SPA_ID_INVALID` to
    /// an application would have it open a remote onto nothing.
    pub fn node_id(&self) -> Option<u32> {
        match self.stream.node_id() {
            id if id == u32::MAX => None,
            id => Some(id),
        }
    }

    /// Whether the stream failed outright, so a caller waiting on it can stop waiting.
    pub fn errored(&self) -> bool {
        matches!(self.shared.borrow().state, Some(StreamState::Error(_)))
    }

    /// The properties `Start` reports alongside the node id.
    ///
    /// `position` is documented as monitor-only, so a window stream omits it rather than
    /// claiming (0, 0) -- an application laying out several shared sources would place the
    /// window at the origin and overlap whatever is really there.
    pub fn stream_properties(&self) -> HashMap<String, zbus::zvariant::OwnedValue> {
        use zbus::zvariant::{OwnedValue, Value};

        const SOURCE_TYPE_MONITOR: u32 = 1;
        const SOURCE_TYPE_WINDOW: u32 = 2;

        let mut properties = HashMap::new();
        let mut insert = |key: &str, value: Value<'_>| {
            if let Ok(value) = OwnedValue::try_from(value) {
                properties.insert(key.to_string(), value);
            }
        };
        insert(
            "size",
            Value::from((
                self.constraints.width as i32,
                self.constraints.height as i32,
            )),
        );
        insert(
            "source_type",
            Value::from(if self.source.is_monitor {
                SOURCE_TYPE_MONITOR
            } else {
                SOURCE_TYPE_WINDOW
            }),
        );
        if self.source.is_monitor {
            insert(
                "position",
                Value::from((self.source.position.0, self.source.position.1)),
            );
        }
        properties
    }

    /// The buffer the compositor captures into.
    ///
    /// Always the same one. The capture runs at the compositor's rate regardless of what
    /// PipeWire is doing -- that independence is the point, see the module docs.
    pub fn capture_buffer(&self) -> &shm::Buffer {
        &self.capture_buffer
    }

    /// Note that a fresh frame has landed in [`Cast::capture_buffer`].
    pub fn frame_captured(&mut self) {
        self.fresh = true;
    }

    /// Follow the source to a new size.
    ///
    /// A shared window resized, or a monitor changed mode. The compositor re-announces its
    /// buffer constraints and the stream has to be told, or every frame from here on is the
    /// wrong shape: the consumer would keep reading the old stride and show a sheared picture.
    ///
    /// Renegotiating rather than restarting matters. Tearing the stream down and building a new
    /// one would give the application a new node id, which it has no way to learn -- `Start`
    /// already answered -- so its stream would simply stop. Updating the parameters on the
    /// existing node keeps the id, and PipeWire tells the consumer the new format itself.
    pub fn reconfigure(
        &mut self,
        shm_global: &wl_shm::WlShm,
        qh: &wayland_client::QueueHandle<crate::Portal>,
        constraints: Constraints,
    ) -> Result<(), String> {
        if constraints == self.constraints {
            return Ok(());
        }
        tracing::info!(
            source = %self.source.id,
            from = ?self.constraints,
            to = ?constraints,
            "the source changed size; renegotiating the stream",
        );

        // Hand back anything already dequeued: it was allocated for the old format and must not
        // be filled at the new one. Size zero says "no data", which consumers skip.
        if let Some(key) = self.shared.borrow_mut().available.take() {
            // SAFETY: dequeued from this stream by `process` and not queued since.
            unsafe {
                let raw = key as *mut pipewire::sys::pw_buffer;
                (*(*(*raw).buffer).datas.offset(0))
                    .chunk
                    .as_mut()
                    .unwrap()
                    .size = 0;
                pipewire::sys::pw_stream_queue_buffer(self.stream.as_raw_ptr(), raw);
            }
        }

        // The capture's own buffer is sized to the source, so it is remade too.
        let length = shm::Buffer::size_for(constraints.width as i32, constraints.height as i32);
        let memory = shm::Memory::new(length)?;
        self.capture_buffer = shm::Buffer::new(
            shm_global,
            qh,
            shm::Region::whole(memory.as_fd(), length),
            constraints,
        );
        self.memory = memory;
        // Whatever was captured is the old size; do not send it at the new one.
        self.fresh = false;
        self.constraints = constraints;

        let mut params = [format_param(constraints)?, buffers_param(constraints)?];
        let mut params: Vec<&Pod> = params
            .iter_mut()
            .map(|bytes| Pod::from_bytes(bytes).expect("a pod this program just serialized"))
            .collect();
        self.stream
            .update_params(&mut params)
            .map_err(|err| format!("could not renegotiate the stream: {err}"))
    }

    /// Copy the newest captured frame into a PipeWire buffer, if there is one of each.
    ///
    /// Does nothing when no frame has been captured since the last time, or when the consumer
    /// has not given a buffer back. Neither is an error: the capture keeps running and the next
    /// frame simply overwrites the last.
    pub fn submit(&mut self) {
        if !self.fresh {
            return;
        }
        let Some(key) = self.shared.borrow_mut().available.take() else {
            return;
        };
        self.fresh = false;

        let stride = self.constraints.width as i32 * shm::BYTES_PER_PIXEL as i32;
        let size = stride as usize * self.constraints.height as usize;
        let raw = key as *mut pipewire::sys::pw_buffer;

        // SAFETY: `raw` was dequeued from this stream by `process` and not queued since --
        // `remove_buffer` clears it if PipeWire takes it back, so reaching here means it is
        // still live. `MAP_BUFFERS` is set, so `data` is mapped and at least `maxsize` long.
        unsafe {
            let data = (*(*raw).buffer).datas.offset(0);
            let destination = (*data).data as *mut u8;
            let capacity = (*data).maxsize as usize;
            let length = size.min(capacity);
            if destination.is_null() || length == 0 {
                // Nothing to write into. Give it back empty rather than stranding it.
                (*(*data).chunk).size = 0;
                pipewire::sys::pw_stream_queue_buffer(self.stream.as_raw_ptr(), raw);
                return;
            }
            std::ptr::copy_nonoverlapping(self.memory.pixels().as_ptr(), destination, length);
            (*(*data).chunk).offset = 0;
            (*(*data).chunk).stride = stride;
            (*(*data).chunk).size = length as u32;
            pipewire::sys::pw_stream_queue_buffer(self.stream.as_raw_ptr(), raw);
        }

        self.frames += 1;
        // The first frame, then every 60th: the first says the pipeline works at all, the rest
        // say it is still going and roughly how fast, without a line per frame burying
        // everything else.
        if self.frames == 1 || self.frames.is_multiple_of(60) {
            tracing::debug!(source = %self.source.id, frames = self.frames, "streaming");
        }
    }
}

impl Drop for Cast {
    fn drop(&mut self) {
        let _ = self.stream.disconnect();
    }
}

/// The exact video format this stream produces.
///
/// Built property by property rather than from a `VideoInfoRaw`: libspa provides
/// `From<AudioInfoRaw> for Vec<Property>` but has no video equivalent, so the object has to be
/// spelled out. The five properties below are the whole of a raw video format.
fn format_param(constraints: Constraints) -> Result<Vec<u8>, String> {
    serialize(spa::pod::Value::Object(spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties: vec![
            property(
                spa::sys::SPA_FORMAT_mediaType,
                spa::pod::Value::Id(spa::utils::Id(spa::sys::SPA_MEDIA_TYPE_video)),
            ),
            property(
                spa::sys::SPA_FORMAT_mediaSubtype,
                spa::pod::Value::Id(spa::utils::Id(spa::sys::SPA_MEDIA_SUBTYPE_raw)),
            ),
            property(
                spa::sys::SPA_FORMAT_VIDEO_format,
                spa::pod::Value::Id(spa::utils::Id(video_format(constraints.format).as_raw())),
            ),
            property(
                spa::sys::SPA_FORMAT_VIDEO_size,
                spa::pod::Value::Rectangle(Rectangle {
                    width: constraints.width,
                    height: constraints.height,
                }),
            ),
            property(
                spa::sys::SPA_FORMAT_VIDEO_framerate,
                // Denominator 1: whole frames per second.
                spa::pod::Value::Fraction(Fraction {
                    num: MAX_FRAMERATE,
                    denom: 1,
                }),
            ),
        ],
    }))
}

/// A pod object property with no flags, which is all of them here.
fn property(key: u32, value: spa::pod::Value) -> spa::pod::Property {
    spa::pod::Property {
        key,
        flags: spa::pod::PropertyFlags::empty(),
        value,
    }
}

/// What kind of buffers to allocate: memfds, one block, of exactly this image's size.
///
/// `dataType` is the load-bearing property. Without it PipeWire is free to hand out plain
/// process memory, which cannot be wrapped in a `wl_shm_pool` and would force the copy this
/// whole design exists to avoid.
fn buffers_param(constraints: Constraints) -> Result<Vec<u8>, String> {
    let stride = constraints.width as i32 * shm::BYTES_PER_PIXEL as i32;
    let size = stride * constraints.height as i32;

    serialize(spa::pod::Value::Object(spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            property(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Range {
                        default: MIN_BUFFERS,
                        min: MIN_BUFFERS,
                        max: MAX_BUFFERS,
                    },
                ))),
            ),
            property(spa::sys::SPA_PARAM_BUFFERS_blocks, spa::pod::Value::Int(1)),
            property(spa::sys::SPA_PARAM_BUFFERS_size, spa::pod::Value::Int(size)),
            property(
                spa::sys::SPA_PARAM_BUFFERS_stride,
                spa::pod::Value::Int(stride),
            ),
            property(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Flags {
                        default: 1 << spa::sys::SPA_DATA_MemFd,
                        flags: vec![1 << spa::sys::SPA_DATA_MemFd],
                    },
                ))),
            ),
        ],
    }))
}

fn serialize(value: spa::pod::Value) -> Result<Vec<u8>, String> {
    Ok(
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
            .map_err(|err| format!("could not serialize a SPA pod: {err}"))?
            .0
            .into_inner(),
    )
}

/// The SPA spelling of a `wl_shm` format.
///
/// Both are 32-bit little-endian with the same channel order, so this is a rename rather than a
/// conversion. SPA's `Bgrx`/`Bgra` name the bytes in memory order; `wl_shm`'s `Xrgb8888` names
/// the pixel as a little-endian word. They are the same layout.
fn video_format(format: wl_shm::Format) -> VideoFormat {
    match format {
        wl_shm::Format::Argb8888 => VideoFormat::BGRA,
        _ => VideoFormat::BGRx,
    }
}
