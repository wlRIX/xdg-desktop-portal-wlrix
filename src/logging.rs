// SPDX-License-Identifier: GPL-3.0-or-later
//! Where the backend's output goes.
//!
//! stderr, and only stderr. Unlike `wlrix-compositor`, which also writes a file because on a
//! TTY its own stderr is behind the screen it is drawing, this runs as a systemd user unit --
//! stderr is the journal, which is already the right place, already rotated, and already
//! interleaved with the compositor's and the session's.
//!
//! `RUST_LOG` picks the level. The default is `info`; `RUST_LOG=debug` is what to reach for
//! when a screen share produces no sources, since that is where the capture negotiation and the
//! picker exchange are logged.

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        // No timestamps: the journal stamps every line already, and a second clock in the text
        // just makes the lines longer.
        //
        // No color either. stderr here is the journal or a redirected file, neither of which
        // renders escape codes -- and they are not merely ugly. `tracing`'s colored output puts
        // them *between* a field's name and its `=`, so `grep 'frames='` silently matches
        // nothing on a log full of `frames=120`. That cost real time during development.
        .with(
            fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(std::io::stderr),
        )
        .init();
}
