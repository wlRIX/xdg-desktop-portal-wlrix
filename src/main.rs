// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX desktop portal backend.
//!
//! Implements `org.freedesktop.impl.portal.ScreenCast`, which is what `xdg-desktop-portal`
//! hands an application's screen-sharing request to. A browser asks the *frontend* for a screen
//! to share; the frontend asks this program which one, and this program asks the user, captures
//! it, and publishes it as a PipeWire node the browser can consume.
//!
//! ## Why this exists rather than `xdg-desktop-portal-wlr`
//!
//! That backend speaks `wlr-screencopy`, which can capture **outputs only** -- "share a single
//! window" was never really possible, so it shells out to `slurp` for a region instead. wlRIX's
//! compositor implements `ext-image-capture-source-v1`, which has per-toplevel sources, so a
//! window picker can be a real window picker. The chooser-command configuration that comes with
//! the wlr backend goes away with it.
//!
//! ## Shape
//!
//! One calloop loop owns all state. D-Bus runs on a thread of its own -- zbus's blocking API,
//! no async runtime -- and reports in through a `calloop::channel`, the same shape `wlrix-idle`
//! uses for its inhibit interfaces and `wlrix-greeter` uses for greetd. The Wayland connection
//! and the PipeWire loop are both fd-driven and join the same loop, so nothing polls.

mod cast;
mod config;
mod dbus;
mod logging;
mod picker;
mod portal;
mod preview;
mod probe;
mod shot;
mod signals;
mod wayland;

pub use portal::Portal;

use std::{process::ExitCode, time::Duration};

/// How often to retry the frame pump. See where it is inserted for why it must exist.
const FRAME_TICK: Duration = Duration::from_millis(16);

/// What the command line asked for.
///
/// Deliberately almost nothing. This is bus-activated: it is started by D-Bus or by systemd,
/// never by a person, so there is no audience for options.
#[derive(Default)]
struct Args {
    /// Take the bus name from whoever already holds it. For development, when a previous run
    /// is still around.
    replace: bool,
    /// List what can be captured, capture one of each, and exit. A development tool -- see
    /// [`probe`].
    probe: bool,
    /// Parse a file, say whether it would be accepted as `portal.toml`, and exit.
    ///
    /// The exception to "no audience for options": this one is not for a person either. It is
    /// what `wlrix-settings-daemon` runs against a candidate file before renaming it into
    /// place, so a settings app cannot write a `portal.toml` this program would refuse.
    check_config: Option<std::path::PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    // `while let` rather than a `for`, so an option can take the argument after it. Only
    // `--check-config` does, and it is spelled the same way here as in every other wlRIX
    // component -- `wlrix-settings-daemon` runs all four with one calling convention.
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--replace" => args.replace = true,
            "--probe" => args.probe = true,
            "--check-config" => {
                args.check_config = Some(
                    argv.next()
                        .ok_or_else(|| "--check-config needs a path".to_string())?
                        .into(),
                );
            }
            "--version" | "-V" => {
                println!("xdg-desktop-portal-wlrix {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "xdg-desktop-portal-wlrix {}\n\n\
                     The wlRIX xdg-desktop-portal backend. Normally started by D-Bus\n\
                     activation, not by hand.\n\n\
                     Usage: xdg-desktop-portal-wlrix [options]\n\n\
                     Options:\n  \
                       --replace       take the bus name from whoever already holds it\n  \
                       --probe         list what can be shared, capture one of each to\n                                   /tmp, and exit. Never claims the bus name.\n  \
                       --check-config <path>  say whether that file would be accepted as\n                                   portal.toml, and exit\n  \
                       -V, --version   version, then exit\n  \
                       -h, --help      this message\n\n\
                     Logging: RUST_LOG, output to the journal. Prefer scoping it to this\n\
                     crate -- a bare RUST_LOG=debug turns on zbus's own message tracing,\n\
                     which buries everything worth reading:\n  \
                       RUST_LOG=xdg_desktop_portal_wlrix=debug",
                    env!("CARGO_PKG_VERSION"),
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("xdg-desktop-portal-wlrix: {err}");
            eprintln!("try --help");
            return ExitCode::FAILURE;
        }
    };

