// SPDX-License-Identifier: GPL-3.0-or-later
//! A pidfile, so `portal.toml` can be reloaded without hunting for the process.
//!
//! `SIGHUP` re-reads the config and announces what changed (see [`crate::signals`] and
//! [`crate::dbus::settings`]), which needs a pid, and a sibling process cannot read one from
//! the environment. So it goes in a well-known file under the per-user runtime directory, the
//! same arrangement `wlrix-compositor`, `wlrix-idle` and `wlrix-desktop` use, and the one
//! `wlrix-settings-daemon` looks for.
//!
//! This backend is bus-activated rather than started by the session, so the file may be written
//! well after login -- a settings app changing the scheme before anything has ever asked for a
//! screen share finds no pidfile and reports the portal as not running, which is true and
//! harmless: the next process to start reads the new value from the file anyway.
//!
//! The file is removed on a clean exit via the returned [`Guard`]. A crash leaves it stale; a
//! reader should treat "no such process" as "not running" rather than trusting the file blindly.

use std::path::PathBuf;

/// Named for the process, beside the compositor's and the desktop's.
const PID_NAME: &str = "xdg-desktop-portal-wlrix.pid";

/// `$XDG_RUNTIME_DIR` (owned by one user, cleaned up on logout), else the temp dir -- the same
/// rule every other wlRIX pidfile follows, so they sit together.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

/// Where the pidfile lives.
pub fn path() -> PathBuf {
    runtime_dir().join(PID_NAME)
}

/// Write this process's pid. Returns a guard that removes the file when dropped; failure to
/// write is reported and swallowed, since a missing pidfile is not worth refusing to start over
/// -- it only costs a scheme change its liveness until the next application starts.
pub fn write() -> Option<Guard> {
    let path = path();
    match create(&path).and_then(|mut file| {
        use std::io::Write;
        writeln!(file, "{}", std::process::id())
    }) {
        Ok(()) => Some(Guard { path }),
        Err(err) => {
            tracing::warn!("could not write {}: {err}", path.display());
            None
        }
    }
}

/// Create (or truncate) the pidfile. `O_NOFOLLOW` because the path is predictable and the
/// temp-dir fallback is world-writable: without it, anyone could leave a symlink there and have
/// this process truncate a file of their choosing.
fn create(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Removes the pidfile on drop, so a live pidfile means a live portal.
pub struct Guard {
    path: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            tracing::warn!("could not remove {}: {err}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pidfile_sits_beside_the_others() {
        // Hand-kept in step with `wlrix-settings-daemon`'s `Owner::pidfile`, which is what
        // looks for it. A change here that is not mirrored there would make a scheme change
        // silently stop reaching running applications.
        assert!(path().ends_with("xdg-desktop-portal-wlrix.pid"));
    }
}
