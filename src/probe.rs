// SPDX-License-Identifier: GPL-3.0-or-later
//! `--probe`: prove the compositor side works, without a browser or a bus in the way.
//!
//! The equivalent of the `examples/test_*.rs` probes in `wlrix-compositor`, and the same idea:
//! advertising a global is not the same as being able to capture, so drive a whole exchange and
//! write the result somewhere it can be looked at. The difference is that this runs the
//! *backend's own* code -- [`crate::wayland::capture`], [`crate::wayland::shm`], the real event
//! loop -- rather than a parallel implementation that could be right while the daemon is wrong.
//!
//! Never claims the bus name, so it is safe to run while the real backend is up:
//!
//! ```text
//! WAYLAND_DISPLAY=wayland-1 ./xdg-desktop-portal-wlrix --probe
//! ```
//!
//! Note the compositor only fills capture frames when its backend next draws, so an idle
//! session produces nothing and a quiet probe proves nothing. Give it something moving --
//! `alacritty -e sh -c 'while true; do date; sleep 0.3; done'` -- or the timings lie.

use std::time::Duration;

use crate::{
    Portal,
    wayland::{
        capture::{Capture, Outcome, Purpose},
        shm::{Buffer, Memory, Region},
    },
};

/// How long to wait for a frame before giving up on it.
const TIMEOUT: Duration = Duration::from_secs(5);

/// One thing being probed, and the buffer it is capturing into.
struct Probe {
    label: &'static str,
    path: String,
    memory: Memory,
    /// Held, not read: the compositor is writing through this `wl_buffer`, and dropping it
    /// would destroy the buffer out from under the frame in flight.
    _buffer: Buffer,
    /// The dmabuf actually captured into, when one could be allocated. Held for the same reason
    /// as `_buffer`; its contents cannot be read from the CPU, so success is judged by the
    /// compositor answering `ready` rather than by counting pixels.
    dmabuf: Option<crate::wayland::dmabuf::Buffer>,
    width: u32,
    height: u32,
    done: bool,
}

