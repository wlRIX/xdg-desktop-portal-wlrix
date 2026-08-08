// SPDX-License-Identifier: GPL-3.0-or-later
//! One PipeWire stream, fed by one Wayland capture.
//!
//! ## The compositor renders into the buffer the consumer reads
//!
//! Both kinds of memory a stream can carry -- a dmabuf or a memfd -- are allocated *here*, and
//! the compositor is handed a `wl_buffer` naming that same memory. Nothing is copied in either
//! mode, and no pixel passes through this process at all.
//!
//! That is the third design this file has had, and the other two are worth knowing about:
//!
//! 1. **Capture into a PipeWire buffer, with no `process` callback.** Delivered exactly one
//!    frame and then stopped forever, with the stream still reporting `Streaming`. Diagnosed at
//!    the time as a deadlock between the compositor's repaint clock and PipeWire's graph cycle;
//!    it was not.
//! 2. **Capture into our own memory and copy into PipeWire's.** Decoupled the two clocks and
//!    worked, at 14 MB of `memcpy` per frame at 1440p.
//!
//! The real cause of (1) was the missing `process` hook below, not the coupling. With that
//! hooked, capturing straight into PipeWire's buffer is safe -- and it is *required* for
//! dmabuf, because this program has no renderer and so could not copy GPU memory even if it
//! wanted to. Fixing the hook let (2)'s copy go as well.
//!
//! ## Two formats are offered, and the consumer picks
//!
//! When a dmabuf can be allocated the stream offers two formats: dmabuf-with-modifier first,
//! then plain shared memory. Offering only the first is what a consumer that cannot import a
//! dmabuf meets as `no more output formats` -- PipeWire exhausts the list and the share dies
//! rather than quietly falling back.
//!
//! The modifier is a *single* mandatory value rather than a list with `DONT_FIXATE`, because
//! [`DmabufPlan::settle`] has already test-allocated a buffer and knows exactly which layout
//! the driver produces. That skips PipeWire's fixation round trip entirely.
//!
//! Which one was chosen is never announced; it is deduced in `param_changed` from whether the
//! agreed format carries a modifier. The buffer parameters depend on that answer -- data type,
//! and one block per plane -- so they are sent only once it is known.
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
    /// Which of the two offered formats the consumer chose. `None` until it has.
    mode: Option<Mode>,
    /// The `wl_buffer` for each PipeWire buffer, by `pw_buffer` pointer.
    ///
    /// Populated from `add_buffer` -- which is why it lives here rather than on [`Cast`]: that
    /// callback cannot reach the loop's state.
    buffers: HashMap<usize, Target>,
    /// The PipeWire buffer the compositor is currently rendering into.
    in_flight: Option<usize>,
    /// The size buffers are allocated at.
    ///
    /// Here rather than captured by the `add_buffer` closure because it changes: a shared
    /// window that is resized renegotiates, and PipeWire then takes every buffer back and asks
    /// for new ones. A closure holding the original size would allocate the old dimensions for
    /// the rest of the share.
    size: (u32, u32),
}

/// What a memfd cast needs to allocate its buffers. The shm counterpart of [`DmabufPlan`].
#[derive(Clone)]
struct ShmPlan {
    shm: wl_shm::WlShm,
    qh: wayland_client::QueueHandle<crate::Portal>,
    constraints: Constraints,
}

impl ShmPlan {
    fn allocate(&self, width: u32, height: u32) -> Option<Target> {
        let length = shm::Buffer::size_for(width as i32, height as i32);
        let memory = shm::Memory::new(length)
            .map_err(|err| tracing::warn!("could not allocate a capture buffer: {err}"))
            .ok()?;
        let buffer = shm::Buffer::new(
            &self.shm,
            &self.qh,
            shm::Region::whole(memory.as_fd(), length),
            Constraints {
                width,
                height,
                ..self.constraints
            },
        );
        Some(Target::Shm {
            _memory: memory,
            buffer,
        })
    }
}

/// One `spa_data` block, as PipeWire needs it described.
struct Block {
    kind: u32,
    fd: std::os::fd::RawFd,
    offset: u32,
    stride: u32,
    size: u32,
}

/// Whether a negotiated format names a DRM modifier, which is what makes it the dmabuf one.
fn format_has_modifier(param: &Pod) -> bool {
    let Ok((_, value)) =
        spa::pod::deserialize::PodDeserializer::deserialize_any_from(param.as_bytes())
    else {
        return false;
    };
    let spa::pod::Value::Object(object) = value else {
        return false;
    };
    object
        .properties
        .iter()
        .any(|property| property.key == spa::sys::SPA_FORMAT_VIDEO_modifier)
}

