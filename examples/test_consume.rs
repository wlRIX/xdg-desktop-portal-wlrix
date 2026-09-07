// SPDX-License-Identifier: GPL-3.0-or-later
//! Consume one of this backend's own streams the way OBS does, and say what arrived.
//!
//! GStreamer is not a sufficient test of the shm path. `pipewiresrc` wraps each `spa_data` in a
//! `GstMemory` and maps the **fd** itself, so it never touches `spa_data.data` and a stream whose
//! `data` pointer is wrong streams perfectly through it. OBS reads `data` directly -- it connects
//! with `PW_STREAM_FLAG_MAP_BUFFERS` (flags `0x5`, confirmed in the disassembly of
//! `linux-pipewire.so`) and hands the pointer straight to `gs_texture_create`. So does anything
//! else built on `pw_stream`'s mapping.
//!
//! This connects with exactly those flags and prints, for each buffer, what PipeWire filled in --
//! then actually *reads* the first and last byte of the frame, which is the thing a consumer does
//! and the thing that segfaults when the mapping is not what it claims to be.
//!
//! Usage: `cargo run --example test_consume -- <node-id> [frames]`, with `PIPEWIRE_RUNTIME_DIR`
//! pointing at the PipeWire the backend published to. Find the node with:
//!
//! ```text
//! pw-dump | jq -r '.[] | select(.info.props."node.name" == "wlrix-screencast") | .id'
//! ```
//!
//! Not part of the backend; a dev tool only.

use pipewire as pw;
use pw::{context::ContextRc, main_loop::MainLoopRc, properties::properties, stream::StreamFlags};