    // Before the logger: this answers a question about a file and starts nothing, so it must
    // work identically whether or not there is a journal to write to.
    if let Some(path) = &args.check_config {
        return match config::check(path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(why) => {
                eprintln!("{why}");
                ExitCode::FAILURE
            }
        };
    }

    logging::init();

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    let mut event_loop = calloop::EventLoop::<'static, portal::Portal>::try_new()
        .map_err(|err| format!("could not create the event loop: {err}"))?;
    let handle = event_loop.handle();
    let mut portal = portal::Portal::default();
    // Read before anything uses it. Without this the file is silently ignored -- which it was,
    // for as long as this line was missing.
    portal.config = config::Config::load();

    // The compositor first, and the bus name only if that worked. A backend that owned the name
    // without being able to capture anything would have every share fail against it, where
    // failing here leaves the name free for a backend that can.
    portal.handle = Some(handle.clone());
    portal.connection = Some(wayland::connect(&handle, &mut portal)?);
    tracing::info!(
        monitors = portal.wayland.inventory.ready_monitors().count(),
        windows = portal.wayland.inventory.ready_windows().count(),
        "connected to the compositor",
    );

    if args.probe {
        return probe::run(&mut event_loop, &mut portal);
    }

    // After the compositor, before the bus, for the same reason: a backend that cannot publish a
    // stream has nothing to offer an application, so it should not hold the name.
    portal.pipewire = Some(cast::connect(&handle)?);

    let (bus, channel) = dbus::spawn(args.replace)?;
    portal.bus = Some(bus.connection.clone());
    handle
        .insert_source(channel, |event, _, portal| {
            if let calloop::channel::Event::Msg(request) = event {
                portal.handle(request);
            }
        })
        .map_err(|err| format!("could not watch the D-Bus channel: {err}"))?;

    // A heartbeat, and it is load-bearing rather than a convenience.
    //
    // The frame pump is edge-driven: a finished capture wakes the loop, which queues the frame
    // and asks for the next. But if at that moment no PipeWire buffer is free -- the consumer
    // has not returned one yet -- no new capture is requested, and nothing else is going to wake
    // this process. The stream would deliver exactly one frame and then stall forever. This tick
    // is what retries.
    //
    // 16ms, so retrying costs at most a frame at 60Hz. When there is nothing to do the tick runs
    // `after_dispatch` over empty lists and goes back to sleep.
    handle
        .insert_source(
            calloop::timer::Timer::from_duration(FRAME_TICK),
            |_, _, _| calloop::timer::TimeoutAction::ToDuration(FRAME_TICK),
        )
        .map_err(|err| format!("could not start the frame timer: {err}"))?;

    // Stop cleanly when systemd stops the unit, so every `Drop` runs -- see `signals`.
    let (quit, quit_source) = calloop::ping::make_ping()
        .map_err(|err| format!("could not create the quit signal: {err}"))?;
    let stopping = std::rc::Rc::new(std::cell::Cell::new(false));
    let stop = std::rc::Rc::clone(&stopping);
    handle
        .insert_source(quit_source, move |_, _, _| {
            tracing::info!("stopping");
            stop.set(true);
        })
        .map_err(|err| format!("could not watch for signals: {err}"))?;
    signals::forward_to_loop(quit);

    tracing::info!("serving {}", dbus::BUS_NAME);
    let signal = event_loop.get_signal();
    event_loop
        // Finished captures are collected once per dispatch rather than from inside the protocol
        // handlers, which cannot touch the buffers they belong to while the queue borrows the
        // state. It is also the pattern the compositor itself uses for the other side of these
        // protocols -- reconcile once per loop iteration rather than hooking every site.
        .run(None, &mut portal, |portal| {
            portal.after_dispatch();
            if stopping.get() {
                portal.shut_down();
                signal.stop();
            }
        })
        .map_err(|err| format!("event loop failed: {err}"))
}