/// Which kind of memory the consumer agreed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Dmabuf,
    Shm,
}

/// The memory behind one PipeWire buffer, and the `wl_buffer` the compositor renders into.
///
/// Both variants are allocated by this program, because `ALLOC_BUFFERS` is set for both. That
/// is not a quirk of the dmabuf path: it is a connect-time flag, so once dmabuf needs it, shm
/// gets it too -- and the happy consequence is that the shm path stops copying as well. The
/// compositor renders into the memfd PipeWire hands the consumer, exactly as it renders into
/// the dmabuf.
enum Target {
    Dmabuf(crate::wayland::dmabuf::Buffer),
    Shm {
        /// Held: the `wl_buffer` and PipeWire's `spa_data` both name this file.
        _memory: shm::Memory,
        buffer: shm::Buffer,
    },
}

impl Target {
    /// The blocks PipeWire has to be told about: one per plane for a dmabuf, one for a memfd.
    fn describe(&self, height: u32) -> Vec<Block> {
        match self {
            Self::Dmabuf(buffer) => buffer
                .planes
                .iter()
                .map(|plane| Block {
                    kind: spa::sys::SPA_DATA_DmaBuf,
                    fd: std::os::fd::AsRawFd::as_raw_fd(&plane.fd),
                    offset: plane.offset,
                    stride: plane.stride,
                    size: plane.stride * height,
                })
                .collect(),
            Self::Shm { _memory, buffer } => vec![Block {
                kind: spa::sys::SPA_DATA_MemFd,
                fd: std::os::fd::AsRawFd::as_raw_fd(&_memory.as_fd()),
                offset: 0,
                stride: buffer.stride as u32,
                size: buffer.stride as u32 * height,
            }],
        }
    }

    fn wl(&self) -> (&wayland_client::protocol::wl_buffer::WlBuffer, i32, i32) {
        match self {
            Self::Dmabuf(buffer) => (&buffer.buffer, buffer.width, buffer.height),
            Self::Shm { buffer, .. } => (&buffer.buffer, buffer.width, buffer.height),
        }
    }
}

/// Everything a dmabuf cast needs to allocate its buffers.
///
/// Settled once, before the stream is connected, by test-allocating a buffer and seeing what
/// the driver chose. That is what lets the format below name a *single* modifier: PipeWire's
/// alternative is to offer a list with `DONT_FIXATE` and then negotiate one in a second round
/// trip, which is a great deal of machinery for a producer that only ever makes one thing.
#[derive(Clone)]
pub struct DmabufPlan {
    allocator: Rc<crate::wayland::dmabuf::Allocator>,
    global: wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: wayland_client::QueueHandle<crate::Portal>,
    fourcc: u32,
    /// The layout the driver picked, which every later allocation is pinned to.
    modifier: u64,
    /// How many `spa_data` blocks a buffer needs; one per plane.
    planes: u32,
}

impl DmabufPlan {
    /// Work out what can be allocated, by allocating one and looking at it.
    ///
    /// `None` means shared memory: no `linux-dmabuf`, no render node, no format in common, or a
    /// driver that would not produce a buffer. None of those is an error -- the shm path works.
    pub fn settle(
        wayland: &mut crate::wayland::Wayland,
        constraints: Constraints,
        offer: &crate::wayland::capture::DmabufOffer,
    ) -> Option<Self> {
        let qh = wayland.qh.clone()?;
        let probe = wayland.dmabuf_buffer(&qh, constraints, offer)?;
        let plan = Self {
            allocator: wayland.allocator.clone()?,
            global: wayland.dmabuf.clone()?,
            qh,
            fourcc: crate::wayland::pick_format(offer)?.0,
            modifier: probe.modifier,
            planes: probe.planes.len() as u32,
        };
        tracing::info!(
            modifier = format!("{:#x}", plan.modifier),
            planes = plan.planes,
            "dmabuf capture negotiated",
        );
        Some(plan)
    }