/// A pod object property with no flags.
fn prop(key: u32, value: libspa::pod::Value) -> libspa::pod::Property {
    libspa::pod::Property {
        key,
        flags: libspa::pod::PropertyFlags::empty(),
        value,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(node) = args.next().and_then(|arg| arg.parse::<u32>().ok()) else {
        eprintln!("usage: test_consume <node-id> [frames]");
        std::process::exit(2);
    };
    let wanted: u32 = args.next().and_then(|arg| arg.parse().ok()).unwrap_or(5);

    pw::init();
    let mainloop = MainLoopRc::new(None).expect("main loop");
    let context = ContextRc::new(&mainloop, None).expect("context");
    let core = context.connect_rc(None).expect("connect to PipeWire");

    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "wlrix-consume-probe",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .expect("stream");

    let seen = std::rc::Rc::new(std::cell::Cell::new(0u32));
    let quit = mainloop.clone();
    let counter = std::rc::Rc::clone(&seen);
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(|_, _, old, new| println!("state {old:?} -> {new:?}"))
        .param_changed(|_, _, id, _| {
            if id == libspa::param::ParamType::Format.as_raw() {
                println!("format negotiated");
            }
        })
        .process(move |stream, _| {
            // SAFETY: dequeued from `process` on the loop thread, which is where dequeueing is
            // defined to happen. The raw call rather than the wrapper because the whole point of
            // this probe is the `spa_data` fields, which the wrapper hides.
            let raw = unsafe { pw::sys::pw_stream_dequeue_buffer(stream.as_raw_ptr()) };
            if raw.is_null() {
                return;
            }
            let index = counter.get();
            counter.set(index + 1);

            unsafe {
                let spa = *(*raw).buffer;
                println!("buffer {index}: n_datas={} n_metas={}", spa.n_datas, spa.n_metas);
                for block in 0..spa.n_datas {
                    let data = &*spa.datas.add(block as usize);
                    let chunk = &*data.chunk;
                    println!(
                        "  data[{block}] type={} fd={} mapoffset={} maxsize={} data={:?} \
                         chunk(offset={} size={} stride={})",
                        data.type_,
                        data.fd,
                        data.mapoffset,
                        data.maxsize,
                        data.data,
                        chunk.offset,
                        chunk.size,
                        chunk.stride,
                    );
                    if data.data.is_null() {
                        println!("  data[{block}] POINTER IS NULL -- a consumer would crash here");
                        continue;
                    }

                    // Map the fd independently and compare. If this succeeds where `data.data`
                    // faults, the pointer PipeWire handed over is not a mapping in *this*
                    // process -- which is what a producer's own address looks like when it
                    // travels over the wire instead of being re-mapped on arrival.
                    if data.fd >= 0 {
                        let own = libc::mmap(
                            std::ptr::null_mut(),
                            data.maxsize as usize,
                            libc::PROT_READ,
                            libc::MAP_SHARED,
                            data.fd as i32,
                            data.mapoffset as i64,
                        );
                        if own == libc::MAP_FAILED {
                            println!(
                                "  data[{block}] own mmap failed: {}",
                                std::io::Error::last_os_error()
                            );
                        } else {
                            let first = own.cast::<u8>().read_volatile();
                            let last = own
                                .cast::<u8>()
                                .add(data.maxsize.saturating_sub(1) as usize)
                                .read_volatile();
                            println!(
                                "  data[{block}] own mmap at {own:?} reads fine: first={first} last={last}"
                            );
                            libc::munmap(own, data.maxsize as usize);
                        }
                    }
                    // What OBS does: read the whole frame out of `data`. Touching both ends is
                    // enough to catch a mapping that is absent or shorter than it claims.
                    let bytes = data.data.cast::<u8>();
                    // One page at a time, announced before it is touched, so the offset the
                    // mapping stops being readable at is in the log rather than inferred from a
                    // signal. A short mapping and a mapping that is not there at all fault at
                    // different offsets and with different signals (SIGBUS against a memfd that
                    // is shorter than its mapping, SIGSEGV against nothing at all).
                    let mut offset = 0usize;
                    while offset < data.maxsize as usize {
                        if offset.is_multiple_of(1 << 20) {
                            println!("  data[{block}] reading at {offset}...");
                        }
                        let _ = bytes.add(offset).read_volatile();
                        offset += 4096;
                    }
                    println!("  data[{block}] read the whole {} bytes", data.maxsize);
                }
                pw::sys::pw_stream_queue_buffer(stream.as_raw_ptr(), raw);
            }

            if counter.get() >= wanted {
                quit.quit();
            }
        })
        .register()
        .expect("listener");

    // A consumer has to offer something to negotiate against; connecting with no params is
    // refused outright ("error input enum formats"). BGRx at any size, which is what the
    // backend's shm path produces.
    let format = libspa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &libspa::pod::Value::Object(libspa::pod::Object {
            type_: libspa::sys::SPA_TYPE_OBJECT_Format,
            id: libspa::sys::SPA_PARAM_EnumFormat,
            properties: vec![
                prop(
                    libspa::sys::SPA_FORMAT_mediaType,
                    libspa::pod::Value::Id(libspa::utils::Id(libspa::sys::SPA_MEDIA_TYPE_video)),
                ),
                prop(
                    libspa::sys::SPA_FORMAT_mediaSubtype,
                    libspa::pod::Value::Id(libspa::utils::Id(libspa::sys::SPA_MEDIA_SUBTYPE_raw)),
                ),
                prop(
                    libspa::sys::SPA_FORMAT_VIDEO_format,
                    libspa::pod::Value::Id(libspa::utils::Id(
                        libspa::param::video::VideoFormat::BGRx.as_raw(),
                    )),
                ),
            ],
        }),
    )
    .expect("serialize the format")
    .0
    .into_inner();
    let pod = libspa::pod::Pod::from_bytes(&format).expect("a pod just serialized");

    stream
        .connect(
            libspa::utils::Direction::Input,
            Some(node),
            // Exactly OBS's flags. `MAP_BUFFERS` is the one under test: it is what makes
            // PipeWire fill in `spa_data.data`, and a producer that hands out memory PipeWire
            // cannot map leaves it null or wrong here rather than anywhere nearer the cause.
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )
        .expect("connect");

    mainloop.run();
    println!("saw {} buffer(s)", seen.get());
}