pub fn run(
    event_loop: &mut calloop::EventLoop<'static, Portal>,
    portal: &mut Portal,
) -> Result<(), String> {
    let qh = portal
        .wayland
        .qh
        .clone()
        .ok_or("no queue handle; the connection did not finish setting up")?;

    println!("monitors:");
    for monitor in portal.wayland.inventory.ready_monitors() {
        println!(
            "  {:<20} {}x{} at {},{}  {}",
            monitor.name,
            monitor.size.0,
            monitor.size.1,
            monitor.position.0,
            monitor.position.1,
            monitor.description,
        );
    }
    println!("windows:");
    for window in portal.wayland.inventory.ready_windows() {
        println!(
            "  {:<20} {:?} ({})",
            window.identifier,
            window.label(),
            window.app_id,
        );
    }

    // One of each, which is what there is to prove: an output source and a toplevel source are
    // different code paths in the compositor and in `Wayland::*_source` here.
    let targets: Vec<(&'static str, String)> = portal
        .wayland
        .inventory
        .ready_monitors()
        .next()
        .map(|monitor| ("monitor", monitor.name.clone()))
        .into_iter()
        .chain(
            portal
                .wayland
                .inventory
                .ready_windows()
                .next()
                .map(|window| ("window", window.identifier.clone())),
        )
        .collect();

    if targets.is_empty() {
        return Err("nothing to capture: no monitors and no windows".into());
    }

    let copy = portal
        .wayland
        .copy
        .clone()
        .ok_or("no ext_image_copy_capture_manager_v1")?;
    for (label, id) in &targets {
        let source = match *label {
            "monitor" => {
                let monitor = portal
                    .wayland
                    .inventory
                    .monitor(id)
                    .ok_or("monitor went away")?;
                portal.wayland.monitor_source(&qh, monitor)
            }
            _ => {
                let window = portal
                    .wayland
                    .inventory
                    .window(id)
                    .ok_or("window went away")?;
                portal.wayland.window_source(&qh, window)
            }
        }
        .ok_or("no capture source manager")?;

        portal.wayland.captures.push(Capture::new(
            &copy,
            &qh,
            source,
            id.clone(),
            Purpose::Preview,
            false,
        ));
    }

    // The constraints arrive unprompted; nothing can be allocated until they do.
    let mut probes: Vec<Probe> = Vec::new();
    let deadline = std::time::Instant::now() + TIMEOUT;
    while probes.len() < targets.len() && std::time::Instant::now() < deadline {
        event_loop
            .dispatch(Duration::from_millis(100), portal)
            .map_err(|err| format!("dispatch: {err}"))?;

        for (index, (label, id)) in targets.iter().enumerate() {
            if probes.len() > index {
                continue;
            }
            let Some(capture) = portal
                .wayland
                .captures
                .iter_mut()
                .find(|capture| &capture.source_id == id)
            else {
                continue;
            };
            let Some(constraints) = capture.constraints else {
                continue;
            };
            println!(
                "{label}: constraints {}x{} {:?}",
                constraints.width, constraints.height, constraints.format
            );

            // Try dmabuf first, which is the whole point of the exercise: it is the only way to
            // find out whether the compositor can really render into a client-provided GPU
            // buffer, as opposed to merely offering to.
            let offer = capture.dmabuf.clone();
            let dmabuf = offer.as_ref().and_then(|offer| {
                println!(
                    "{label}: compositor offers dmabuf on device {} ({} formats)",
                    offer.device,
                    offer.formats.len()
                );
                portal.wayland.dmabuf_buffer(&qh, constraints, offer)
            });

            let shm = portal.wayland.shm.clone().ok_or("no wl_shm")?;
            let len = Buffer::size_for(constraints.width as i32, constraints.height as i32);
            let memory = Memory::new(len)?;
            let buffer = Buffer::new(&shm, &qh, Region::whole(memory.as_fd(), len), constraints);

            let capture = portal
                .wayland
                .captures
                .iter_mut()
                .find(|capture| &capture.source_id == id)
                .ok_or("the capture went away")?;
            match &dmabuf {
                Some(dma) => {
                    println!("{label}: capturing into a dmabuf");
                    capture.request(&qh, &dma.buffer, dma.width, dma.height);
                }
                None => {
                    println!("{label}: capturing into shared memory");
                    capture.request(&qh, &buffer.buffer, buffer.width, buffer.height);
                }
            }
            probes.push(Probe {
                label,
                path: format!("/tmp/wlrix-portal-probe-{label}.pnm"),
                memory,
                _buffer: buffer,
                dmabuf,
                width: constraints.width,
                height: constraints.height,
                done: false,
            });
        }
    }

    // Then wait for the frames themselves.
    let deadline = std::time::Instant::now() + TIMEOUT;
    while probes.iter().any(|probe| !probe.done) && std::time::Instant::now() < deadline {
        event_loop
            .dispatch(Duration::from_millis(100), portal)
            .map_err(|err| format!("dispatch: {err}"))?;

        for (probe, (_, id)) in probes.iter_mut().zip(&targets) {
            if probe.done {
                continue;
            }
            let Some(capture) = portal
                .wayland
                .captures
                .iter_mut()
                .find(|capture| &capture.source_id == id)
            else {
                continue;
            };
            match capture.take_outcome() {
                Some(Outcome::Ready) if probe.dmabuf.is_some() => {
                    // Nothing to count: the frame is in GPU memory this process cannot map
                    // cheaply. `ready` from the compositor *is* the result -- it means the
                    // buffer was accepted, imported and rendered into.
                    println!("{}: ready — rendered directly into the dmabuf", probe.label);
                    probe.done = true;
                }
                Some(Outcome::Ready) => {
                    let pixels = probe.memory.pixels();
                    write_pnm(&probe.path, probe.width, probe.height, pixels);
                    // Non-black pixels are the proof this is a capture rather than a buffer that
                    // was allocated and never written.
                    let lit = pixels
                        .chunks_exact(4)
                        .filter(|px| px[..3] != [0, 0, 0])
                        .count();
                    println!(
                        "{}: ready, {lit} non-black pixels -> {}",
                        probe.label, probe.path
                    );
                    probe.done = true;
                }
                Some(Outcome::Failed(reason)) => {
                    println!("{}: failed ({reason})", probe.label);
                    probe.done = true;
                }
                None if capture.stopped => {
                    println!("{}: the compositor stopped the session", probe.label);
                    probe.done = true;
                }
                None => {}
            }
        }
    }

    let timed_out: Vec<_> = probes
        .iter()
        .filter(|probe| !probe.done)
        .map(|probe| probe.label)
        .collect();
    // Dropped explicitly and before the report, so the buffers are destroyed while the
    // connection is still up rather than at process exit.
    drop(probes);
    portal.wayland.captures.clear();

    if timed_out.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "timed out waiting for: {} (is anything drawing? an idle compositor never repaints)",
            timed_out.join(", "),
        ))
    }
}

/// Dump a capture as binary PPM, which any image viewer opens.
fn write_pnm(path: &str, width: u32, height: u32, pixels: &[u8]) {
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    // xrgb8888 little-endian: B, G, R, X per pixel.
    for px in pixels.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    if let Err(err) = std::fs::write(path, out) {
        eprintln!("could not write {path}: {err}");
    }
}