    fn allocate(&self, width: u32, height: u32) -> Option<crate::wayland::dmabuf::Buffer> {
        self.allocator
            .allocate(
                &self.global,
                &self.qh,
                width,
                height,
                self.fourcc,
                &[self.modifier],
            )
            .map_err(|err| tracing::warn!("could not allocate a capture buffer: {err}"))
            .ok()
    }
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
    /// Set when this cast renders straight into GPU buffers. `None` is the shm path.
    plan: Option<DmabufPlan>,
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
        plan: Option<DmabufPlan>,
    ) -> Result<Self, String> {
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

        let shared = Rc::new(RefCell::new(Shared {
            size: (constraints.width, constraints.height),
            ..Shared::default()
        }));

        let listener = {
            let state_shared = Rc::clone(&shared);
            let remove_shared = Rc::clone(&shared);
            let process_shared = Rc::clone(&shared);
            let add_shared = Rc::clone(&shared);
            let add_plan = plan.clone();
            let add_shm = ShmPlan {
                shm: shm_global.clone(),
                qh: qh.clone(),
                constraints,
            };
            let format_shared = Rc::clone(&shared);
            let format_plan = plan.clone();
            stream
                .add_local_listener_with_user_data(())
                // Only record the state here. Calling back into the stream from this callback --
                // `is_driving()` was tried -- crashes the process: it fires while `connect` is
                // still setting the stream up, before there is anything to ask.
                .state_changed(move |_, _, old, new| {
                    tracing::debug!(?old, ?new, "PipeWire stream state");
                    state_shared.borrow_mut().state = Some(new);
                })
                // Where the memory actually comes from. PipeWire was told the application
                // allocates (`ALLOC_BUFFERS`), for both kinds: a gbm buffer object or a memfd,
                // depending on which format the consumer settled on, plus a `wl_buffer` naming
                // it for the compositor and the descriptors the consumer will read.
                .add_buffer(move |_, _, pw_buffer| {
                    let (mode, (width, height)) = {
                        let shared = add_shared.borrow();
                        (shared.mode, shared.size)
                    };
                    let Some(mode) = mode else {
                        tracing::error!("PipeWire asked for a buffer before agreeing a format");
                        return;
                    };

                    let target = match mode {
                        Mode::Dmabuf => add_plan
                            .as_ref()
                            .and_then(|plan| plan.allocate(width, height))
                            .map(Target::Dmabuf),
                        Mode::Shm => add_shm.allocate(width, height),
                    };
                    let Some(target) = target else {
                        return;
                    };

                    // SAFETY: PipeWire hands this callback a buffer whose `spa_data` array it
                    // has just allocated, sized to the `blocks` this stream asked for. Each
                    // block is filled once, here, before anything reads it.
                    unsafe {
                        let datas = (*(*pw_buffer).buffer).datas;
                        let blocks = (*(*pw_buffer).buffer).n_datas as usize;
                        let planes = target.describe(height);
                        if blocks < planes.len() {
                            tracing::error!(
                                blocks,
                                planes = planes.len(),
                                "PipeWire allocated fewer blocks than the buffer has planes",
                            );
                            return;
                        }
                        for (index, plane) in planes.iter().enumerate() {
                            let data = datas.add(index);
                            (*data).type_ = plane.kind;
                            (*data).flags = 0;
                            (*data).fd = plane.fd as i64;
                            (*data).mapoffset = 0;
                            (*data).maxsize = plane.size;
                            // Null for both: nothing here maps the memory. The compositor
                            // writes through the `wl_buffer` and the consumer maps the fd
                            // itself, so a non-null pointer would only invite someone to read
                            // memory this process never mapped.
                            (*data).data = std::ptr::null_mut();
                            (*(*data).chunk).offset = plane.offset;
                            (*(*data).chunk).stride = plane.stride as i32;
                            (*(*data).chunk).size = plane.size;
                        }
                    }
                    add_shared
                        .borrow_mut()
                        .buffers
                        .insert(pw_buffer as usize, target);
                })
                .remove_buffer(move |_, _, buffer| {
                    // PipeWire frees the buffer as soon as this returns, and it takes all of
                    // them back whenever the last consumer disconnects. A pointer held here
                    // would dangle, and queueing it later would write through freed memory.
                    let mut shared = remove_shared.borrow_mut();
                    let key = buffer as usize;
                    if shared.available == Some(key) {
                        shared.available = None;
                    }
                    if shared.in_flight == Some(key) {
                        shared.in_flight = None;
                    }
                    // Dropping the target destroys the `wl_buffer` and frees the memory.
                    shared.buffers.remove(&key);
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
                // Where the choice between the two offered formats is discovered.
                //
                // PipeWire does not say "I picked the dmabuf one"; it hands back the format it
                // settled on, and the presence of a modifier property is what distinguishes
                // them. The buffer parameters depend on that answer -- data type, and one block
                // per plane -- so they are announced here rather than at connect, once there is
                // something to announce them for.
                .param_changed(move |stream, _, id, param| {
                    if id != spa::param::ParamType::Format.as_raw() {
                        return;
                    }
                    let Some(param) = param else {
                        // A null format means the negotiation was torn down; nothing to do
                        // until the next one.
                        format_shared.borrow_mut().mode = None;
                        return;
                    };

                    let mode = if format_has_modifier(param) {
                        Mode::Dmabuf
                    } else {
                        Mode::Shm
                    };
                    let (width, height) = {
                        let mut shared = format_shared.borrow_mut();
                        shared.mode = Some(mode);
                        shared.size
                    };
                    tracing::info!(?mode, width, height, "PipeWire agreed a format");

                    let planes = match mode {
                        Mode::Dmabuf => format_plan.as_ref().map_or(1, |plan| plan.planes),
                        Mode::Shm => 1,
                    };
                    let Ok(bytes) = buffers_param(width, height, mode, planes) else {
                        return;
                    };
                    let Some(pod) = Pod::from_bytes(&bytes) else {
                        return;
                    };
                    if let Err(err) = stream.update_params(&mut [pod]) {
                        tracing::error!("could not announce buffer parameters: {err}");
                    }
                })
                .register()
                .map_err(|err| format!("could not register the stream listener: {err}"))?
        };

        let mut offered = format_params(constraints, plan.as_ref())?;
        let mut params: Vec<&Pod> = offered
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
                // `ALLOC_BUFFERS` always, even when the consumer ends up choosing shared
                // memory. It is a *connect-time* flag and the format is not agreed until
                // afterwards, so it cannot be decided per mode: either this program allocates
                // both kinds or it cannot offer dmabuf at all.
                //
                // No `MAP_BUFFERS` with it: nothing here reads or writes the pixels in either
                // mode. The compositor renders through the `wl_buffer` and the consumer maps
                // the fd, so a mapping in this process would be set up per buffer and never
                // touched.
                pipewire::stream::StreamFlags::ALLOC_BUFFERS,
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
            plan,
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

    /// Where the next capture should be drawn, as `(wl_buffer, width, height)`.
    ///
    /// Always a buffer PipeWire handed over, whichever kind of memory was agreed, so a capture
    /// cannot start until the graph offers one. That is the coupling which deadlocked this
    /// program once before, and it is survivable only because `process` is hooked and keeps
    /// offering them.
    ///
    /// It is also what removed the last copy: the compositor renders into the memory the
    /// consumer reads, for shared memory exactly as for dmabuf.
    pub fn next_target(
        &mut self,
    ) -> Option<(wayland_client::protocol::wl_buffer::WlBuffer, i32, i32)> {
        let mut shared = self.shared.borrow_mut();
        if shared.in_flight.is_some() {
            return None;
        }
        let key = shared.available.take()?;
        let Some(target) = shared.buffers.get(&key) else {
            // Offered a buffer with no `wl_buffer` behind it -- `add_buffer` could not allocate.
            // Give it straight back rather than stranding it.
            drop(shared);
            self.give_back(key, 0);
            return None;
        };
        let (buffer, width, height) = target.wl();
        let target = (buffer.clone(), width, height);
        shared.in_flight = Some(key);
        Some(target)
    }

    /// Hand a PipeWire buffer back, with `size` bytes of content in it (0 for "nothing").
    fn give_back(&self, key: usize, size: u32) {
        let raw = key as *mut pipewire::sys::pw_buffer;
        // SAFETY: `key` is a buffer this stream dequeued and has not queued since;
        // `remove_buffer` clears both slots before PipeWire frees one, so reaching here means
        // it is still live.
        unsafe {
            let data = (*(*raw).buffer).datas;
            if !data.is_null() {
                (*(*data).chunk).size = size;
            }
            pipewire::sys::pw_stream_queue_buffer(self.stream.as_raw_ptr(), raw);
        }
    }

    /// A capture finished.
    ///
    /// With dmabuf the compositor has just drawn into PipeWire's own buffer, so the frame is
    /// already where it needs to be and only has to be handed on. With shared memory it sits in
    /// this cast's buffer until [`Cast::submit`] finds somewhere to copy it.
    pub fn frame_captured(&mut self) {
        let Some(key) = self.shared.borrow_mut().in_flight.take() else {
            return;
        };
        let stride = self.constraints.width * shm::BYTES_PER_PIXEL as u32;
        self.give_back(key, stride * self.constraints.height);
        self.count_frame();
    }

    /// A capture failed; give the buffer back empty rather than stranding it.
    pub fn frame_failed(&mut self) {
        if let Some(key) = self.shared.borrow_mut().in_flight.take() {
            self.give_back(key, 0);
        }
    }

    /// Count a delivered frame, and say so occasionally.
    fn count_frame(&mut self) {
        self.frames += 1;
        // The first frame, then every 60th: the first says the pipeline works at all, the rest
        // say it is still going and roughly how fast, without a line per frame burying
        // everything else.
        if self.frames == 1 || self.frames.is_multiple_of(60) {
            tracing::debug!(source = %self.source.id, frames = self.frames, "streaming");
        }
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
    pub fn reconfigure(&mut self, constraints: Constraints) -> Result<(), String> {
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

        // Nothing is re-allocated here. Announcing the new format makes PipeWire take every
        // buffer back and ask for fresh ones, and `add_buffer` allocates those at the size
        // recorded below -- so the buffers follow the format rather than being replaced twice.
        self.constraints = constraints;
        // What `add_buffer` will allocate when PipeWire asks again after this renegotiation.
        self.shared.borrow_mut().size = (constraints.width, constraints.height);

        let mut offered = format_params(constraints, self.plan.as_ref())?;
        let mut params: Vec<&Pod> = offered
            .iter_mut()
            .map(|bytes| Pod::from_bytes(bytes).expect("a pod this program just serialized"))
            .collect();
        self.stream
            .update_params(&mut params)
            .map_err(|err| format!("could not renegotiate the stream: {err}"))
    }
}

impl Drop for Cast {
    fn drop(&mut self) {
        let _ = self.stream.disconnect();
    }
}

/// Every format this stream will accept, best first.
///
/// **Two of them when dmabuf is possible, and that is the whole point.** Offering only the
/// dmabuf format leaves a consumer that cannot import one with nothing to fall back to: PipeWire
/// exhausts the list and the share dies with `no more output formats` rather than quietly using
/// shared memory. Offering both lets the consumer decide, and `param_changed` finds out which it
/// chose.
///
/// Order matters: PipeWire works down the list, so the dmabuf format goes first and shared
/// memory is what is left when nothing else fits.
fn format_params(
    constraints: Constraints,
    plan: Option<&DmabufPlan>,
) -> Result<Vec<Vec<u8>>, String> {
    let mut params = Vec::new();
    if let Some(plan) = plan {
        params.push(format_param(constraints, Some(plan))?);
    }
    params.push(format_param(constraints, None)?);
    Ok(params)
}

/// The exact video format this stream produces.
///
/// Built property by property rather than from a `VideoInfoRaw`: libspa provides
/// `From<AudioInfoRaw> for Vec<Property>` but has no video equivalent, so the object has to be
/// spelled out. The five properties below are the whole of a raw video format.
fn format_param(constraints: Constraints, plan: Option<&DmabufPlan>) -> Result<Vec<u8>, String> {
    let mut properties = vec![
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
    ];

    // The modifier is what makes this a dmabuf format rather than a description of some
    // pixels. **Mandatory**, deliberately: a consumer that cannot handle this layout must fail
    // negotiation rather than silently receive buffers it will misread.
    //
    // A single value, not a choice, so there is nothing to fixate. Offering a list would mean
    // `DONT_FIXATE` and a second negotiation round in which one is chosen by trial allocation
    // -- machinery this producer does not need, because it settled on one layout before the
    // stream was ever connected (see `DmabufPlan::settle`).
    if let Some(plan) = plan {
        properties.push(spa::pod::Property {
            key: spa::sys::SPA_FORMAT_VIDEO_modifier,
            flags: spa::pod::PropertyFlags::MANDATORY,
            value: spa::pod::Value::Long(plan.modifier as i64),
        });
    }

    serialize(spa::pod::Value::Object(spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties,
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
fn buffers_param(width: u32, height: u32, mode: Mode, planes: u32) -> Result<Vec<u8>, String> {
    let stride = width as i32 * shm::BYTES_PER_PIXEL as i32;
    let size = stride * height as i32;
    let data_type = match mode {
        Mode::Dmabuf => spa::sys::SPA_DATA_DmaBuf,
        Mode::Shm => spa::sys::SPA_DATA_MemFd,
    };

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
            property(
                spa::sys::SPA_PARAM_BUFFERS_blocks,
                // One `spa_data` per plane. A packed 32-bit RGB buffer has one, but a tiled
                // dmabuf can carry metadata planes as well, and a wrong count is a buffer
                // PipeWire cannot describe.
                spa::pod::Value::Int(planes as i32),
            ),
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
                        default: 1 << data_type,
                        flags: vec![1 << data_type],
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
