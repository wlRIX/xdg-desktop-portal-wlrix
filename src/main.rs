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

mod dbus;
mod logging;
mod portal;

use std::process::ExitCode;

/// What the command line asked for.
///
/// Deliberately almost nothing. This is bus-activated: it is started by D-Bus or by systemd,
/// never by a person, so there is no audience for options.
#[derive(Default)]
struct Args {
    /// Take the bus name from whoever already holds it. For development, when a previous run
    /// is still around.
    replace: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--replace" => args.replace = true,
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
    let mut event_loop = calloop::EventLoop::<portal::Portal>::try_new()
        .map_err(|err| format!("could not create the event loop: {err}"))?;
    let handle = event_loop.handle();

    // The bus thread reports in here. It is started before anything else so that a failure to
    // own the name is fatal at once: a portal backend nobody can reach is not worth running,
    // and exiting lets D-Bus activation try again later rather than leaving a silent process.
    let (_bus, channel) = dbus::spawn(args.replace)?;

    handle
        .insert_source(channel, |event, _, portal| {
            if let calloop::channel::Event::Msg(request) = event {
                portal.handle(request);
            }
        })
        .map_err(|err| format!("could not watch the D-Bus channel: {err}"))?;

    let mut portal = portal::Portal::default();

    tracing::info!("serving {}", dbus::BUS_NAME);
    event_loop
        .run(None, &mut portal, |_| {})
        .map_err(|err| format!("event loop failed: {err}"))
}
