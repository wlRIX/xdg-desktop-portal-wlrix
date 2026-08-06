// SPDX-License-Identifier: GPL-3.0-or-later
//! Stopping when asked to.
//!
//! `SIGTERM` (systemd stopping the unit) and `SIGINT` (Ctrl+C in a development run) are turned
//! into a calloop [`Ping`] fired from the handler -- an eventfd write, which is
//! async-signal-safe -- whose source on the loop ends the loop. The same shape `wlrix-idle`
//! uses, and for the same reason: the work of shutting down touches Wayland objects and child
//! processes, neither of which may be touched from signal context.
//!
//! Dying on the default disposition is not harmless here. Nothing this program owns is cleaned
//! up by the kernel: the picker is a **child process**, which would be left on screen asking
//! about a share whose answer now has nowhere to go, and the preview files would be left in the
//! runtime directory. Running the loop's exit path instead means every `Drop` runs -- captures
//! released, streams disconnected, the picker killed, the previews removed.
//!
//! No `SIGHUP` reload. `portal.toml` is read once at startup and holds only how fast the
//! picker's thumbnails refresh; there is nothing worth re-reading mid-session, and a screen
//! share is not something to reconfigure underneath.

use std::sync::OnceLock;

use calloop::ping::Ping;

/// The ping the handler fires. Set once, before the handlers are installed.
static QUIT: OnceLock<Ping> = OnceLock::new();

/// Install `SIGTERM`/`SIGINT` handlers that fire `quit`, so the loop can stop itself.
pub fn forward_to_loop(quit: Ping) {
    if QUIT.set(quit).is_err() {
        return;
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler does only async-signal-safe work -- firing the ping, which is an
        // eventfd write.
        unsafe { libc::signal(signal, handle as *const () as libc::sighandler_t) };
    }
}

/// Runs in signal context; may only do async-signal-safe work.
extern "C" fn handle(_signal: libc::c_int) {
    if let Some(quit) = QUIT.get() {
        quit.ping();
    }
}
