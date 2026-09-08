// SPDX-License-Identifier: GPL-3.0-or-later
//! Stopping when asked to, and re-reading the config when told.
//!
//! `SIGTERM` (systemd stopping the unit), `SIGINT` (Ctrl+C in a development run) and `SIGHUP`
//! (`wlrix-settings-daemon`, after it writes `portal.toml`) are turned into calloop [`Ping`]s
//! fired from the handler -- an eventfd write, which is async-signal-safe -- whose sources on
//! the loop do the actual work. The same shape `wlrix-idle` uses, and for the same reason: the
//! work touches Wayland objects, D-Bus connections and child processes, none of which may be
//! touched from signal context.
//!
//! Dying on the default disposition is not harmless here. Nothing this program owns is cleaned
//! up by the kernel: the picker is a **child process**, which would be left on screen asking
//! about a share whose answer now has nowhere to go, and the preview files would be left in the
//! runtime directory. Running the loop's exit path instead means every `Drop` runs -- captures
//! released, streams disconnected, the picker killed, the previews removed.
//!
//! ## Why there is a `SIGHUP` handler now
//!
//! There was not one, on the reasoning that a screen share is not something to reconfigure
//! underneath. That reasoning was about `[preview]` and `[capture]`, and it survives: a reload
//! does not touch a share already running, only the next one.
//!
//! What it missed is `[appearance]`. Since this backend grew a Settings interface it is where a
//! toolkit learns whether the session is light or dark, and a toolkit asks **once, at startup**
//! and then waits for `SettingChanged`. Without a signal to fire that on, a scheme change
//! reached only applications started afterwards -- which is exactly how Edge sat in light mode
//! under a dark scheme, with the right answer on the bus the whole time.

use std::sync::OnceLock;

use calloop::ping::Ping;

/// The pings the handler fires. Set once, before the handlers are installed.
static QUIT: OnceLock<Ping> = OnceLock::new();
static RELOAD: OnceLock<Ping> = OnceLock::new();

/// Install the handlers: `SIGTERM`/`SIGINT` fire `quit`, `SIGHUP` fires `reload`.
pub fn forward_to_loop(quit: Ping, reload: Ping) {
    if QUIT.set(quit).is_err() || RELOAD.set(reload).is_err() {
        return;
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler does only async-signal-safe work -- firing the ping, which is an
        // eventfd write.
        unsafe { libc::signal(signal, handle_quit as *const () as libc::sighandler_t) };
    }
    // SAFETY: as above.
    unsafe {
        libc::signal(
            libc::SIGHUP,
            handle_reload as *const () as libc::sighandler_t,
        )
    };
}

/// Runs in signal context; may only do async-signal-safe work.
extern "C" fn handle_quit(_signal: libc::c_int) {
    if let Some(quit) = QUIT.get() {
        quit.ping();
    }
}

/// Runs in signal context; may only do async-signal-safe work.
extern "C" fn handle_reload(_signal: libc::c_int) {
    if let Some(reload) = RELOAD.get() {
        reload.ping();
    }
}
